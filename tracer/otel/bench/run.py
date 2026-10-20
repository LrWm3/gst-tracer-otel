import http.server
import json
import os
from pathlib import Path
import re
import subprocess
import argparse
import time
import threading

ROOT = Path(__file__).resolve().parents[3]
OUT = ROOT / 'target/otel-bench'
OUT.mkdir(parents=True, exist_ok=True)
parser = argparse.ArgumentParser(description='Build the plugin first; optionally compare a prebuilt baseline.')
parser.add_argument('--baseline', type=Path, help='Directory containing the old plugin without Pyroscope')
parser.add_argument('--repeats', type=int, default=3)
args = parser.parse_args()
subprocess.run(['cc', '-O2', '-Wall', '-Wextra', str(Path(__file__).with_name('pipeline.c')),
                '-o', str(OUT / 'pipeline'), *subprocess.check_output(
                    ['pkg-config', '--cflags', '--libs', 'gstreamer-1.0'], text=True).split()], check=True)
receiver_delay = 0
received = 0


def varint(data, pos):
    value, shift = 0, 0
    while True:
        byte = data[pos]
        pos += 1
        value |= (byte & 127) << shift
        if byte < 128:
            return value, pos
        shift += 7


def messages(data, field):
    pos = 0
    while pos < len(data):
        key, pos = varint(data, pos)
        wire = key & 7
        if wire == 0:
            _, pos = varint(data, pos)
        elif wire == 1:
            pos += 8
        elif wire == 5:
            pos += 4
        elif wire == 2:
            length, pos = varint(data, pos)
            if key >> 3 == field:
                yield data[pos:pos + length]
            pos += length
        else:
            raise ValueError(f'Unexpected wire type {wire}')


class Receiver(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        global received
        body = self.rfile.read(int(self.headers['Content-Length']))
        if self.path == '/v1/traces':
            # ExportTraceServiceRequest.resource_spans.scope_spans.spans
            received += sum(1 for resource in messages(body, 1)
                            for scope in messages(resource, 2)
                            for _ in messages(scope, 2))
        if receiver_delay:
            time.sleep(receiver_delay)
        self.send_response(200)
        self.send_header('Content-Type', 'application/x-protobuf')
        self.send_header('Content-Length', '0')
        self.end_headers()

    def log_message(self, *args):
        pass


server = http.server.HTTPServer(('127.0.0.1', 0), Receiver)
threading.Thread(target=server.serve_forever, daemon=True).start()
base = {key: value for key, value in os.environ.items()
        if not key.startswith(('GST_', 'OTEL_'))}
base.update(GST_DEBUG='0',
            GST_PYROSCOPE_SERVER_URL='http://127.0.0.1:9',
            OTEL_EXPORTER_OTLP_ENDPOINT=f'http://127.0.0.1:{server.server_port}',
            OTEL_BSP_SCHEDULE_DELAY='10',
            OTEL_BSP_MAX_QUEUE_SIZE='65536')
results = []
configs = ['none', 'sampled', 'unsampled', 'partial', 'slow', 'absent']
if args.baseline:
    configs.insert(1, 'baseline')
try:
    for name in configs:
        count = 10000
        receiver_delay = 0.05 if name == "slow" else 0
        for repeat in range(args.repeats):
            directory = args.baseline.resolve() if name == 'baseline' else ROOT / 'target/release'
            env = dict(base, GST_PLUGIN_PATH=str(directory),
                       GST_REGISTRY=str(OUT / f'registry-{name}.bin'),
                       GST_TRACERS='' if name == 'none' else 'otel-tracer',
                       GST_OTEL_LOGS='false', GST_OTEL_BATCH_SIZE='512',
                       GST_OTEL_EXPORT_INTERVAL_MS='10',
                       GST_OTEL_QUEUE_CAPACITY='128' if name in ('slow', 'absent') else '65536',
                       OTEL_TRACES_SAMPLER='parentbased_traceidratio',
                       OTEL_TRACES_SAMPLER_ARG={'unsampled': '0', 'partial': '0.1'}.get(name, '1'),
                       OTEL_EXPORTER_OTLP_TRACES_TIMEOUT='200')
            if name in ('slow', 'absent'):
                env['GST_OTEL_BATCH_SIZE'] = '64'
            if name == 'absent':
                env['OTEL_EXPORTER_OTLP_ENDPOINT'] = 'http://127.0.0.1:9'
            pipeline = (f'fakesrc name=source num-buffers={count} ! identity ! identity ! identity ! '
                        'identity ! fakesink name=sink sync=false async=false')
            before = received
            process = subprocess.run([str(OUT / 'pipeline'), pipeline], env=env,
                                     capture_output=True, timeout=50)
            output = process.stdout + process.stderr
            (OUT / f'compare-{name}-{count}-{repeat}.log').write_bytes(output)
            match = re.search(rb'RESULT ([^\n]+)', output)
            result = dict(mode=name, buffers=count, repeat=repeat, exit=process.returncode,
                          received_spans=received - before)
            if match:
                result.update({k.decode(): int(v) for k, v in re.findall(rb'(\w+)=(\d+)', match[1])})
            if process.returncode or not match:
                raise RuntimeError(output.decode(errors='replace'))
            counters = re.search(rb'COUNTERS ([^\n]+)', output)
            if counters:
                result['counters'] = {k.decode(): int(v) for k, v in
                                      re.findall(rb'(\w+)=\(guint64\)(\d+)', counters[1])}
                stats = result['counters']
                assert stats['pending_spans'] == 0, result
                assert stats['completed_spans'] == sum(stats[k] for k in
                    ['exported_spans', 'failed_export_spans', 'dropped_queue_full', 'dropped_closed']), result
                assert stats['exported_spans'] == result['received_spans'], result
            if name in ('baseline', 'sampled'):
                assert result['received_spans'] == count * 5, result
            results.append(result)
            filename = 'results.json'
            (OUT / filename).write_text(json.dumps(results, indent=2) + '\n')
            print(json.dumps(result), flush=True)
finally:
    server.shutdown()
