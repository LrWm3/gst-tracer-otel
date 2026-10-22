"""Compare shared capture modes against decoded OTLP counts; build release first."""
import argparse
import csv
import http.server
import os
from pathlib import Path
import re
import statistics
import subprocess
import threading
import sys
sys.dont_write_bytecode = True
from profiles import fields

ROOT = Path(__file__).resolve().parents[3]


def analyze(payloads):
    spans = 0
    points = {}
    for path, body in payloads:
        if path == '/v1/traces':
            spans += sum(len(fields(scope)[2]) for resource in fields(body)[1]
                         for scope in fields(resource)[2])
        elif path == '/v1/metrics':
            for resource in fields(body)[1]:
                for scope in fields(resource)[2]:
                    for raw in fields(scope)[2]:
                        metric = fields(raw)
                        if metric[1][0] != b'gst.pad.push.wall':
                            continue
                        for point in fields(metric[9][0])[1]:
                            point = fields(point)
                            key = tuple(point[9])
                            points[key] = int.from_bytes(point[4][0], 'little')
        else:
            raise AssertionError(path)
    return spans, sum(points.values())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--plugin-dir', type=Path, default=ROOT / 'target/release')
    parser.add_argument('--repeats', type=int, default=5)
    parser.add_argument('--buffers', type=int, default=10000)
    parser.add_argument('--streams', type=int, nargs='+', default=[1, 4])
    parser.add_argument('--output', type=Path, default=ROOT / 'target/instrument-bench/results.csv')
    args = parser.parse_args()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    binary = ROOT / 'target/instrument-bench/pipeline'
    binary.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(['cc', '-O2', '-Wall', '-Wextra', str(Path(__file__).with_name('pipeline.c')),
                    '-o', str(binary), *subprocess.check_output(
                        ['pkg-config', '--cflags', '--libs', 'gstreamer-1.0'], text=True).split()], check=True)
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
    base = {k: v for k, v in os.environ.items() if not k.startswith(('GST_', 'OTEL_'))}
    base.update(GST_PLUGIN_PATH=str(args.plugin_dir.resolve()), GST_DEBUG='0',
                GST_REGISTRY=str(binary.parent / 'registry.bin'),
                OTEL_EXPORTER_OTLP_ENDPOINT=f'http://127.0.0.1:{server.server_port}',
                OTEL_SERVICE_NAME='instrument-bench', GST_OTEL_QUEUE_CAPACITY='65536',
                GST_OTEL_BATCH_SIZE='512', GST_OTEL_EXPORT_INTERVAL_MS='200',
                GST_INSTRUMENT_METRIC_INTERVAL_MS='60000',
                OTEL_BENCH_DRAIN_MS='0', GST_INSTRUMENT_PROFILES='false', GST_INSTRUMENT_LOGS='false')
    rows = []
    modes = ['none', 'otel', 'metrics', 'instrument', 'instrument-unsampled']
    try:
        for streams in args.streams:
            assert args.buffers % streams == 0
            pipeline = ' '.join(f'fakesrc name=source{i} num-buffers={args.buffers // streams} ! '
                                f'identity ! identity ! identity ! identity ! fakesink name=sink{i} '
                                'sync=false async=false' for i in range(streams))
            for repeat in range(args.repeats):
                offset = repeat % len(modes)
                for mode in modes[offset:] + modes[:offset]:
                    payloads.clear()
                    env = dict(base, GST_TRACERS='' if mode == 'none' else 'mars-otel' if mode == 'otel' else 'mars-instrument',
                               GST_INSTRUMENT_TRACES='false' if mode == 'metrics' else 'true',
                               OTEL_TRACES_SAMPLER='parentbased_always_off' if mode == 'instrument-unsampled' else 'parentbased_always_on')
                    result = subprocess.run([str(binary), pipeline, str(streams)], env=env, capture_output=True, timeout=45, check=True)
                    output = result.stdout.decode()
                    row = dict(mode=mode, streams=streams, buffers=args.buffers, repeat=repeat,
                               **{k: int(v) for k, v in re.findall(r'(\w+)=(\d+)', output.split('RESULT ')[1].splitlines()[0])})
                    row['received_spans'], row['metric_pushes'] = analyze(payloads)
                    expected = args.buffers * 5
                    assert row['received_spans'] == (expected if mode in ('otel', 'instrument') else 0), row
                    assert row['metric_pushes'] == (0 if mode in ('none', 'otel') else expected), row
                    if mode not in ('none', 'otel'):
                        metrics = dict((k, int(v)) for k, v in re.findall(r'(\w+)=\(guint64\)(\d+)', output.split('METRICS ')[1]))
                        assert metrics['recorded_pushes'] == expected and metrics['failed_exports'] == 0, metrics
                    rows.append(row)
                    print(row, flush=True)
                    with args.output.open('w', newline='') as file:
                        writer = csv.DictWriter(file, fieldnames=list(rows[0]), lineterminator="\n")
                        writer.writeheader()
                        writer.writerows(rows)
        for streams in args.streams:
            for mode in modes:
                group = [r for r in rows if r['streams'] == streams and r['mode'] == mode]
                print(f'{streams} streams, {mode}: source CPU {statistics.median(r["stream_cpu_ns"] for r in group)/1e6:.1f} ms; '
                      f'process CPU {statistics.median(r["process_cpu_ns"] for r in group)/1e6:.1f} ms; '
                      f'peak RSS {statistics.median(r["max_rss_kb"] for r in group)/1024:.1f} MiB')
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


if __name__ == '__main__':
    main()
