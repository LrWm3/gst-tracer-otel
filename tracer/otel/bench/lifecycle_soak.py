"""Exercise recreation, native thread retirement, loader locks, and failed delivery.

Build lifecycle_soak with --release --features profiles and frame pointers.
The receiver stores bounded counters, never the complete stream of payloads.
"""
import argparse
import collections
import gzip
import hashlib
import http.server
import json
import os
from pathlib import Path
import platform
import re
import signal
import subprocess
import sys
import threading
import time

sys.dont_write_bytecode = True
from profiles import fields, packed

ROOT = Path(__file__).resolve().parents[3]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--seconds', type=int, default=1800)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    binary = ROOT/'target/release/examples/lifecycle_soak'
    module = (args.output/'soak-module.so').resolve()
    subprocess.run(['cc', '-shared', '-fPIC', '-O2', '-fno-omit-frame-pointer', str(Path(__file__).with_name('soak_module.c')), '-o', str(module)], check=True)
    counts = collections.Counter()
    phase_counts = collections.Counter()
    errors = []
    lock = threading.Lock()
    start = time.monotonic()
    period = min(120, max(6, args.seconds/3))
    snapshots = []
    current = None

    def phase():
        elapsed = time.monotonic()-start
        if elapsed >= args.seconds-5:
            return 'healthy'
        offset = elapsed % period
        if period/6 <= offset < period/3:
            return 'reject'
        if period/2 <= offset < period*0.6:
            return 'slow'
        return 'healthy'

    class Receiver(http.server.BaseHTTPRequestHandler):
        protocol_version = 'HTTP/1.1'
        def do_POST(self):
            body = self.rfile.read(int(self.headers['Content-Length']))
            disposition = phase()
            try:
                if self.path in ('/v1/traces', '/v1/logs'):
                    units = sum(len(fields(scope)[2]) for resource in fields(body)[1] for scope in fields(resource)[2])
                elif self.path == '/v1/metrics':
                    units = 1
                    assert fields(body)[1]
                else:
                    assert self.path == '/push.v1.PusherService/Push'
                    units = 0
                    for series in fields(gzip.decompress(body))[1]:
                        for raw in fields(series)[2]:
                            profile = fields(fields(raw)[1][0])
                            if any(b'mars_soak_module_work' in name for name in profile[6]):
                                with lock:
                                    counts['profiles_with_loader_fixture'] += 1
                            units += sum(list(packed(fields(sample)[2][0]))[0] for sample in profile[2])
                if disposition == 'slow' and self.path != '/push.v1.PusherService/Push':
                    time.sleep(0.35)  # OTLP request timeout is 100 ms.
                code = 503 if disposition == 'reject' else 200
                self.send_response(code)
                self.send_header('Content-Length', '0')
                self.end_headers()
                with lock:
                    counts[f'{self.path}:status{code}:units'] += units
                    counts[f'{self.path}:status{code}:requests'] += 1
                    phase_counts[disposition] += 1
            except (BrokenPipeError, ConnectionResetError):
                with lock:
                    counts[self.path+':disconnected'] += 1
            except Exception as error:
                with lock:
                    errors.append(repr(error))
                self.close_connection = True
        def log_message(self, *_):
            pass
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Receiver)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    endpoint = f'http://127.0.0.1:{server.server_port}'
    env = {k:v for k,v in os.environ.items() if not k.startswith(('GST_', 'OTEL_', 'MARS_'))}
    env.update(GST_PLUGIN_PATH='', GST_TRACERS='', MARS_SIGNALS='metrics,traces,profiles,logs',
               MARS_MONITORING='all', MARS_PROFILE_URL=endpoint, MARS_PROFILE_HZ='100',
               MARS_METRIC_INTERVAL_MS='1000', MARS_METRIC_SAMPLE_RATE='16',
               OTEL_SERVICE_NAME='mars-lifecycle-soak', OTEL_EXPORTER_OTLP_ENDPOINT=endpoint,
               OTEL_EXPORTER_OTLP_TIMEOUT='100', GST_OTEL_FLUSH_TIMEOUT_MS='1000',
               GST_OTEL_QUEUE_CAPACITY='256', GST_OTEL_BATCH_SIZE='64',
               OTEL_BLRP_MAX_QUEUE_SIZE='256', OTEL_BLRP_MAX_EXPORT_BATCH_SIZE='64',
               OTEL_BLRP_SCHEDULE_DELAY='200', GST_DEBUG='0,mars-soak-work:4', GST_DEBUG_NO_COLOR='1')
    metadata = dict(architecture=platform.machine(), duration_seconds=args.seconds,
                    binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
                    gstreamer=subprocess.check_output(['pkg-config', '--modversion', 'gstreamer-1.0'], text=True).strip(),
                    environment={k:v for k,v in env.items() if k.startswith(('GST_', 'OTEL_', 'MARS_'))},
                    phase_period_seconds=period, request_delay_ms=350, timeout_ms=100,
                    pipeline_buffers=4, producer_sleep_ms=80, native_producer_threads='one new thread per cycle',
                    module_loads='one constructor/destructor pair per cycle, each doing 15 ms thread CPU under loader lock',
                    final_shutdown_callers=4, subsequent_shutdown_calls=100)
    (args.output/'metadata.json').write_text(json.dumps(metadata,indent=2)+'\n')
    with (args.output/'stderr.log').open('w') as stderr, (args.output/'stdout.log').open('w') as stdout:
        process = subprocess.Popen([str(binary),str(args.seconds),str(module)], env=env, stdout=subprocess.PIPE, stderr=stderr, text=True, start_new_session=True)
        def kill():
            try: os.killpg(process.pid,signal.SIGKILL)
            except ProcessLookupError: pass
        timer = threading.Timer(args.seconds+90,kill)
        timer.start()
        try:
            for line in process.stdout:
                stdout.write(line)
                stdout.flush()
                if line.startswith('STATE '):
                    current = {k:int(v) if v.isdigit() else v for k,v in re.findall(r'(\w+)=(\S+)',line)}
                    snapshots.append(current)
                    print(line.strip(),flush=True)
                elif line.startswith(('TRACE ', 'PROFILE ', 'METRIC ', 'LOG ')):
                    kind = line.split()[0].lower()
                    current[kind] = {k:int(v) for k,v in re.findall(r'(\w+): (\d+)',line)}
                (args.output/'snapshots.json').write_text(json.dumps(snapshots,indent=2)+'\n')
            assert process.wait(timeout=5) == 0, 'soak failed; see stdout.log/stderr.log'
        finally:
            timer.cancel()
            if process.poll() is None:
                kill()
                process.wait()
            server.shutdown()
            server.server_close()
    assert not errors, errors
    assert snapshots[-1]['phase']=='shutdown'
    final = snapshots[-1]
    for kind in ('trace','log'):
        stats = final[kind]
        pending = 'pending_spans' if kind=='trace' else 'pending'
        assert stats[pending]==0
    trace = final['trace']
    assert trace['started_spans']==trace['completed_spans']==final['cycles']*12
    assert trace['completed_spans']==sum(trace[key] for key in ('exported_spans','failed_export_spans','dropped_queue_full','dropped_closed'))
    assert final['log']['exported']+final['log']['failed']+final['log']['dropped_queue_full']+final['log']['dropped_closed']==final['cycles']*4
    assert final['profile']['context_descriptions']==0
    assert final['profile']['samples']==final['profile']['reported_samples']
    assert final['profile']['dropped_full']+final['profile']['dropped_processing']+final['profile']['dropped_upload']==0
    if args.seconds >= 30:
        assert trace['failed_export_spans']>0 and trace['exported_spans']>0
        assert final['log']['failed']>0 and final['log']['exported']>0
        assert final['metric']['failed_exports']>0 and final['metric']['exported_batches']>0
        assert phase_counts['reject']>0 and phase_counts['slow']>0
        assert counts['profiles_with_loader_fixture']>0, 'loader-lock fixture was not observed in actual profiles'
        # Cumulative accepted counts must continue increasing after the first
        # rejection; failure history is intentionally retained in drain results.
        failed = [s for s in snapshots if s['trace']['failed_export_spans']>0][0]
        assert trace['exported_spans']>failed['trace']['exported_spans']
    result = dict(received=dict(counts), phase_requests=dict(phase_counts), snapshots=len(snapshots), final=final)
    (args.output/'result.json').write_text(json.dumps(result,indent=2)+'\n')
    print('PASS',json.dumps(dict(cycles=final['cycles'],samples=final['profile']['samples'],elapsed_ms=final['elapsed_ms'])),flush=True)


if __name__=='__main__':
    main()
