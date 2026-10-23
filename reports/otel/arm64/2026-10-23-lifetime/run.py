import collections, csv, http.server, json, os, pathlib, subprocess, threading, time
root = pathlib.Path('/work/target/profile-soak-review')
root.mkdir(exist_ok=True)
counts = collections.Counter()
class Receiver(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'
    def do_POST(self):
        remaining = int(self.headers['Content-Length'])
        total = remaining
        while remaining:
            data = self.rfile.read(min(65536, remaining))
            if not data: raise RuntimeError('short request')
            remaining -= len(data)
        counts[self.path] += 1
        counts['bytes'] += total
        self.send_response(200)
        self.send_header('Content-Length', '0')
        self.end_headers()
    def log_message(self, *_): pass
server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Receiver)
threading.Thread(target=server.serve_forever, daemon=True).start()
endpoint = f'http://127.0.0.1:{server.server_port}'
base = {k:v for k,v in os.environ.items() if not k.startswith(('GST_', 'OTEL_'))}
base.update(GST_PLUGIN_PATH='', GST_TRACERS='', GST_DEBUG_NO_COLOR='1', GST_REGISTRY=str(root/'registry.bin'), OTEL_SERVICE_NAME='mars-profile-soak', OTEL_EXPORTER_OTLP_ENDPOINT=endpoint, OTEL_EXPORTER_OTLP_PROTOCOL='http/protobuf', OTEL_TRACES_SAMPLER='parentbased_always_on', GST_OTEL_PROFILE_HZ='100', GST_OTEL_QUEUE_CAPACITY='8192', GST_OTEL_BATCH_SIZE='512', GST_OTEL_EXPORT_INTERVAL_MS='200', GST_OTEL_PROFILE_ELEMENTS='true')
for mode, seconds in [('none', 30), ('profiles', 90), ('otel', 90)]:
    counts.clear()
    env = dict(base, GST_DEBUG='0,mars-profile-soak:4' if mode=='otel' else '0', GST_OTEL_LOGS='true' if mode=='otel' else 'false')
    if mode!='none': env['GST_OTEL_PROFILE_URL'] = endpoint
    start = time.monotonic()
    print(f'START {mode}', flush=True)
    with (root/f'{mode}.log').open('w') as log, (root/f'{mode}.csv').open('w') as out:
        writer = csv.DictWriter(out, fieldnames=['seconds','rss_kb','anon_kb','peak_kb','threads','fds'])
        writer.writeheader()
        proc = subprocess.Popen(['taskset','-c','0,2','/work/target/profiling/examples/profile_soak', mode,str(seconds)], env=env, stdout=log, stderr=log)
        while proc.poll() is None:
            elapsed = time.monotonic()-start
            if elapsed>seconds*2+90:
                proc.kill(); proc.wait(); raise RuntimeError(f'{mode} watchdog timeout')
            try:
                fields = {s.split(':')[0]: s.split(':',1)[1].strip().split()[0] for s in pathlib.Path(f'/proc/{proc.pid}/status').read_text().splitlines() if ':' in s}
                writer.writerow(dict(seconds=f'{elapsed:.3f}', rss_kb=fields.get('VmRSS',0), anon_kb=fields.get('RssAnon',0), peak_kb=fields.get('VmHWM',0), threads=fields['Threads'], fds=len(list(pathlib.Path(f'/proc/{proc.pid}/fd').iterdir()))))
                out.flush()
            except FileNotFoundError: pass
            time.sleep(0.5)
    result = dict(mode=mode, exit_code=proc.returncode, wall_seconds=time.monotonic()-start, receiver=dict(counts), hz=100, phase_seconds=seconds, churn_pipelines=1500)
    (root/f'{mode}.json').write_text(json.dumps(result,indent=2)+'\n')
    print(json.dumps(result),flush=True)
    if proc.returncode: raise RuntimeError((root/f'{mode}.log').read_text()[-5000:])
server.shutdown()
