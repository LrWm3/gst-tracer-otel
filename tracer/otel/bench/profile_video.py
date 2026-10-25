"""Compare profiles, traces, logs and combined metrics on a matched 1080p workload.

Build the profile_video example with --release --features profiles and
RUSTFLAGS='-C force-frame-pointers=yes'. This runner never builds during timing.
The receiver only stores HTTP payloads; decode/validation happens after exit.
"""
import argparse
import collections
import csv
import datetime
import hashlib
import http.server
import json
import os
from pathlib import Path
import platform
import re
import statistics
import subprocess
import sys
import threading
import time

sys.dont_write_bytecode = True
from profiles import analyze, fields

ROOT = Path(__file__).resolve().parents[3]
MODES = ('none', 'native-logs', 'profiles', 'otel', 'otel-logs',
         'otel-profiles', 'otel-profiles-logs', 'instrument', 'instrument-profiles-logs', 'profiles-raw')


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def validate_metrics(payloads, spans, expected):
    latest = {}
    exemplars = set()
    exemplar_count = 0
    names = {'gst.pad.push.wall', 'gst.pad.push.exclusive.wall',
             'gst.pad.push.exclusive.cpu', 'gst.pad.push.exclusive.blocked'}
    for path, body in payloads:
        if path != '/v1/metrics':
            continue
        for resource in fields(body)[1]:
            for scope in fields(resource)[2]:
                for raw in fields(scope)[2]:
                    metric = fields(raw)
                    name = metric[1][0].decode()
                    if name.startswith("mars_instrument_"):
                        continue
                    assert name in names, name
                    histogram = fields(metric[9][0])
                    assert histogram[2] == [2], 'expected cumulative histograms'
                    for raw_point in histogram[1]:
                        point = fields(raw_point)
                        count = int.from_bytes(point[4][0], 'little')
                        buckets = b''.join(point[6])
                        assert len(buckets) % 8 == 0
                        assert count == sum(int.from_bytes(buckets[i:i + 8], 'little')
                                            for i in range(0, len(buckets), 8))
                        attributes = tuple(sorted(point[9]))
                        assert len(attributes) == 8
                        assert all(fields(a)[1][0] not in (b'trace_id', b'span_id') for a in attributes)
                        latest[name, attributes] = count
                        for raw_example in point[8]:
                            example = fields(raw_example)
                            key = (example[5][0], example[4][0])
                            assert key in spans, 'metric exemplar has no exported span'
                            exemplars.add(key)
                            exemplar_count += 1
    totals = collections.Counter()
    for (name, attributes), count in latest.items():
        totals[name] += count
        if name == 'gst.pad.push.exclusive.cpu':
            assert latest.get(('gst.pad.push.exclusive.blocked', attributes)) == count
            assert count <= latest['gst.pad.push.wall', attributes]
    assert totals['gst.pad.push.wall'] == totals['gst.pad.push.exclusive.wall'] == expected
    assert totals['gst.pad.push.exclusive.cpu'] == totals['gst.pad.push.exclusive.blocked']
    if expected:
        assert set(totals) == names and totals['gst.pad.push.exclusive.cpu'] > 0
        for name in ('gst.pad.push.wall', 'gst.pad.push.exclusive.wall'):
            counts = [count for (metric, _), count in latest.items() if metric == name]
            assert counts == [expected // 4] * 4, counts
        assert exemplars, 'expected sampled metric exemplars'
    else:
        assert not latest
    return dict(metric_pushes=totals['gst.pad.push.wall'],
                metric_cpu_observations=totals['gst.pad.push.exclusive.cpu'],
                metric_histograms=len(latest), metric_exemplars=exemplar_count), exemplars


def validate(payloads, mode, frames, hz, profile, trace, metrics):
    is_otel = mode.startswith(('otel', 'instrument'))
    has_metrics = mode.startswith('instrument')
    has_profiles = 'profiles' in mode
    has_logs = is_otel and 'logs' in mode
    paths = collections.Counter(path for path, _ in payloads)
    allowed = set()
    if is_otel:
        allowed.add('/v1/traces')
    if has_profiles:
        allowed.add('/push.v1.PusherService/Push')
    if has_logs:
        allowed.add('/v1/logs')
    if has_metrics:
        allowed.add('/v1/metrics')
    assert set(paths) == allowed, (mode, paths, allowed)
    decoded = analyze([(path, body) for path, body in payloads if path not in ('/v1/logs', '/v1/metrics')])
    expected = frames * 4 if is_otel else 0
    assert decoded['received_spans'] == trace['exported_spans'] == expected
    assert trace['started_spans'] == trace['completed_spans'] == expected
    if has_profiles:
        assert decoded['profiles'] > 0 and profile['samples'] > 0
        assert decoded['profile_cpu_ns'] == profile['samples'] * (1_000_000_000 // hz)
        assert decoded['correlated_cpu_ns'] == profile['correlated_samples'] * (1_000_000_000 // hz)
        if is_otel and frames >= 600:
            assert decoded['correlated_spans'] > 0
    else:
        assert decoded['profiles'] == profile['samples'] == 0
    if not is_otel:
        assert decoded['correlated_spans'] == 0
    spans = set()
    logs = []
    for path, body in payloads:
        if path not in ('/v1/traces', '/v1/logs'):
            continue
        for resource in fields(body)[1]:
            for scope in fields(resource)[2]:
                for raw in fields(scope)[2]:
                    record = fields(raw)
                    if path == '/v1/traces':
                        key = (record[1][0], record[2][0])
                        assert key not in spans, 'duplicate span'
                        spans.add(key)
                    else:
                        logs.append(record)
    assert len(logs) == (frames if has_logs else 0), (mode, len(logs), frames)
    messages = []
    for log in logs:
        assert (log[9][0], log[10][0]) in spans, 'uncorrelated log'
        message = fields(log[5][0])[1][0].decode()
        match = re.fullmatch(r'processed frame (\d+)', message)
        assert match, message
        messages.append(int(match[1]))
    if has_logs:
        assert sorted(messages) == list(range(1, frames + 1))
    metric_values, exemplars = validate_metrics(payloads, spans, frames * 4 if has_metrics else 0)
    assert metrics['recorded_pushes'] == metric_values['metric_pushes']
    assert metrics['exported_batches'] == paths['/v1/metrics']
    profiled_spans = {(bytes.fromhex(s['trace_id']), bytes.fromhex(s['span_id']))
                      for s in decoded['span_cpu_ns']}
    return dict(
        received_spans=decoded['received_spans'], received_logs=len(logs),
        profiles=decoded['profiles'], profile_cpu_ns=decoded['profile_cpu_ns'],
        correlated_cpu_ns=decoded['correlated_cpu_ns'], correlated_spans=decoded['correlated_spans'],
        **metric_values, profiled_exemplar_spans=len(exemplars & profiled_spans),
        http_requests=sum(paths.values()), http_payload_bytes=sum(len(body) for _, body in payloads),
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=ROOT / 'target/profile-video-build/release/examples/profile_video')
    parser.add_argument('--build-profile', choices=('release', 'profiling'), default='release',
                        help='Record the profile used to build --binary; does not build or select a binary')
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--frames', type=int, default=1200)
    parser.add_argument('--repeats', type=int, default=7)
    parser.add_argument('--cpus', default='0,2')
    parser.add_argument('--hz', type=int, default=100)
    parser.add_argument('--export-interval-ms', type=int, default=10, help='Trace export interval; plugin default is 200')
    parser.add_argument('--queue-capacity', type=int, default=65536, help='Trace queue records; plugin default is 8192')
    parser.add_argument('--perf', action='store_true', help='Also record whole-process hardware cycles and instructions')
    parser.add_argument('--modes', nargs='+', choices=MODES, default=MODES[:7])
    args = parser.parse_args()
    assert args.frames > 0 and args.repeats > 0 and 1 <= args.hz <= 1000
    assert args.export_interval_ms > 0 and args.queue_capacity > 0
    binary = args.binary.resolve()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    work = ROOT / 'target/profile-video-runs' / args.output.stem
    work.mkdir(parents=True, exist_ok=True)
    source_paths = [Path(__file__), ROOT / 'tracer/otel/examples/profile_video.rs',
                    Path(__file__).with_name('profiles.py'), ROOT / 'Cargo.lock']
    metadata = dict(
        date=datetime.datetime.now(datetime.timezone.utc).isoformat(),
        revision=subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
        command=sys.argv, rustc=subprocess.check_output(['rustc', '-Vv'], text=True),
        perf_version=subprocess.check_output(['perf', '--version'], text=True).strip() if args.perf else None,
        gstreamer=subprocess.check_output(['pkg-config', '--modversion', 'gstreamer-1.0'], text=True).strip(),
        uname=platform.uname()._asdict(),
        cpu=subprocess.check_output(['lscpu'], text=True),
        affinity=list(os.sched_getaffinity(0)), child_cpus=args.cpus,
        governor=Path('/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor').read_text().strip(),
        binary_sha256=sha256(binary), source_sha256={str(p.relative_to(ROOT)): sha256(p) for p in source_paths},
        settings=dict(frames=args.frames, repeats=args.repeats, hz=args.hz, modes=list(args.modes),
                      rustflags='-C force-frame-pointers=yes', profile=args.build_profile,
                      log_messages_per_frame=1, trace_sample_ratio=1,
                      queue_capacity=args.queue_capacity, export_interval_ms=args.export_interval_ms,
                      metric_interval_ms=10000, metric_cpu_sample_rate=16,
                      element_attribution={mode: mode != 'profiles-raw' for mode in args.modes},
                      perf=args.perf),
    )
    args.output.with_suffix('.json').write_text(json.dumps(metadata, indent=2) + '\n')
    payloads = []

    class Receiver(http.server.BaseHTTPRequestHandler):
        protocol_version = 'HTTP/1.1'

        def do_POST(self):
            payloads.append((self.path, self.rfile.read(int(self.headers['Content-Length']))))
            self.send_response(200)
            self.send_header('Content-Length', '0')
            self.end_headers()

        def log_message(self, *args):
            pass

    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Receiver)
    thread = threading.Thread(target=server.serve_forever)
    thread.start()
    endpoint = f'http://127.0.0.1:{server.server_port}'
    base = {k: v for k, v in os.environ.items() if not k.startswith(('GST_', 'OTEL_', 'MARS_'))}
    base.update(GST_PLUGIN_PATH='', GST_TRACERS='', GST_DEBUG_NO_COLOR='1',
                GST_REGISTRY=str(work / 'registry.bin'), OTEL_SERVICE_NAME='gst-profile-bench',
                OTEL_EXPORTER_OTLP_ENDPOINT=endpoint, OTEL_EXPORTER_OTLP_PROTOCOL='http/protobuf',
                OTEL_TRACES_SAMPLER='parentbased_always_on', GST_OTEL_PROFILE_HZ=str(args.hz),
                GST_OTEL_QUEUE_CAPACITY=str(args.queue_capacity), GST_OTEL_BATCH_SIZE='512',
                GST_OTEL_EXPORT_INTERVAL_MS=str(args.export_interval_ms),
                GST_INSTRUMENT_METRIC_INTERVAL_MS='10000', GST_INSTRUMENT_CPU_SAMPLE_RATE='16')
    rows = []
    try:
        # A discarded warm-up for each mode, followed by a full rotation of mode
        # order. Every child starts fresh: cold symbolization remains included.
        for repeat in range(-1, args.repeats):
            offset = max(0, repeat) % len(args.modes)
            for mode in args.modes[offset:] + args.modes[:offset]:
                logging = 'logs' in mode
                env = dict(base, GST_DEBUG='0,mars-profile-bench:4' if logging else '0',
                           GST_OTEL_PROFILE_ELEMENTS='false' if mode == 'profiles-raw' else 'true',
                           GST_OTEL_LOGS='true' if logging and mode.startswith(('otel', 'instrument')) else 'false')
                if 'profiles' in mode:
                    env['GST_OTEL_PROFILE_URL'] = endpoint
                driver_mode = ('instrument' if mode.startswith('instrument') else
                               'otel' if mode.startswith('otel') else 'profiles' if mode.startswith('profiles') else 'none')
                payloads.clear()
                start = time.time_ns()
                command = ['taskset', '-c', args.cpus]
                perf_path = work / f'{mode}-{repeat}.perf'
                if args.perf:
                    command += ['perf', 'stat', '-x', ';', '-e', 'cycles,instructions', '-o', str(perf_path), '--']
                command += [str(binary), driver_mode, str(args.frames)]
                result = subprocess.run(command,
                                        env=env, capture_output=True, text=True, timeout=150)
                output = result.stdout + result.stderr
                (work / f'{mode}-{repeat}.log').write_text(output)
                assert result.returncode == 0, output
                values = {k: int(v) for k, v in re.findall(r'(\w+)=(\d+)', re.search(r'^RESULT (.+)$', output, re.M)[1])}
                if args.perf:
                    hardware = collections.Counter()
                    for line in perf_path.read_text().splitlines():
                        columns = line.split(';')
                        if len(columns) < 5 or not columns[0].strip().isdigit():
                            continue
                        for event in ('cycles', 'instructions'):
                            if event in columns[2]:
                                # Reject multiplexed measurements; on hybrid
                                # CPUs sum the events that actually ran.
                                assert float(columns[4]) >= 99, line
                                hardware[event] += int(columns[0])
                    assert hardware['cycles'] > 0 and hardware['instructions'] > 0
                    values.update(hardware)
                counters = {}
                for prefix in ('TRACE', 'PROFILE', 'METRIC'):
                    line = re.search(rf'^{prefix} .+$', output, re.M)[0]
                    counters[prefix] = {k: int(v) for k, v in re.findall(r'(\w+): (\d+)', line)}
                decoded = validate(payloads, mode, args.frames, args.hz,
                                   counters['PROFILE'], counters['TRACE'], counters['METRIC'])
                # The standard GStreamer handler remains enabled in log modes;
                # verify its output too, including the no-tracer logging control.
                assert len(re.findall(r'processed frame \d+', result.stderr)) == (args.frames if logging else 0)
                row = dict(mode=mode, repeat=repeat, started_ns=start, cpus=args.cpus, hz=args.hz,
                           **values, **decoded,
                           **{'profile_' + k: v for k, v in counters['PROFILE'].items()},
                           **{'metric_' + k: v for k, v in counters['METRIC'].items()})
                rows.append(row)
                with args.output.open('w', newline='') as out:
                    writer = csv.DictWriter(out, fieldnames=list(row))
                    writer.writeheader()
                    writer.writerows(rows)
                print(f'{repeat:2} {mode:20} CPU={values["process_cpu_ns"]/1e9:.3f}s '
                      f'wall={values["elapsed_ns"]/1e9:.3f}s RSS={values["max_rss_kb"]/1024:.1f}MiB '
                      f'spans={decoded["received_spans"]} logs={decoded["received_logs"]} '
                      f'metric_pushes={decoded["metric_pushes"]} '
                      f'samples={counters["PROFILE"]["samples"]}', flush=True)
        for mode in args.modes:
            measured = [r for r in rows if r['mode'] == mode and r['repeat'] >= 0]
            print('MEDIAN', mode, json.dumps({k: statistics.median(r[k] for r in measured) for k in
                  ('process_cpu_ns', 'total_cpu_ns', 'stream_cpu_ns', 'elapsed_ns', 'max_rss_kb', 'shutdown_ns')}), flush=True)
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


if __name__ == '__main__':
    main()
