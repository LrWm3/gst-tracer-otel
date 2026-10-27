#!/usr/bin/env python3
"""Check split GPU reports against real Pyroscope v2 compaction, without CUDA.

Builds the Rust integration fixture and starts a disposable loopback-only backend.
Two profiler processes submit the same service/second. A negative control retains
unique upload IDs but restores the old identical series/timestamps. Its data must
be lost during compaction while every fixed element/span/trace total survives.
"""
import argparse
import json
from pathlib import Path
import re
import subprocess
import time
import urllib.request
import uuid


ROOT = Path(__file__).resolve().parents[4]


def command(*args, **kwargs):
    return subprocess.check_output(args, text=True, **kwargs)


def request(url, data=None, headers=None):
    req = urllib.request.Request(url, data=data, headers=headers or {})
    with urllib.request.urlopen(req, timeout=15) as response:
        return response.read()


def eventually(check, seconds=90):
    deadline = time.monotonic() + seconds
    while True:
        try:
            return check()
        except (AssertionError, OSError) as error:
            if time.monotonic() >= deadline:
                raise RuntimeError(f'Timed out: {error}') from error
            time.sleep(1)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, default=ROOT / 'target/gpu-compaction')
    parser.add_argument('--test-binary', type=Path)
    args = parser.parse_args()
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=True)
    binary = args.test_binary
    if binary is None:
        build = command('cargo', 'test', '-p', 'gst-otel-tracer', '--features',
                        'gpu-profiles', '--test', 'gpu_profiles', '--no-run',
                        '--message-format=json', cwd=ROOT)
        binary, = [Path(row['executable']) for row in map(json.loads, build.splitlines())
                   if row.get('executable') and row.get('target', {}).get('name') == 'gpu_profiles']
    adapter = args.output / 'mock.so'
    subprocess.run(['cc', '-std=c11', '-shared', '-fPIC', '-pthread',
                    str(ROOT / 'tracer/otel/native/gpu_mock.c'), '-o', str(adapter)], check=True)
    service = 'mars-compaction-' + uuid.uuid4().hex
    second = int(time.time())
    samples = []
    captures = []
    for index in range(2):
        capture = args.output / f'instance-{index}'
        capture.mkdir(exist_ok=True)
        env = dict(GPU_TEST_MODE='split-reports', GPU_TEST_CAPTURE_DIR=str(capture),
                   GPU_TEST_SUBMISSION_SECOND=str(second), MARS_GPU_ADAPTER=str(adapter),
                   OTEL_SERVICE_NAME=service)
        result = subprocess.run([str(binary.resolve()), '--exact', 'gpu_child', '--ignored',
                                 '--nocapture'], env=env, capture_output=True, text=True)
        (capture / 'fixture.log').write_text(result.stdout + result.stderr)
        assert result.returncode == 0, result.stdout + result.stderr
        assert f'BUCKET {second}' in result.stdout
        rows = re.findall(r'SAMPLE element=(\w+) span=([0-9a-f]+) trace=([0-9a-f]+) nanos=(\d+)', result.stdout)
        assert len(rows) == 4
        samples.extend(dict(element=e, span=s, trace=t, nanos=int(n)) for e, s, t, n in rows)
        captures.extend(capture / f'{i}.pb.gz' for i in range(4))
    total = sum(s['nanos'] for s in samples)
    container = 'mars-gpu-compaction-' + uuid.uuid4().hex[:12]
    command('docker', 'run', '--rm', '-d', '--name', container, '--cpus', '2',
            '--memory', '1g', '-e', 'GOMAXPROCS=2', '-p', '127.0.0.1::4040',
            'grafana/pyroscope:2.4.0', '-architecture.storage=v2',
            '-segment-writer.segment-duration=1s', '-compaction-worker.job-poll-interval=1s',
            '-metastore.min-ready-duration=0s', '-segment-writer.min-ready-duration=0s')
    try:
        base = 'http://' + command('docker', 'port', container, '4040/tcp').strip()
        eventually(lambda: request(base + '/ready'))
        for capture in captures:
            for path in [capture, capture.with_name(capture.name.replace('.pb.gz', '.control.pb.gz'))]:
                request(base + '/push.v1.PusherService/Push', path.read_bytes(),
                        {'Content-Type': 'application/proto', 'Content-Encoding': 'gzip'})
            # Synchronous acknowledgements put successive reports in different
            # segment blocks, so the compactor actually merges separate reports.
        query = dict(start=(second - 1) * 1000, end=(second + 2) * 1000,
                     profileTypeID='gpu_kernel:gpu:nanoseconds:gpu:nanoseconds', maxNodes=10000)

        def profile(control=False, **selection):
            body = {**query, 'labelSelector': '{service_name="' + service + ('-control' if control else '') + '"}', **selection}
            return json.loads(request(base + '/querier.v1.QuerierService/SelectMergeStacktraces',
                                      json.dumps(body).encode(), {'Content-Type': 'application/json'}))

        def verify():
            merged = profile()
            assert int(merged.get('flamegraph', {}).get('total', 0)) == total, merged
            for sample in samples:
                flame = profile(spanSelector=[sample['span']])['flamegraph']
                assert int(flame['total']) == sample['nanos'], (sample, flame)
                owners = [name for name in flame['names'] if name.startswith('[element]')]
                assert len(owners) == 1 and f"{sample['element']} [identity]" in owners[0], owners
                assert 'same_kernel' in flame['names']
                assert len(flame['levels']) == 3
            for trace in {s['trace'] for s in samples}:
                expected = sum(s['nanos'] for s in samples if s['trace'] == trace)
                assert int(profile(traceIdSelector=[trace])['flamegraph']['total']) == expected
            assert int(profile(spanSelector=['ffffffffffffffff']).get('flamegraph', {}).get('total', 0)) == 0
            return merged

        def full_control():
            control = profile(control=True)
            assert int(control.get('flamegraph', {}).get('total', 0)) == total, control
            return control

        before_control = eventually(full_control)
        before = eventually(verify)

        def compacted():
            metrics = request(base + '/metrics').decode()
            completed = sum(float(line.rsplit(' ', 1)[1]) for line in metrics.splitlines()
                            if line.startswith('pyroscope_compaction_worker_jobs_completed_total{')
                            and 'status="success"' in line)
            assert completed > 0, 'backend has not completed compaction'
            control = profile(control=True)
            actual = int(control.get('flamegraph', {}).get('total', 0))
            assert actual == samples[0]['nanos'], f'negative control has not fully deduplicated: {actual}/{total}'
            return control

        # First-level v2 batches age out after 36 seconds. Seeing the control
        # lose data proves we exercised deduplication, not just HTTP acceptance.
        after_control = eventually(compacted, seconds=120)
        after = verify()
        (args.output / 'metrics.txt').write_bytes(request(base + '/metrics'))
        result = dict(passed=True, service=service, submission_second=second, expected_ns=total,
                      samples=samples, before=before, after=after,
                      before_control=before_control, after_control=after_control)
        (args.output / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
        print(f'PASS: {len(captures)} reports, two instances, {total} ns retained after compaction; every element/span/trace total agrees')
        print(f"CONTROL: old timestamps retained only {after_control['flamegraph']['total']} ns")
    finally:
        logs = subprocess.run(['docker', 'logs', container], capture_output=True, text=True)
        (args.output / 'pyroscope.log').write_text(logs.stdout + logs.stderr)
        subprocess.run(['docker', 'rm', '-f', container], stdout=subprocess.DEVNULL, check=True)


if __name__ == '__main__':
    main()
