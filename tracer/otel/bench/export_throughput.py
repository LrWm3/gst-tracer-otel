"""Compare bounded trace export settings against local and delayed receivers.

Build the release plugin and compile bench/pipeline.c into target/otel-bench/pipeline
before running. Each case has one excluded warm-up and rotating measured order.
"""
import argparse
import csv
import gzip
import hashlib
import http.server
import json
import os
from pathlib import Path
import platform
import re
import subprocess
import sys
import tempfile
import threading
import time

sys.dont_write_bytecode = True
from profiles import fields
ROOT = Path(__file__).resolve().parents[3]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--repeats', type=int, default=3)
    parser.add_argument('--buffers', type=int, default=10000)
    parser.add_argument('--cpus', default='0,2')
    parser.add_argument('--receiver-cpu', type=int, default=4)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    os.sched_setaffinity(0, {args.receiver_cpu})
    lock = threading.Lock()
    counts = {}
    delay = 0

    class Receiver(http.server.BaseHTTPRequestHandler):
        protocol_version = 'HTTP/1.1'

        def do_POST(self):
            body = self.rfile.read(int(self.headers['Content-Length']))
            encoded = len(body)
            if self.headers.get('Content-Encoding') == 'gzip':
                body = gzip.decompress(body)
            assert self.path == '/v1/traces', self.path
            spans = sum(len(fields(scope).get(2, [])) for resource in fields(body).get(1, [])
                        for scope in fields(resource).get(2, []))
            with lock:
                for key, value in dict(requests=1, received=spans, bytes=encoded,
                                       uncompressed_bytes=len(body), active=1).items():
                    counts[key] = counts.get(key, 0) + value
                counts['peak_active'] = max(counts.get('peak_active', 0), counts['active'])
            if delay:
                time.sleep(delay / 1000)
            with lock:
                counts['active'] -= 1
            self.send_response(200)
            self.send_header('Content-Length', '0')
            self.end_headers()

        def log_message(self, *_):
            pass

    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Receiver)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    binary = ROOT / 'target/otel-bench/pipeline'
    plugin = ROOT / 'target/release/libgstoteltracer.so'
    base = {k: v for k, v in os.environ.items() if not k.startswith(('GST_', 'OTEL_', 'MARS_'))}
    base.update(GST_PLUGIN_PATH=str(plugin.parent), GST_TRACERS='mars-otel', GST_DEBUG='0',
                GST_OTEL_LOGS='false', GST_OTEL_QUEUE_CAPACITY='8192', GST_OTEL_EXPORT_INTERVAL_MS='200',
                OTEL_BENCH_DRAIN_MS='0', OTEL_EXPORTER_OTLP_ENDPOINT=f'http://127.0.0.1:{server.server_port}',
                OTEL_TRACES_SAMPLER='parentbased_always_on')
    cases = [(512, 'none', 1), (2048, 'none', 1), (512, 'gzip', 1),
             (512, 'none', 4), (2048, 'none', 4), (2048, 'gzip', 4)]
    metadata = dict(architecture=platform.machine(), repeats=args.repeats, buffers=args.buffers,
                    cpus=args.cpus, receiver_cpu=args.receiver_cpu, delays_ms=[0, 50],
                    queue_capacity=8192, export_interval_ms=200, source_pause_us=100,
                    warmups_per_case=1, revision=subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
                    plugin_sha256=hashlib.sha256(plugin.read_bytes()).hexdigest(),
                    binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
                    source_sha256={str(p.relative_to(ROOT)): hashlib.sha256(p.read_bytes()).hexdigest()
                                   for p in list((ROOT/'tracer/otel/src').glob('*.rs')) +
                                   [Path(__file__).resolve(), ROOT/'tracer/otel/bench/pipeline.c', ROOT/'Cargo.lock']})
    (args.output/'metadata.json').write_text(json.dumps(metadata, indent=2)+'\n')
    rows = []
    try:
        with tempfile.TemporaryDirectory(prefix='mars-throughput-') as temporary:
            base['GST_REGISTRY'] = str(Path(temporary)/'registry.bin')
            for delay in (0, 50):
                for repeat in range(-1, args.repeats):
                    offset = max(repeat, 0) % len(cases)
                    for batch, compression, concurrency in cases[offset:]+cases[:offset]:
                        env = dict(base, GST_OTEL_BATCH_SIZE=str(batch), GST_OTEL_EXPORT_CONCURRENCY=str(concurrency),
                                   OTEL_EXPORTER_OTLP_COMPRESSION=compression)
                        with lock:
                            assert counts.get('active', 0) == 0
                            counts.clear()
                        pipeline = (f'fakesrc name=source0 num-buffers={args.buffers} ! identity sleep-time=100 ! '
                                    'identity ! identity ! identity ! fakesink name=sink0 sync=false async=false')
                        result = subprocess.run(['taskset','-c',args.cpus,str(binary),pipeline,'1'],env=env,
                                                capture_output=True,text=True,timeout=60)
                        assert result.returncode == 0,result.stdout+result.stderr
                        values = {k:int(v) for k,v in re.findall(r'(\w+)=(\d+)',re.search(r'^RESULT (.+)$',result.stdout,re.M)[1])}
                        stats = {k:int(v) for k,v in re.findall(r'(\w+)=\(guint64\)(\d+)',re.search(r'^COUNTERS (.+)$',result.stdout,re.M)[1])}
                        with lock:
                            receiver = dict(counts)
                        assert stats['pending_spans']==0 and stats['failed_export_spans']==0,stats
                        assert stats['exported_spans']==receiver['received'],(stats,receiver)
                        assert stats['completed_spans']==args.buffers*5,stats
                        assert stats['completed_spans']==stats['exported_spans']+stats['dropped_queue_full'],stats
                        assert receiver['peak_active']<=concurrency,receiver
                        row = dict(delay_ms=delay, repeat=repeat, batch=batch, compression=compression, concurrency=concurrency,
                                   **values, **stats, **receiver)
                        if repeat >= 0:
                            rows.append(row)
                            (args.output/f'{delay}-{batch}-{compression}-{concurrency}-{repeat}.txt').write_text(result.stdout)
                            with (args.output/'results.csv').open('w') as stream:
                                writer=csv.DictWriter(stream,fieldnames=list(row));writer.writeheader();writer.writerows(rows)
                        print(f'delay={delay} batch={batch} compression={compression} concurrency={concurrency} repeat={repeat} '
                              f'received={receiver["received"]} dropped={stats["dropped_queue_full"]} '
                              f'cpu_ms={values["process_cpu_ns"]/1e6:.1f} drain_ms={values["null_ns"]/1e6:.1f}',flush=True)
    finally:
        server.shutdown()


if __name__ == '__main__':
    main()
