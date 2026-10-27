"""Alternate two prebuilt profile_video binaries; validate every captured upload.

Build both revisions with identical release flags and the profiles feature.
Uses 100 Hz function symbolization, element attribution and monitoring=none.
No compilation or backend decoding occurs during the timed child process.
"""
import argparse
import collections
import csv
import hashlib
import http.server
import json
import os
from pathlib import Path
import re
import statistics
import subprocess
import sys
import threading
import time

sys.dont_write_bytecode = True
from profile_video import validate


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline', type=Path, required=True)
    parser.add_argument('--candidate', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--frames', type=int, default=600)
    parser.add_argument('--repeats', type=int, default=5)
    parser.add_argument('--cpus', default='0,2')
    args = parser.parse_args()
    assert args.frames > 0 and args.repeats > 0
    args.output.mkdir(parents=True, exist_ok=False)
    binaries = {name: getattr(args, name).resolve() for name in ('baseline', 'candidate')}
    metadata = dict(
        command=sys.argv, started_ns=time.time_ns(), cpus=args.cpus,
        frames=args.frames, repeats=args.repeats, hz=100,
        symbolization='functions', monitoring='none', element_attribution=True,
        binaries={name: dict(path=str(path), sha256=hashlib.sha256(path.read_bytes()).hexdigest())
                  for name, path in binaries.items()},
        source_commit=subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
        source_diff_sha256=hashlib.sha256(subprocess.check_output(['git', 'diff'])).hexdigest(),
    )
    (args.output / 'metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')
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
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    env = {k: v for k, v in os.environ.items() if not k.startswith(('GST_', 'OTEL_', 'MARS_'))}
    env.update(GST_TRACERS='', GST_DEBUG='0', GST_PLUGIN_PATH='',
               OTEL_SERVICE_NAME='gst-profile-bench', MARS_PROFILE_HZ='100',
               MARS_PROFILE_URL=f'http://127.0.0.1:{server.server_port}',
               MARS_PROFILE_SYMBOLIZATION='functions', MARS_MONITORING='none')
    rows = []
    cases = [(build, mode) for mode in ('none', 'profiles') for build in binaries]
    try:
        for repeat in range(-1, args.repeats):
            order = cases if repeat % 2 else list(reversed(cases))
            for build, mode in order:
                stem = f'{repeat}-{build}-{mode}'
                perf = args.output / f'{stem}.perf'
                payloads.clear()
                result = subprocess.run(
                    ['taskset', '-c', args.cpus, 'perf', 'stat', '-x', ';', '-e',
                     'cycles,instructions', '-o', str(perf), '--', str(binaries[build]),
                     mode, str(args.frames)], env=env, capture_output=True, text=True, timeout=150)
                output = result.stdout + result.stderr
                (args.output / f'{stem}.log').write_text(output)
                assert result.returncode == 0, output
                values = {k: int(v) for k, v in re.findall(
                    r'(\w+)=(\d+)', re.search(r'^RESULT (.+)$', output, re.M)[1])}
                counters = {prefix: {k: int(v) for k, v in re.findall(
                    r'(\w+): (\d+)', re.search(rf'^{prefix} .+$', output, re.M)[0])}
                    for prefix in ('TRACE', 'PROFILE', 'METRIC')}
                decoded = validate(payloads, mode, args.frames, 100,
                                   counters['PROFILE'], counters['TRACE'], counters['METRIC'])
                hardware = collections.Counter()
                for line in perf.read_text().splitlines():
                    fields = line.split(';')
                    if len(fields) < 5 or not fields[0].strip().isdigit():
                        continue
                    assert float(fields[4]) >= 99, line
                    for event in ('cycles', 'instructions'):
                        if event in fields[2]:
                            hardware[event] += int(fields[0])
                assert hardware['cycles'] > 0 and hardware['instructions'] > 0
                row = dict(repeat=repeat, build=build, mode=mode, **values, **hardware,
                           samples=counters['PROFILE']['samples'],
                           timer_overruns=counters['PROFILE']['timer_overruns'],
                           profiles=decoded['profiles'])
                rows.append(row)
                with (args.output / 'results.csv').open('w', newline='') as stream:
                    writer = csv.DictWriter(stream, fieldnames=list(row))
                    writer.writeheader()
                    writer.writerows(rows)
                print(json.dumps(row), flush=True)
        for build, mode in cases:
            measured = [r for r in rows if r['repeat'] >= 0 and r['build'] == build and r['mode'] == mode]
            print('MEDIAN', build, mode, json.dumps({key: statistics.median(r[key] for r in measured)
                  for key in ('cycles', 'instructions', 'total_cpu_ns', 'elapsed_ns', 'max_rss_kb')}), flush=True)
    finally:
        server.shutdown()
        server.server_close()


if __name__ == '__main__':
    main()
