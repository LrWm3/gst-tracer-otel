import http.server
import json
import os
from pathlib import Path
import re
import subprocess
import argparse
import time
import threading
from plugins import otel_tracer

ROOT = Path(__file__).resolve().parents[3]
OUT = ROOT / 'target/otel-bench'
OUT.mkdir(parents=True, exist_ok=True)
parser = argparse.ArgumentParser(description='Build the plugin first; optionally compare a prebuilt baseline.')
parser.add_argument('--baseline', type=Path, help='Directory containing a prebuilt comparison plugin')
parser.add_argument('--repeats', type=int, default=3)
parser.add_argument('--streams', type=int, nargs='+', default=[1])
parser.add_argument('--buffers', type=int, default=10000, help='Total buffers shared between streams')
parser.add_argument('--modes', nargs='+', choices=['none', 'hooks', 'sampled', 'unsampled', 'partial', 'slow', 'absent'], default=['none', 'sampled', 'unsampled', 'partial', 'slow', 'absent'], help='hooks runs benchmark-only empty push hooks')
parser.add_argument('--baseline-sampling', action='store_true', help='Compare sampling modes too, for baselines that support them')
parser.add_argument('--baseline-disabled', action='store_true', help='Also run a tracing-disabled control with the baseline plugin directory')
parser.add_argument('--baseline-hooks', action='store_true', help='Also compare empty hooks, for private GStreamer build comparisons')
parser.add_argument('--gst-library-path', type=Path, help='Private directory containing libgstreamer-1.0.so.0 for candidate runs')
parser.add_argument('--baseline-gst-library-path', type=Path, help='Private libgstreamer directory for baseline runs')
parser.add_argument('--drain-ms', type=int, default=2000, help='Use 0 when both plugins flush on pipeline NULL')
parser.add_argument('--output', default='results.json')
parser.add_argument('--batch-size', type=int, default=512)
parser.add_argument('--http-client', choices=['blocking', 'async'], default='blocking')
parser.add_argument('--keep-alive', action='store_true', help='Use a persistent HTTP/1.1 collector connection')
parser.add_argument('--churn-every', type=int, default=0, help='Rename an unrelated element every N source buffers')
parser.add_argument('--tee-queues', action='store_true', help='Use tee and queue stages to exercise shared buffers')
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
    protocol_version = 'HTTP/1.1' if args.keep_alive else 'HTTP/1.0'
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
        if not key.startswith(('GST_', 'OTEL_', 'MARS_'))}
base.update(GST_DEBUG='0',
            GST_PYROSCOPE_SERVER_URL='http://127.0.0.1:9',
            OTEL_EXPORTER_OTLP_ENDPOINT=f'http://127.0.0.1:{server.server_port}',
            OTEL_BSP_SCHEDULE_DELAY='10',
            OTEL_BSP_MAX_QUEUE_SIZE='65536')
results = []
tracer_names = {}
configs = list(args.modes)
if args.baseline:
    if args.baseline_hooks and 'hooks' in configs:
        configs.insert(configs.index('hooks'), 'baseline-hooks')
    if args.baseline_disabled and 'none' in configs:
        configs.insert(configs.index('none'), 'baseline-none')
    if 'sampled' in configs:
        configs.insert(configs.index('sampled'), 'baseline')
    if args.baseline_sampling:
        for mode in ['unsampled', 'partial']:
            if mode in configs:
                configs.insert(configs.index(mode), 'baseline-' + mode)
try:
    for streams in args.streams:
        assert 1 <= streams <= 64
        assert args.buffers % streams == 0
        count = args.buffers // streams
        for repeat in range(args.repeats):
            # Rotate variants to reduce systematic ordering/temperature bias.
            offset = repeat % len(configs)
            for name in configs[offset:] + configs[:offset]:
                baseline = name.startswith('baseline')
                mode = name.removeprefix('baseline-') if name != 'baseline' else 'sampled'
                receiver_delay = 0.05 if mode == 'slow' else 0
                directory = args.baseline.resolve() if baseline else ROOT / 'target/release'
                env = dict(base, GST_PLUGIN_PATH=str(directory),
                           GST_REGISTRY=str(OUT / f'registry-{name}.bin'),
                           GST_TRACERS='' if mode in ('none', 'hooks') else 'mars-otel',
                           OTEL_BENCH_EMPTY_HOOKS='1' if mode == 'hooks' else '0',
                           GST_OTEL_LOGS='false', GST_OTEL_BATCH_SIZE=str(args.batch_size),
                           GST_OTEL_HTTP_CLIENT=args.http_client,
                           GST_OTEL_EXPORT_INTERVAL_MS='10',
                           GST_OTEL_QUEUE_CAPACITY='128' if mode in ('slow', 'absent') else '65536',
                           OTEL_TRACES_SAMPLER='parentbased_traceidratio',
                           OTEL_TRACES_SAMPLER_ARG={'unsampled': '0', 'partial': '0.1'}.get(mode, '1'),
                           OTEL_BENCH_DRAIN_MS=str(args.drain_ms),
                           OTEL_BENCH_CHURN_EVERY=str(args.churn_every),
                           OTEL_EXPORTER_OTLP_TRACES_TIMEOUT='200')
                libraries = args.baseline_gst_library_path if baseline else args.gst_library_path
                if libraries:
                    env['LD_LIBRARY_PATH'] = str(libraries.resolve()) + (
                        os.pathsep + base['LD_LIBRARY_PATH'] if base.get('LD_LIBRARY_PATH') else '')
                if env['GST_TRACERS']:
                    key = (directory, libraries)
                    if key not in tracer_names:
                        tracer_names[key] = otel_tracer(directory, env)
                    env['GST_TRACERS'] = tracer_names[key]
                if mode in ('slow', 'absent'):
                    env['GST_OTEL_BATCH_SIZE'] = '64'
                if mode == 'absent':
                    env['OTEL_EXPORTER_OTLP_ENDPOINT'] = 'http://127.0.0.1:9'
                pipeline = ' '.join(
                    (f'fakesrc name=source{i} num-buffers={count} ! tee name=split{i} '
                     f'split{i}. ! queue ! fakesink name=sink{i} sync=false async=false '
                     f'split{i}. ! queue ! fakesink sync=false async=false') if args.tee_queues else
                    (f'fakesrc name=source{i} num-buffers={count} ! identity ! identity ! identity ! '
                     f'identity ! fakesink name=sink{i} sync=false async=false')
                    for i in range(streams))
                before = received
                process = subprocess.run([str(OUT / 'pipeline'), pipeline, str(streams)], env=env,
                                         capture_output=True, timeout=50)
                output = process.stdout + process.stderr
                (OUT / f'compare-{name}-{streams}-{count}-{repeat}.log').write_bytes(output)
                match = re.search(rb'RESULT ([^\n]+)', output)
                result = dict(mode=name, streams=streams, buffers=args.buffers, repeat=repeat,
                              batch_size=int(env['GST_OTEL_BATCH_SIZE']),
                              http_client=args.http_client,
                              keep_alive=args.keep_alive,
                              churn_every=args.churn_every, tee_queues=args.tee_queues,
                              gst_library_path=str(libraries.resolve()) if libraries else 'default',
                              exit=process.returncode, received_spans=received - before)
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
                    if args.tee_queues:
                        assert stats['propagation_fallbacks'] > 0, result
                if mode == 'sampled':
                    assert result['received_spans'] == args.buffers * 5, result
                if mode in ('none', 'hooks', 'unsampled'):
                    assert result['received_spans'] == 0, result
                results.append(result)
                (OUT / args.output).write_text(json.dumps(results, indent=2) + '\n')
                print(json.dumps(result), flush=True)
finally:
    server.shutdown()
