"""Upload and validate attribution off/on profiles for visual review.

Requires a prebuilt profile_elements example and a local Pyroscope server.
The workload deliberately spends 2/6 ms CPU in two identity instances.
"""
import argparse
import collections
import datetime
import gzip
import hashlib
import http.server
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import threading
import time
import urllib.request

sys.dont_write_bytecode = True
from profiles import fields, packed

ROOT = Path(__file__).resolve().parents[3]


def decode(payloads):
    weights = collections.Counter()
    total = 0
    labels = set()
    for body in payloads:
        for series in fields(gzip.decompress(body))[1]:
            for encoded in fields(series)[2]:
                profile = fields(fields(encoded)[1][0])
                strings = [s.decode() for s in profile[6]]
                functions = {fields(f)[1][0]: strings[fields(f).get(2, [0])[0]] for f in profile[5]}
                locations = {fields(l)[1][0]: functions[fields(fields(l)[4][0])[1][0]] for l in profile[4]}
                for encoded_sample in profile[2]:
                    sample = fields(encoded_sample)
                    value = list(packed(sample[2][0]))[0]
                    stack = list(packed(sample[1][0]))
                    assert stack and value > 0
                    labels.update(strings[fields(label)[1][0]] for label in sample[3])
                    weights[locations[stack[-1]]] += value
                    total += value
    assert labels == {'thread_id'}, labels
    return total, dict(weights.most_common())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=ROOT / 'target/profile-video-build/release/examples/profile_elements')
    parser.add_argument('--pyroscope', default='http://localhost:44040')
    parser.add_argument('--frames', type=int, default=1200)
    parser.add_argument('--output', type=Path, default=ROOT / 'target/profile-element-review/results.json')
    args = parser.parse_args()
    assert args.frames >= 600, 'Use at least 600 frames for the sample-ratio check'
    args.output.parent.mkdir(parents=True, exist_ok=True)
    payloads, errors = [], []

    class Receiver(http.server.BaseHTTPRequestHandler):
        def do_POST(self):
            assert self.path == '/push.v1.PusherService/Push', self.path
            body = self.rfile.read(int(self.headers['Content-Length']))
            payloads.append(body)
            try:
                request = urllib.request.Request(args.pyroscope.rstrip('/') + self.path, data=body,
                    headers={key: self.headers[key] for key in ('Content-Type', 'Content-Encoding')})
                with urllib.request.urlopen(request, timeout=15) as response:
                    response.read()
                status = 200
            except Exception as error:
                errors.append(str(error))
                status = 502
            self.send_response(status)
            self.send_header('Content-Length', '0')
            self.end_headers()

        def log_message(self, *args):
            pass

    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Receiver)
    thread = threading.Thread(target=server.serve_forever)
    thread.start()
    results = dict(date=datetime.datetime.now(datetime.timezone.utc).isoformat(),
                   revision=subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
                   binary_sha256=hashlib.sha256(args.binary.read_bytes()).hexdigest(),
                   frames=args.frames, hz=100, modes={})
    try:
        for mode in ('off', 'on'):
            payloads.clear()
            service = f'mars-elements-{mode}'
            env = {k: v for k, v in os.environ.items() if not k.startswith(('GST_', 'OTEL_'))}
            env.update(GST_TRACERS='', GST_PLUGIN_PATH='', GST_DEBUG='0',
                       GST_OTEL_PROFILE_URL=f'http://127.0.0.1:{server.server_port}',
                       GST_OTEL_PROFILE_ELEMENTS='true' if mode == 'on' else 'false',
                       GST_OTEL_PROFILE_HZ='100', OTEL_SERVICE_NAME=service)
            started = time.time_ns() // 1_000_000
            child = subprocess.run([str(args.binary.resolve()), str(args.frames)], env=env,
                                   capture_output=True, text=True, timeout=150)
            ended = time.time_ns() // 1_000_000
            (args.output.parent / f'elements-{mode}.log').write_text(child.stdout + child.stderr)
            assert child.returncode == 0, child.stdout + child.stderr
            assert not errors, errors
            stats = {k: int(v) for k, v in re.findall(r'(\w+): (\d+)', child.stdout)}
            total, groups = decode(payloads)
            assert total == stats['samples'] * 10_000_000
            assert stats['reported_samples'] == stats['samples']
            assert stats['active_threads'] == 0
            if mode == 'on':
                assert sum(v for k, v in groups.items() if k.startswith('[element]')) == stats['element_samples'] * 10_000_000
                assert sum(v for k, v in groups.items() if k.startswith('[task owner]')) == stats['task_samples'] * 10_000_000
                light = sum(v for k, v in groups.items() if k.startswith('[element]') and 'GstIdentity:light' in k)
                heavy = sum(v for k, v in groups.items() if k.startswith('[element]') and 'GstIdentity:heavy' in k)
                assert light > 0 and 2 < heavy / light < 4, groups
                assert any('GstVideoTestSrc:source' in k and k.startswith('[task owner]') for k in groups), groups
            else:
                assert stats['element_samples'] == stats['task_samples'] == 0
                assert not any(k.startswith('[element]') for k in groups)
            # Check actual server-side ingestion, rather than only successful HTTP.
            query = dict(start=started - 2000, end=ended + 2000,
                         labelSelector='{service_name="' + service + '"}',
                         profileTypeID='process_cpu:cpu:nanoseconds:cpu:nanoseconds', maxNodes=16384)
            request = urllib.request.Request(args.pyroscope.rstrip('/') + '/querier.v1.QuerierService/SelectMergeStacktraces',
                data=json.dumps(query).encode(), headers={'Content-Type': 'application/json'})
            with urllib.request.urlopen(request, timeout=30) as response:
                actual = json.load(response)
            assert int(actual['flamegraph']['total']) == total, (actual, total)
            server_names = set(actual['flamegraph']['names'])
            assert set(groups) <= server_names, set(groups) - server_names
            results['modes'][mode] = dict(service=service, started_ms=started, ended_ms=ended,
                total_cpu_ns=total, groups_cpu_ns=groups, stats=stats, server_total_cpu_ns=int(actual['flamegraph']['total']))
            args.output.write_text(json.dumps(results, indent=2) + '\n')
            print(mode, json.dumps(results['modes'][mode]), flush=True)
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


if __name__ == '__main__':
    main()
