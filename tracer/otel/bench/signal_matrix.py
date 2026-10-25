"""Matched full-metrics and compatibility video runs, including self monitoring.

Build profile_video and the three plugins with frame pointers first. This runner
never compiles while measuring. Receivers and decoding run outside the child.
"""
import argparse
import collections
import csv
import hashlib
import http.server
import json
import os
from pathlib import Path
import platform
import re
import signal
import socket
import statistics
import struct
import subprocess
import sys
import threading
import time
import urllib.request

sys.dont_write_bytecode = True
from profile_video import validate
from profiles import analyze, fields

ROOT = Path(__file__).resolve().parents[3]


def full_metrics(payloads, spans, expected, level):
    latest = {}
    examples = set()
    names = set()
    histograms = {}
    for path, body in payloads:
        if path != '/v1/metrics':
            continue
        for resource in fields(body)[1]:
            for scope in fields(resource)[2]:
                for raw in fields(scope)[2]:
                    metric = fields(raw)
                    name = metric[1][0].decode()
                    names.add(name)
                    if name == 'gst_element_latency_count_count':
                        for raw_point in fields(metric[7][0])[1]:
                            point = fields(raw_point)
                            latest[tuple(sorted(point[7]))] = int(struct.unpack('<d', point[4][0])[0])
                    if 9 in metric:
                        for raw_point in fields(metric[9][0])[1]:
                            point = fields(raw_point)
                            buckets = b''.join(point[6])
                            count = int.from_bytes(point[4][0], 'little') if point[4] else 0
                            assert sum(int.from_bytes(buckets[i:i+8], 'little') for i in range(0, len(buckets), 8)) == count
                            if not name.startswith('mars_instrument_'):
                                histograms[name, tuple(sorted(point[9]))] = count
                            for raw_example in point[8]:
                                example = fields(raw_example)
                                key = (example[5][0], example[4][0])
                                assert key in spans, 'full-metrics exemplar points to an absent span'
                                examples.add(key)
    assert sum(latest.values()) == expected, (latest, expected)
    assert len(latest) == 4 and all(value == expected // 4 for value in latest.values())
    assert bool(examples) == bool(spans)
    assert any(name.startswith('mars_instrument_') for name in names) == (level != 'none')
    assert ('mars_instrument_work_duration_seconds' in names) == (level == 'all'), names
    profile = analyze([(p,b) for p,b in payloads if p not in ('/v1/metrics', '/v1/logs')])
    profiled = {(bytes.fromhex(s['trace_id']), bytes.fromhex(s['span_id'])) for s in profile['span_cpu_ns']}
    return dict(metric_exemplars=len(examples), metric_histograms=len(histograms),
                metric_cpu_observations=sum(count for (name,_),count in histograms.items() if name=='gst_element_sync_cpu_seconds'),
                profiled_exemplar_spans=len(examples & profiled))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--frames', type=int, default=600)
    parser.add_argument('--repeats', type=int, default=5)
    parser.add_argument('--cpus', default='0,2')
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--perf', action='store_true')
    parser.add_argument('--cases', nargs='+', help='Optional subset for a smoke run')
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    binary = ROOT / 'target/release/examples/profile_video'
    cases = [('none', 'none', '', 'none'), ('noop', 'none', '', 'none'),
             ('standalone-metrics', 'none', 'metrics', 'core'),
             ('otel', 'otel', 'traces', 'none'), ('profiles', 'profiles', 'profiles', 'none'),
             ('legacy-instrument', 'instrument', 'metrics,traces', 'core'),
             ('otel-profiles-logs', 'otel', 'traces,profiles,logs', 'none')]
    for name, signals in [('full-metrics', 'metrics'), ('full-default', 'metrics,traces'),
                          ('full-all', 'metrics,traces,profiles,logs')]:
        cases.extend((name+'-'+level, 'instrument', signals, level) for level in ('none', 'core', 'all'))
    cases.extend([('full-logs-core', 'instrument', 'metrics,traces,logs', 'core'),
                  ('full-profiles-core', 'instrument', 'metrics,traces,profiles', 'core')])
    if args.cases:
        assert set(args.cases) <= {c[0] for c in cases}
        cases = [c for c in cases if c[0] in args.cases]
    metadata = dict(architecture=platform.machine(), gstreamer=subprocess.check_output(['pkg-config', '--modversion', 'gstreamer-1.0'], text=True).strip(),
                    revision=subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip(),
                    source_diff_sha256=hashlib.sha256(subprocess.check_output(['git', 'diff'], cwd=ROOT)).hexdigest(),
                    binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
                    plugin_sha256={p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in (ROOT/'target/release').glob('libgst*tracer.so')},
                    frames=args.frames, repeats=args.repeats, cpus=args.cpus, cases=cases, warmups_per_case=1,
                    profile_hz=100, metric_interval_ms=1000, metric_sample_rate=16,
                    trace_queue=8192, trace_batch=512, trace_interval_ms=200, trace_ratio=1,
                    rustflags='-C force-frame-pointers=yes', perf=args.perf,
                    steady_rss_definition='VmRSS at EOS, before final drain; this is not a long-running memory plateau')
    (args.output/'metadata.json').write_text(json.dumps(metadata, indent=2)+'\n')
    payloads = []
    class Receiver(http.server.BaseHTTPRequestHandler):
        protocol_version = 'HTTP/1.1'
        def do_POST(self):
            payloads.append((self.path, self.rfile.read(int(self.headers['Content-Length']))))
            self.send_response(200)
            self.send_header('Content-Length', '0')
            self.end_headers()
        def log_message(self, *_):
            pass
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Receiver)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    endpoint = f'http://127.0.0.1:{server.server_port}'
    base = {k:v for k,v in os.environ.items() if not k.startswith(('GST_', 'OTEL_', 'MARS_'))}
    base.update(GST_PLUGIN_PATH=str(ROOT/'target/release'), GST_TRACERS='', GST_DEBUG_NO_COLOR='1',
                GST_REGISTRY=str((args.output/'registry.bin').resolve()),
                OTEL_EXPORTER_OTLP_ENDPOINT=endpoint, OTEL_SERVICE_NAME='gst-profile-bench',
                MARS_METRIC_INTERVAL_MS='1000', GST_INSTRUMENT_METRIC_INTERVAL_MS='1000',
                MARS_METRIC_SAMPLE_RATE='16', GST_INSTRUMENT_CPU_SAMPLE_RATE='16',
                MARS_BENCH_SYNC_EOS='1', MARS_PROFILE_HZ='100')
    rows = []
    try:
        for repeat in range(-1, args.repeats):
            offset = max(0, repeat) % len(cases)
            for name, driver, signals, level in cases[offset:]+cases[:offset]:
                env = dict(base, MARS_MONITORING=level, GST_DEBUG='0,mars-profile-bench:4' if 'logs' in signals else '0',
                           GST_OTEL_LOGS='true' if 'logs' in signals else 'false')
                if name.startswith('full-'):
                    env['MARS_SIGNALS'] = signals
                if 'profiles' in signals:
                    env['MARS_PROFILE_URL'] = endpoint
                if name == 'noop':
                    env['GST_TRACERS'] = 'mars-noop'
                if name == 'standalone-metrics':
                    with socket.socket() as reserve:
                        reserve.bind(('127.0.0.1', 0))
                        port = reserve.getsockname()[1]
                    env['GST_TRACERS'] = f'mars-metrics(port={port},monitoring={level})'
                payloads.clear()
                prefix = args.output/f'{name}-{repeat}'
                command = ['taskset', '-c', args.cpus]
                if args.perf:
                    command += ['perf', 'stat', '-x', ';', '-e', 'cycles,instructions', '-o', str(prefix.with_suffix('.perf')), '--']
                command += [str(binary), driver, str(args.frames)]
                with prefix.with_suffix('.stderr').open('w') as stderr:
                    process = subprocess.Popen(command, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr, text=True, start_new_session=True)
                    def kill_child():
                        try:
                            os.killpg(process.pid, signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                    timer = threading.Timer(150, kill_child)
                    timer.start()
                    try:
                        lines = []
                        for line in process.stdout:
                            lines.append(line)
                            if line.strip() == 'EOS':
                                if name == 'standalone-metrics':
                                    with urllib.request.urlopen(f'http://127.0.0.1:{port}/metrics', timeout=2) as response:
                                        scrape = response.read().decode()
                                    prefix.with_suffix('.prom').write_text(scrape)
                                    values = re.findall(r'^gst_element_latency_count_count\{[^\n]+\} (\d+)$', scrape, re.M)
                                    assert len(values) == 4 and sum(map(int, values)) == args.frames*4, values
                                process.stdin.write('x')
                                process.stdin.flush()
                        assert process.wait(timeout=5) == 0, prefix.with_suffix('.stderr').read_text()
                    finally:
                        timer.cancel()
                        if process.poll() is None:
                            kill_child()
                            process.wait()
                output = ''.join(lines)
                prefix.with_suffix('.stdout').write_text(output)
                values = {k:int(v) for k,v in re.findall(r'(\w+)=(\d+)', re.search(r'^RESULT (.+)$', output, re.M)[1])}
                counters = {signal:{k:int(v) for k,v in re.findall(r'(\w+): (\d+)', re.search(rf'^{signal} .+$', output, re.M)[0])}
                            for signal in ('TRACE', 'PROFILE', 'METRIC', 'LOG')}
                # Reuse full span/log/pprof validation; validate the richer metric
                # snapshot separately instead of pretending it is four histograms.
                validation_mode = ('otel' if 'traces' in signals else 'none')
                if 'profiles' in signals:
                    validation_mode = 'profiles' if validation_mode == 'none' else validation_mode+'-profiles'
                if 'logs' in signals:
                    validation_mode += '-logs'
                if name == 'legacy-instrument':
                    validation_mode = 'instrument'
                checked = payloads if name == 'legacy-instrument' else [(p,b) for p,b in payloads if p != '/v1/metrics']
                decoded = validate(checked, validation_mode, args.frames, 100, counters['PROFILE'], counters['TRACE'], counters['METRIC'] if name == 'legacy-instrument' else dict(recorded_pushes=0, exported_batches=0))
                if name.startswith('full-'):
                    spans = {(fields(span)[1][0], fields(span)[2][0]) for path,body in payloads if path == '/v1/traces' for resource in fields(body)[1] for scope in fields(resource)[2] for span in fields(scope)[2]}
                    decoded.update(full_metrics(payloads, spans, args.frames*4, level))
                    decoded['metric_pushes'] = args.frames*4
                    assert counters['METRIC']['recorded_pushes'] == args.frames*4
                    assert counters['METRIC']['exported_batches'] == sum(p == '/v1/metrics' for p,_ in payloads)
                elif name == 'standalone-metrics':
                    decoded['metric_pushes'] = args.frames*4
                assert counters['LOG']['exported'] == decoded['received_logs'], counters['LOG']
                assert sum(counters['LOG'][key] for key in ('failed', 'rejected', 'dropped_queue_full', 'dropped_closed', 'pending')) == 0
                assert counters['PROFILE']['context_capacity_rejections'] == 0
                row = dict(mode=name, monitoring=level, repeat=repeat, **values, **decoded,
                           **{signal.lower()+'_'+k:v for signal,counts in counters.items() for k,v in counts.items()})
                row['http_requests'] = len(payloads)
                row['http_payload_bytes'] = sum(len(body) for _,body in payloads)
                if args.perf:
                    hardware = collections.Counter()
                    for line in prefix.with_suffix('.perf').read_text().splitlines():
                        columns = line.split(';')
                        if len(columns) < 5 or not columns[0].strip().isdigit():
                            continue
                        for event in ('cycles', 'instructions'):
                            if event in columns[2]:
                                assert float(columns[4]) >= 99, line
                                hardware[event] += int(columns[0])
                    assert hardware['cycles'] > 0 and hardware['instructions'] > 0
                    row.update(hardware)
                rows.append(row)
                with (args.output/'results.csv').open('w') as stream:
                    writer = csv.DictWriter(stream, fieldnames=list(row))
                    writer.writeheader()
                    writer.writerows(rows)
                print(f'{repeat:2} {name:27} CPU={values["total_cpu_ns"]/1e9:.3f}s RSS={values["steady_rss_kb"]/1024:.1f}/{values["max_rss_kb"]/1024:.1f} MiB drain={values["shutdown_ns"]/1e6:.1f}ms', flush=True)
        for name, *_ in cases:
            selected = [row for row in rows if row['mode']==name and row['repeat']>=0]
            print('MEDIAN', name, json.dumps({k:statistics.median(row[k] for row in selected) for k in ('total_cpu_ns', 'elapsed_ns', 'steady_rss_kb', 'max_rss_kb', 'shutdown_ns')}), flush=True)
    finally:
        server.shutdown()
        server.server_close()
        (args.output/'registry.bin').unlink(missing_ok=True)


if __name__ == '__main__':
    main()
