"""Compare none/core/all on the same 1080p workload and selected exporters.

Build release profile_video and both plugins first. Each configuration has one
excluded warm-up; measured order rotates. Receiver CPU is outside the child.
"""
import argparse
import csv
import hashlib
import http.server
import json
import os
from pathlib import Path
import platform
import re
import socket
import statistics
import subprocess
import tempfile
import threading
import time
import urllib.request

ROOT = Path(__file__).resolve().parents[3]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--frames', type=int, default=240)
    parser.add_argument('--repeats', type=int, default=5)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--perf', action='store_true')
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    binary = ROOT / 'target/release/examples/profile_video'
    rows = []
    counts = {}
    lock = threading.Lock()

    class Receiver(http.server.BaseHTTPRequestHandler):
        def do_POST(self):
            body = self.rfile.read(int(self.headers['Content-Length']))
            with lock:
                counts['http_requests'] = counts.get('http_requests', 0) + 1
                counts['http_bytes'] = counts.get('http_bytes', 0) + len(body)
            self.send_response(200)
            self.send_header('Content-Length', '0')
            self.end_headers()

        def log_message(self, *_):
            pass

    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Receiver)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    cpus = sorted(os.sched_getaffinity(0))
    affinity = ','.join(map(str, cpus[:1] + cpus[2:3]))
    base = {k: v for k, v in os.environ.items() if not k.startswith(('GST_', 'OTEL_', 'MARS_'))}
    base.update(GST_PLUGIN_PATH=str(ROOT / 'target/release'), GST_DEBUG='0',
                MARS_METRIC_SAMPLE_RATE='16', MARS_METRIC_INTERVAL_MS='1000',
                OTEL_SERVICE_NAME='mars-monitoring-bench',
                OTEL_EXPORTER_OTLP_ENDPOINT=f'http://127.0.0.1:{server.server_port}')
    cases = [(mode, level) for mode in ('metrics', 'metrics-traces', 'standalone')
             for level in ('none', 'core', 'all')]
    metadata = dict(architecture=platform.machine(), frames=args.frames, repeats=args.repeats,
                    cpus=affinity, metric_interval_ms=1000, metric_sample_rate=16,
                    trace_sampling=1, warmups_per_case=1, perf=args.perf,
                    revision=subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip(),
                    binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
                    plugin_sha256={p.name: hashlib.sha256(p.read_bytes()).hexdigest()
                                   for p in (ROOT / 'target/release').glob('libgst*tracer.so')},
                    diff_sha256=hashlib.sha256(subprocess.check_output(['git', 'diff'], cwd=ROOT)).hexdigest())
    (args.output / 'metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')
    try:
        with tempfile.TemporaryDirectory(prefix='mars-monitoring-') as temp:
            base['GST_REGISTRY'] = str(Path(temp) / 'registry.bin')
            for repeat in range(-1, args.repeats):
                offset = max(repeat, 0) % len(cases)
                for mode, level in cases[offset:] + cases[:offset]:
                    env = dict(base, MARS_MONITORING=level)
                    stop = threading.Event()
                    scrapes = []
                    scraper = None
                    if mode == 'standalone':
                        with socket.socket() as reserve:
                            reserve.bind(('127.0.0.1', 0))
                            port = reserve.getsockname()[1]
                        env['GST_TRACERS'] = f'mars-metrics(port={port},monitoring={level})'
                        driver = 'none'

                        def scrape():
                            while not stop.wait(0.1 if not scrapes else 1):
                                try:
                                    with urllib.request.urlopen(f'http://127.0.0.1:{port}/metrics', timeout=0.5) as response:
                                        scrapes.append(response.read().decode())
                                except OSError:
                                    pass
                        scraper = threading.Thread(target=scrape)
                        scraper.start()
                    else:
                        driver = 'instrument'
                        env['MARS_SIGNALS'] = 'metrics' if mode == 'metrics' else 'metrics,traces'
                    with lock:
                        counts.clear()
                    command = ['taskset', '-c', affinity]
                    perf = Path(temp) / 'perf.txt'
                    if args.perf:
                        command += ['perf', 'stat', '-x', ';', '-e', 'cycles,instructions', '-o', str(perf), '--']
                    command += [str(binary), driver, str(args.frames)]
                    try:
                        result = subprocess.run(command, env=env, text=True, capture_output=True, timeout=60)
                    finally:
                        stop.set()
                        if scraper:
                            scraper.join()
                    assert result.returncode == 0, result.stderr + result.stdout
                    if mode == 'standalone':
                        assert scrapes, 'no standalone scrape completed'
                        assert ('mars_instrument_collector_pushes_total' in scrapes[-1]) == (level != 'none')
                        assert ('mars_instrument_work_duration_seconds' in scrapes[-1]) == (level == 'all')
                    row = dict(mode=mode, level=level, repeat=repeat)
                    row.update({k: int(v) for k, v in re.findall(r'(\w+)=(\d+)', re.search(r'^RESULT (.+)$', result.stdout, re.M)[1])})
                    if args.perf:
                        hardware = dict(cycles=0, instructions=0)
                        for line in perf.read_text().splitlines():
                            columns = line.split(';')
                            if len(columns) < 5 or not columns[0].strip().isdigit():
                                continue
                            for event in hardware:
                                if event in columns[2]:
                                    assert float(columns[4]) >= 99, line
                                    hardware[event] += int(columns[0])
                        assert all(hardware.values()), 'hardware counters unavailable'
                        row.update(hardware)
                    with lock:
                        row.update(http_requests=counts.get('http_requests', 0), http_bytes=counts.get('http_bytes', 0))
                    row['scrapes'] = len(scrapes)
                    if repeat >= 0:
                        rows.append(row)
                        (args.output / f'{mode}-{level}-{repeat}.txt').write_text(result.stdout)
                        with (args.output / 'results.csv').open('w') as stream:
                            writer = csv.DictWriter(stream, fieldnames=list(row))
                            writer.writeheader()
                            writer.writerows(rows)
                    print(f'{mode}/{level} repeat={repeat} cpu={row["total_cpu_ns"] / 1e9:.3f}s rss={row["max_rss_kb"] / 1024:.1f}MiB', flush=True)
        for mode, level in cases:
            selected = [r for r in rows if r['mode'] == mode and r['level'] == level]
            print(mode, level, 'median CPU seconds', statistics.median(r['total_cpu_ns'] / 1e9 for r in selected), flush=True)
    finally:
        server.shutdown()


if __name__ == '__main__':
    main()
