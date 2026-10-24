"""Compare prebuilt plugins and validate actual pprof/OTLP attribution on Linux.

All plugin variants should use the same Rust release/frame-pointer flags.
The local receiver stores payloads during execution and decodes them afterward.
Optional --pyroscope forwards uploads for server-side query validation.
"""
import argparse
import collections
import gzip
import http.server
import json
import os
from pathlib import Path
import re
import statistics
import subprocess
import threading
import time
import urllib.request
from plugins import otel_tracer

ROOT = Path(__file__).resolve().parents[3]


def varint(data, pos=0):
    value, shift = 0, 0
    while True:
        byte = data[pos]
        pos += 1
        value |= (byte & 127) << shift
        if byte < 128:
            return value, pos
        shift += 7


def fields(data):
    result = collections.defaultdict(list)
    pos = 0
    while pos < len(data):
        key, pos = varint(data, pos)
        wire = key & 7
        if wire == 0:
            value, pos = varint(data, pos)
        elif wire in (1, 5):
            size = 8 if wire == 1 else 4
            value = data[pos:pos + size]
            pos += size
        elif wire == 2:
            size, pos = varint(data, pos)
            value = data[pos:pos + size]
            pos += size
        else:
            raise ValueError(f'wire type {wire}')
        result[key >> 3].append(value)
    return result


def packed(data):
    pos = 0
    while pos < len(data):
        value, pos = varint(data, pos)
        yield value


def analyze(payloads, service_name='gst-profile-bench'):
    wanted = collections.Counter()
    cpu_ns = 0
    correlated_ns = 0
    depths = collections.Counter()
    names = collections.Counter()
    profiles = 0
    for path, body in payloads:
        if path == '/v1/traces':
            continue
        assert path == '/push.v1.PusherService/Push', path
        for rawseries in fields(gzip.decompress(body))[1]:
            series = fields(rawseries)
            labels = {fields(l)[1][0].decode(): fields(l)[2][0].decode() for l in series[1]}
            assert labels['service_name'] == service_name, labels
            assert 'span_id' not in labels and 'trace_id' not in labels
            for rawsample in series[2]:
                profile = fields(fields(rawsample)[1][0])
                profiles += 1
                strings = [s.decode() for s in profile[6]]
                functions = {fields(f)[1][0]: strings[fields(f).get(2, [0])[0]] for f in profile[5]}
                locations = {fields(l)[1][0]: functions[fields(fields(l)[4][0])[1][0]] for l in profile[4]}
                for raw in profile[2]:
                    sample = fields(raw)
                    value = list(packed(sample[2][0]))[0]
                    labels = {}
                    for rawlabel in sample[3]:
                        label = fields(rawlabel)
                        labels[strings[label[1][0]]] = strings[label.get(2, [0])[0]]
                    stack = list(packed(sample[1][0]))
                    depths[len(stack)] += value
                    names[locations[stack[0]]] += value
                    cpu_ns += value
                    if 'span_id' in labels:
                        wanted[(labels['trace_id'], labels['span_id'])] += value
                        correlated_ns += value
    received = 0
    matched = set()
    for path, body in payloads:
        if path != '/v1/traces':
            continue
        for resource in fields(body)[1]:
            for scope in fields(resource)[2]:
                for rawspan in fields(scope)[2]:
                    received += 1
                    span = fields(rawspan)
                    key = (span[1][0].hex(), span[2][0].hex())
                    if key in wanted:
                        attrs = {fields(a)[1][0].decode(): fields(fields(a)[2][0]).get(1, [b''])[0] for a in span[9]}
                        assert attrs['pyroscope.profile.id'].decode() == key[1]
                        matched.add(key)
    assert matched == wanted.keys(), ('unmatched profile samples', wanted.keys() - matched)
    return dict(received_spans=received, profiles=profiles, profile_cpu_ns=cpu_ns,
                correlated_cpu_ns=correlated_ns, correlated_spans=len(wanted),
                depth_cpu_ns=dict(depths), leaf_cpu_ns=dict(names.most_common(20)),
                span_cpu_ns=[dict(trace_id=t, span_id=s, cpu_ns=v) for (t, s), v in wanted.items()])


def verify_server(url, rows, output):
    """Check server totals against decoded uploads, including negative selection."""
    spans = [span for row in rows for span in row['span_cpu_ns']]
    assert spans, 'server validation requires at least one correlated sample'
    selected = spans[-1]
    common = dict(start=min(r['started_ms'] for r in rows) - 2000,
                  end=max(r['ended_ms'] for r in rows) + 2000,
                  labelSelector='{service_name="gst-profile-bench"}',
                  profileTypeID='process_cpu:cpu:nanoseconds:cpu:nanoseconds', maxNodes=10000)
    cases = [
        ('all', {}, sum(r['profile_cpu_ns'] for r in rows)),
        ('span', {'spanSelector': [selected['span_id']]},
         sum(s['cpu_ns'] for s in spans if s['span_id'] == selected['span_id'])),
        ('trace', {'traceIdSelector': [selected['trace_id']]},
         sum(s['cpu_ns'] for s in spans if s['trace_id'] == selected['trace_id'])),
        ('missing', {'spanSelector': ['ffffffffffffffff']}, 0),
    ]
    results = {}
    for name, selector, expected in cases:
        body = {**common, **selector}
        request = urllib.request.Request(url.rstrip('/') + '/querier.v1.QuerierService/SelectMergeStacktraces',
            data=json.dumps(body).encode(), headers={'Content-Type': 'application/json'})
        with urllib.request.urlopen(request, timeout=20) as response:
            data = json.load(response)
        actual = int(data.get('flamegraph', {}).get('total', 0))
        assert actual == expected, (name, actual, expected)
        results[name] = dict(request=body, expected_cpu_ns=expected, response=data)
    output.with_suffix('.queries.json').write_text(json.dumps(results, indent=2) + '\n')
    print('SERVER queries matched aggregate, span, trace, and missing-span sample totals.', flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline', type=Path, required=True)
    parser.add_argument('--no-feature', type=Path, required=True)
    parser.add_argument('--profiles', type=Path, required=True)
    parser.add_argument('--previous', type=Path, help='Previous profiles implementation, enabled for comparison')
    parser.add_argument('--output', type=Path, default=ROOT / 'target/profile-evaluation/results.json')
    parser.add_argument('--repeats', type=int, default=5)
    parser.add_argument('--buffers', type=int, default=10000)
    parser.add_argument('--video-buffers', type=int, default=240)
    parser.add_argument('--drain-ms', type=int, default=2000)
    parser.add_argument('--queue-capacity', type=int, default=65536)
    parser.add_argument('--workloads', nargs='+', choices=['push1', 'push4', 'video'], default=['push1', 'push4', 'video'])
    parser.add_argument('--modes', nargs='+', choices=['baseline', 'no-feature', 'disabled', 'enabled', 'previous'], default=['baseline', 'no-feature', 'disabled', 'enabled'])
    parser.add_argument('--cpus', default='0,2', help='Child CPU affinity; record topology before choosing')
    parser.add_argument('--hz', type=int, default=100)
    parser.add_argument('--pyroscope', help='Forward profile uploads to a real server; use separately from timing runs')
    args = parser.parse_args()
    if 'previous' in args.modes and args.previous is None:
        parser.error('--previous is required for mode previous')
    args.output.parent.mkdir(parents=True, exist_ok=True)
    binary = args.output.parent / 'pipeline'
    flags = subprocess.check_output(['pkg-config', '--cflags', '--libs', 'gstreamer-1.0'], text=True).split()
    subprocess.run(['cc', '-O2', '-Wall', '-Wextra', str(Path(__file__).with_name('pipeline.c')), '-o', str(binary), *flags], check=True)
    payloads = []
    forwards = []

    class Receiver(http.server.BaseHTTPRequestHandler):
        protocol_version = 'HTTP/1.1'

        def do_POST(self):
            body = self.rfile.read(int(self.headers['Content-Length']))
            payloads.append((self.path, body))
            if args.pyroscope and self.path != '/v1/traces':
                request = urllib.request.Request(args.pyroscope.rstrip('/') + self.path, data=body,
                    headers={'Content-Type': self.headers['Content-Type'], 'Content-Encoding': self.headers['Content-Encoding']})
                try:
                    with urllib.request.urlopen(request, timeout=15) as response:
                        forwards.append(response.status)
                        response.read()
                except Exception as error:
                    forwards.append(str(error))
                    self.send_response(502)
                    self.send_header('Content-Length', '0')
                    self.end_headers()
                    return
            self.send_response(200)
            self.send_header('Content-Length', '0')
            self.end_headers()

        def log_message(self, *args):
            pass

    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Receiver)
    thread = threading.Thread(target=server.serve_forever)
    thread.start()
    base = {k: v for k, v in os.environ.items() if not k.startswith(('GST_', 'OTEL_', 'MARS_'))}
    endpoint = f'http://127.0.0.1:{server.server_port}'
    results = []
    tracer_names = {}
    try:
        for workload in args.workloads:
            streams = 4 if workload == 'push4' else 1
            if workload == 'video':
                pipeline = (f'videotestsrc name=source0 num-buffers={args.video_buffers} pattern=snow ! '
                            'video/x-raw,format=RGBA,width=1920,height=1080 ! videoconvert ! '
                            'video/x-raw,format=I420 ! fakesink name=sink0 sync=false async=false')
                expected = args.video_buffers * 4
            else:
                assert args.buffers % streams == 0
                pipeline = ' '.join(f'fakesrc name=source{i} num-buffers={args.buffers // streams} ! '
                                    f'identity ! identity ! identity ! identity ! fakesink name=sink{i} sync=false async=false'
                                    for i in range(streams))
                expected = args.buffers * 5
            # Warm registry and code/page caches once per variant, then rotate order.
            for repeat in range(-1, args.repeats):
                offset = max(0, repeat) % len(args.modes)
                for mode in args.modes[offset:] + args.modes[:offset]:
                    directory = {'baseline': args.baseline, 'no-feature': args.no_feature, 'previous': args.previous}.get(mode, args.profiles)
                    env = dict(base, GST_PLUGIN_PATH=str(directory.resolve()), GST_TRACERS='mars-otel',
                               GST_REGISTRY=str(args.output.parent / f'registry-{mode}.bin'), GST_DEBUG='0',
                               OTEL_SERVICE_NAME='gst-profile-bench', OTEL_EXPORTER_OTLP_ENDPOINT=endpoint,
                               GST_OTEL_LOGS='false', GST_OTEL_EXPORT_INTERVAL_MS='10', GST_OTEL_BATCH_SIZE='512',
                               GST_OTEL_QUEUE_CAPACITY=str(args.queue_capacity), OTEL_BENCH_DRAIN_MS=str(args.drain_ms),
                               OTEL_TRACES_SAMPLER='parentbased_always_on')
                    if directory not in tracer_names:
                        tracer_names[directory] = otel_tracer(directory.resolve(), env)
                    env['GST_TRACERS'] = tracer_names[directory]
                    if mode in ('enabled', 'previous'):
                        env.update(GST_OTEL_PROFILE_URL=endpoint, GST_OTEL_PROFILE_HZ=str(args.hz))
                    payloads.clear()
                    forwards.clear()
                    started = int(time.time() * 1000)
                    # Keep time's inherited high-water mark separate from the
                    # driver's post-exec VmHWM (max_rss_kb).
                    process = subprocess.run(['/usr/bin/time', '-f', 'RESOURCE user_s=%U sys_s=%S time_max_rss_kb=%M',
                        'taskset', '-c', args.cpus, str(binary), pipeline, str(streams)],
                        env=env, capture_output=True, text=True, timeout=50)
                    ended = int(time.time() * 1000)
                    output = process.stdout + process.stderr
                    log = args.output.parent / f'{args.output.stem}-{workload}-{mode}-{repeat}.log'
                    log.write_text(output)
                    assert process.returncode == 0, output
                    metrics = {k: int(v) for k, v in re.findall(r'(\w+)=(\d+)', re.search(r'RESULT ([^\n]+)', output)[1])}
                    counters = {k: int(v) for k, v in re.findall(r'(\w+)=\(guint64\)(\d+)', output)}
                    resources = {k: float(v) for k, v in re.findall(r'(\w+)=([\d.]+)', re.search(r'RESOURCE ([^\n]+)', output)[1])}
                    decoded = analyze(payloads)
                    assert decoded['received_spans'] == expected, (mode, workload, decoded['received_spans'], expected, counters)
                    assert counters['pending_spans'] == counters['dropped_queue_full'] == counters['failed_export_spans'] == 0, counters
                    assert all(s == 200 for s in forwards), forwards
                    row = dict(workload=workload, mode=mode, repeat=repeat, cpus=args.cpus, hz=args.hz,
                               buffers=args.video_buffers if workload == 'video' else args.buffers,
                               streams=streams, queue_capacity=args.queue_capacity,
                               started_ms=started, ended_ms=ended, drain_ms=args.drain_ms,
                               **metrics, **resources, **decoded, counters=counters, forwarded=forwards[:])
                    results.append(row)
                    args.output.write_text(json.dumps(results, indent=2) + '\n')
                    print(json.dumps({k: v for k, v in row.items() if k not in ('span_cpu_ns', 'leaf_cpu_ns', 'counters')}), flush=True)
        if args.pyroscope:
            verify_server(args.pyroscope, results, args.output)
        for workload in args.workloads:
            for mode in args.modes:
                rows = [r for r in results if r['workload'] == workload and r['mode'] == mode and r['repeat'] >= 0]
                print('MEDIAN', workload, mode, json.dumps({k: statistics.median(r[k] for r in rows) for k in
                    ('elapsed_ns', 'stream_cpu_ns', 'process_cpu_ns', 'max_rss_kb', 'profile_cpu_ns', 'correlated_cpu_ns')}), flush=True)
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


if __name__ == '__main__':
    main()
