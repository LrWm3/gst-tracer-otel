"""Measure Prometheus and OTLP metrics-only capture with validated HTTP output.

Build both release plugins first. Uses the same pipeline driver as instrument.py.
"""
import argparse
import csv
import datetime
import hashlib
import http.server
import json
import os
from pathlib import Path
import platform
import re
import selectors
import socket
import statistics
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request

sys.dont_write_bytecode = True
from profiles import fields

ROOT = Path(__file__).resolve().parents[3]
MODES = ('none', 'prometheus', 'instrument-metrics')


def prometheus_counts(body):
    totals = {}
    groups = {}
    for line in body.decode().splitlines():
        if not line or line.startswith('#'):
            continue
        name = line.split('{', 1)[0]
        if name == 'gst_element_latency_count_count' or name.endswith('_seconds_count'):
            totals[name] = totals.get(name, 0) + int(line.rsplit(' ', 1)[1])
            groups[name] = groups.get(name, 0) + 1
    return totals, groups


def otlp_counts(payloads):
    latest = {}
    for path, body in payloads:
        assert path == '/v1/metrics', f'Unexpected signal: {path}'
        for resource in fields(body)[1]:
            for scope in fields(resource)[2]:
                for raw in fields(scope)[2]:
                    metric = fields(raw)
                    name = metric[1][0].decode()
                    for raw_point in fields(metric[9][0])[1]:
                        point = fields(raw_point)
                        count = int.from_bytes(point[4][0], 'little')
                        # OTLP encodes fixed64 bucket counts packed or unpacked.
                        buckets = b''.join(point[6])
                        assert count == sum(int.from_bytes(buckets[i:i + 8], 'little')
                                            for i in range(0, len(buckets), 8))
                        assert not point[8], 'Metrics-only output contains an exemplar'
                        latest[name, tuple(point[9])] = count
    totals, groups = {}, {}
    for (name, _), count in latest.items():
        totals[name] = totals.get(name, 0) + count
        groups[name] = groups.get(name, 0) + 1
    return totals, groups


def run(binary, pipeline, streams, env, port, workdir, cpus):
    command = [str(binary), pipeline, str(streams)]
    if cpus:
        command = ['taskset', '-c', cpus, *command]
    # Bypass HTTP proxies for the local scrape receiver.
    client = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    scrapes, scrape_bytes, final_body = 0, 0, b''

    def scrape():
        nonlocal scrapes, scrape_bytes
        with client.open(f'http://127.0.0.1:{port}/metrics', timeout=5) as response:
            body = response.read()
        scrapes += 1
        scrape_bytes += len(body)
        return body

    with tempfile.TemporaryFile(dir=workdir) as errors:
        with subprocess.Popen(command, env=env, stdin=subprocess.PIPE,
                              stdout=subprocess.PIPE, stderr=errors) as child:
            try:
                deadline, next_scrape = time.monotonic() + 60, time.monotonic() + 1
                # The driver emits no stdout until READY_FOR_SCRAPE. It holds
                # the graph alive so the final HTTP response has exact counts.
                with selectors.DefaultSelector() as selector:
                    selector.register(child.stdout, selectors.EVENT_READ)
                    while True:
                        now = time.monotonic()
                        if now >= deadline:
                            raise TimeoutError('Pipeline did not reach the final scrape')
                        if selector.select(min(0.1, deadline - now)):
                            marker = child.stdout.readline()
                            assert marker == b'READY_FOR_SCRAPE\n', marker
                            break
                        if port and time.monotonic() >= next_scrape:
                            scrape()
                            next_scrape = time.monotonic() + 1
                if port:
                    final_body = scrape()
                output, _ = child.communicate(b'\n', timeout=max(1, deadline - time.monotonic()))
                if child.returncode:
                    raise RuntimeError(f'Pipeline exited with {child.returncode}')
            except BaseException:
                child.kill()
                child.wait()
                errors.seek(0)
                sys.stderr.write(errors.read().decode(errors='replace'))
                raise
    return output.decode(), final_body, scrapes, scrape_bytes


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--plugin-dir', type=Path, default=ROOT / 'target/release')
    parser.add_argument('--repeats', type=int, default=7)
    parser.add_argument('--buffers', type=int, default=1000000)
    parser.add_argument('--streams', type=int, nargs='+', default=[1, 4])
    parser.add_argument('--sample-rate', type=int, default=16)
    parser.add_argument('--cpus', help='Optional taskset CPU list for the pipeline process')
    parser.add_argument('--output', type=Path, default=ROOT / 'target/metrics-bench/results.csv')
    args = parser.parse_args()
    if min(args.repeats, args.buffers, args.sample_rate, *args.streams) < 1:
        parser.error('Counts and sample rate must be positive')
    if any(s > 64 or args.buffers % s for s in args.streams):
        parser.error('Each stream count must be <=64 and divide the buffer count')
    args.output.parent.mkdir(parents=True, exist_ok=True)
    workdir = ROOT / 'target/metrics-bench'
    workdir.mkdir(parents=True, exist_ok=True)
    binary = workdir / 'pipeline'
    subprocess.run(['cc', '-O2', '-Wall', '-Wextra', '-Werror',
                    str(Path(__file__).with_name('pipeline.c')), '-o', str(binary),
                    *subprocess.check_output(['pkg-config', '--cflags', '--libs',
                                              'gstreamer-1.0'], text=True).split()], check=True)
    plugins = [args.plugin_dir / name for name in ('libgstoteltracer.so', 'libgstprometheustracer.so')]
    metadata = dict(date=datetime.datetime.now(datetime.timezone.utc).isoformat(),
                    revision=subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip(),
                    command=sys.argv, host=platform.platform(),
                    cpu=next(line.split(':', 1)[1].strip() for line in Path('/proc/cpuinfo').read_text().splitlines()
                             if line.startswith('model name')),
                    allowed_cpus=sorted(os.sched_getaffinity(0)), cpus=args.cpus,
                    governors={p.parent.parent.name: p.read_text().strip() for p in
                               Path('/sys/devices/system/cpu').glob('cpu[0-9]*/cpufreq/scaling_governor')},
                    rustc=subprocess.check_output(['rustc', '--version'], text=True).strip(),
                    gstreamer=subprocess.check_output(['pkg-config', '--modversion', 'gstreamer-1.0'], text=True).strip(),
                    plugin_sha256={p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in plugins},
                    harness_sha256={p.name: hashlib.sha256(p.read_bytes()).hexdigest()
                                    for p in (Path(__file__), Path(__file__).with_name('pipeline.c'))})
    args.output.with_suffix('.json').write_text(json.dumps(metadata, indent=2) + '\n')
    payloads = []

    class Receiver(http.server.BaseHTTPRequestHandler):
        def do_POST(self):
            payloads.append((self.path, self.rfile.read(int(self.headers['Content-Length']))))
            self.send_response(200)
            self.send_header('Content-Length', '0')
            self.end_headers()

        def log_message(self, *args):
            pass

    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Receiver)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    base = {k: v for k, v in os.environ.items() if not k.startswith(('GST_', 'OTEL_', 'MARS_'))}
    base.update(GST_PLUGIN_PATH=str(args.plugin_dir.resolve()), GST_DEBUG='0',
                GST_REGISTRY=str(workdir / 'registry.bin'),
                OTEL_EXPORTER_OTLP_ENDPOINT=f'http://127.0.0.1:{server.server_port}',
                OTEL_SERVICE_NAME='metrics-bench', GST_INSTRUMENT_TRACES='false',
                GST_INSTRUMENT_METRICS='true', GST_INSTRUMENT_LOGS='false',
                GST_INSTRUMENT_PROFILES='false', GST_INSTRUMENT_METRIC_INTERVAL_MS='1000',
                GST_INSTRUMENT_CPU_SAMPLE_RATE=str(args.sample_rate),
                GST_PROM_LATENCY_INCLUDE='*', GST_PROM_LATENCY_PTS_AGE='false',
                GST_PROM_LATENCY_IO_COUNTS='false',
                GST_PROM_LATENCY_HISTOGRAM_SAMPLE_RATE=str(args.sample_rate),
                OTEL_BENCH_DRAIN_MS='0', OTEL_BENCH_WAIT_FOR_SCRAPE='1')
    rows = []
    try:
        for streams in args.streams:
            pipeline = ' '.join(f'fakesrc name=source{i} num-buffers={args.buffers // streams} ! '
                                'identity ! identity ! identity ! identity ! '
                                f'fakesink name=sink{i} sync=false async=false' for i in range(streams))
            for repeat in range(-1, args.repeats):
                # One unrecorded warmup per mode/source count, then rotated rounds.
                offset = max(0, repeat) % len(MODES)
                for mode in MODES[offset:] + MODES[:offset]:
                    payloads.clear()
                    port = 0
                    if mode == 'prometheus':
                        with socket.socket() as reservation:
                            reservation.bind(('127.0.0.1', 0))
                            port = reservation.getsockname()[1]
                    tracer = '' if mode == 'none' else f'mars-metrics(port={port})' if port else 'mars-instrument'
                    output, body, scrapes, scrape_bytes = run(
                        binary, pipeline, streams, dict(base, GST_TRACERS=tracer), port, workdir, args.cpus)
                    totals, groups = prometheus_counts(body) if port else otlp_counts(payloads)
                    assert not (port and payloads), 'Prometheus emitted OTLP output'
                    wall = 'gst_element_sync_wall_seconds_count' if port else 'gst.pad.push.exclusive.wall'
                    cpu = 'gst_element_sync_cpu_seconds_count' if port else 'gst.pad.push.exclusive.cpu'
                    blocked = 'gst_element_sync_blocked_seconds_count' if port else 'gst.pad.push.exclusive.blocked'
                    count = totals.get('gst_element_latency_count_count' if port else 'gst.pad.push.wall', 0)
                    expected = args.buffers * 5 if mode != 'none' else 0
                    assert count == expected, (mode, count, expected)
                    if expected:
                        assert groups[wall] == streams * 5, groups
                        assert 0 < totals[cpu] == totals[blocked] <= expected, totals
                        assert totals[wall] == (totals[cpu] if port else expected), totals
                    if mode == 'instrument-metrics':
                        counters = dict((k, int(v)) for k, v in re.findall(
                            r'(\w+)=\(guint64\)(\d+)', output.split('METRICS ')[1]))
                        assert counters['recorded_pushes'] == expected, counters
                        assert all(counters[k] == 0 for k in ('failed_exports', 'dropped_capacity',
                                                             'dropped_invalid', 'skipped_closed')), counters
                    row = dict(mode=mode, streams=streams, buffers=args.buffers, sample_rate=args.sample_rate,
                               repeat=repeat, **{k: int(v) for k, v in re.findall(
                                   r'(\w+)=(\d+)', output.split('RESULT ')[1].splitlines()[0])},
                               metric_pushes=count, wall_histogram_samples=totals.get(wall, 0),
                               cpu_histogram_samples=totals.get(cpu, 0),
                               http_requests=scrapes + len(payloads),
                               http_payload_bytes=scrape_bytes + sum(len(body) for _, body in payloads))
                    assert row['status'] == 0, row
                    print(('warmup ' if repeat < 0 else '') + str(row), flush=True)
                    if repeat < 0:
                        continue
                    rows.append(row)
                    with args.output.open('w', newline='') as file:
                        writer = csv.DictWriter(file, fieldnames=list(rows[0]), lineterminator='\n')
                        writer.writeheader()
                        writer.writerows(rows)
        for streams in args.streams:
            for mode in MODES:
                group = [r for r in rows if r['streams'] == streams and r['mode'] == mode]
                print(f'{streams} sources, {mode}: source CPU '
                      f'{statistics.median(r["stream_cpu_ns"] for r in group)/1e6:.1f} ms; '
                      f'process CPU {statistics.median(r["process_cpu_ns"] for r in group)/1e6:.1f} ms; '
                      f'peak RSS {statistics.median(r["max_rss_kb"] for r in group)/1024:.1f} MiB')
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


if __name__ == '__main__':
    main()
