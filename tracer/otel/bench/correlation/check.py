"""Verify provisioned Grafana, Tempo and Pyroscope using real tracer exports.

Start compose.yaml and build the profile_correlation example first. This checks
Grafana's configuration and proxy APIs; it does not automate browser clicks.
"""
import argparse
import base64
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
import urllib.error
import urllib.parse
import urllib.request

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from profiles import analyze

ROOT = Path(__file__).resolve().parents[4]
TEMPO_UID = 'mars-correlation-tempo'
PROFILE_UID = 'mars-correlation-pyroscope'
PROFILE_TYPE = 'process_cpu:cpu:nanoseconds:cpu:nanoseconds'


def request(url, data=None, headers=None):
    with urllib.request.urlopen(urllib.request.Request(url, data=data, headers=headers or {}), timeout=15) as response:
        return response.read()


def get_json(url, body=None):
    return json.loads(request(url, None if body is None else json.dumps(body).encode(),
                              {'Accept': 'application/json', 'Content-Type': 'application/json'}))


def eventually(action, description):
    deadline = time.monotonic() + 60
    while True:
        try:
            return action()
        except (urllib.error.URLError, OSError, AssertionError) as error:
            if time.monotonic() >= deadline:
                raise RuntimeError(f'{description}: {error}') from error
            time.sleep(1)


def objects(value):
    if isinstance(value, dict):
        yield value
        for child in value.values():
            yield from objects(child)
    elif isinstance(value, list):
        for child in value:
            yield from objects(child)


def identifier(value, length):
    if re.fullmatch('[0-9a-fA-F]{' + str(length * 2) + '}', value):
        return value.lower()
    decoded = base64.b64decode(value, validate=True)
    assert len(decoded) == length
    return decoded.hex()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=ROOT / 'target/release/examples/profile_correlation')
    parser.add_argument('--grafana', default='http://127.0.0.1:43002')
    parser.add_argument('--tempo-otlp', default='http://127.0.0.1:44318')
    parser.add_argument('--pyroscope', default='http://127.0.0.1:44041')
    parser.add_argument('--output', type=Path, default=ROOT / 'target/correlation/result.json')
    args = parser.parse_args()
    binary = args.binary.resolve()
    assert binary.is_file(), f'Build {binary} first'
    args.output.parent.mkdir(parents=True, exist_ok=True)
    grafana = args.grafana.rstrip('/')
    tempo_proxy = f'{grafana}/api/datasources/proxy/uid/{TEMPO_UID}'
    profile_proxy = f'{grafana}/api/datasources/proxy/uid/{PROFILE_UID}'
    health = eventually(lambda: get_json(grafana + '/api/health'), 'Grafana readiness')
    eventually(lambda: request(tempo_proxy + '/ready'), 'Tempo readiness through Grafana')
    eventually(lambda: request(profile_proxy + '/ready'), 'Pyroscope readiness through Grafana')
    tempo = get_json(f'{grafana}/api/datasources/uid/{TEMPO_UID}')
    pyroscope = get_json(f'{grafana}/api/datasources/uid/{PROFILE_UID}')
    config = tempo['jsonData']['tracesToProfiles']
    assert tempo['type'] == 'tempo' and pyroscope['type'] == 'grafana-pyroscope-datasource'
    assert config['datasourceUid'] == pyroscope['uid'] == PROFILE_UID
    assert config['profileTypeId'] == PROFILE_TYPE
    assert config['tags'] == [{'key': 'service.name', 'value': 'service_name'}]
    assert not config.get('customQuery', False)
    print('PASS: provisioned Grafana data sources and backend readiness', flush=True)

    service = f'mars-correlation-{time.time_ns()}'
    payloads, failures = [], []

    class Receiver(http.server.BaseHTTPRequestHandler):
        protocol_version = 'HTTP/1.1'

        def do_POST(self):
            destinations = {'/v1/traces': args.tempo_otlp,
                            '/push.v1.PusherService/Push': args.pyroscope}
            if self.path not in destinations:
                self.send_error(404)
                return
            body = self.rfile.read(int(self.headers['Content-Length']))
            payloads.append((self.path, body))
            headers = {name: self.headers[name] for name in ['Content-Type', 'Content-Encoding']
                       if name in self.headers}
            try:
                request(destinations[self.path].rstrip('/') + self.path, body, headers)
                status = 200
            except Exception as error:
                failures.append(str(error))
                status = 502
            self.send_response(status)
            self.send_header('Content-Length', '0')
            self.end_headers()

        def log_message(self, *_):
            pass

    receiver = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Receiver)
    thread = threading.Thread(target=receiver.serve_forever)
    thread.start()
    endpoint = f'http://127.0.0.1:{receiver.server_port}'
    env = {k: v for k, v in os.environ.items() if not k.startswith(('GST_', 'OTEL_'))}
    env.update(GST_TRACERS='', GST_PLUGIN_PATH='', GST_DEBUG='0',
               OTEL_SERVICE_NAME=service, OTEL_TRACES_SAMPLER='parentbased_always_on',
               OTEL_EXPORTER_OTLP_ENDPOINT=endpoint, OTEL_EXPORTER_OTLP_PROTOCOL='http/protobuf',
               GST_OTEL_PROFILE_URL=endpoint, GST_OTEL_PROFILE_HZ='100',
               GST_OTEL_PROFILE_ELEMENTS='true')
    started = time.time_ns() // 1_000_000
    try:
        run = subprocess.run([str(binary)], env=env, capture_output=True, text=True, timeout=60)
    finally:
        receiver.shutdown()
        receiver.server_close()
        thread.join()
    ended = time.time_ns() // 1_000_000
    args.output.with_suffix('.log').write_text(run.stdout + run.stderr)
    assert run.returncode == 0, run.stdout + run.stderr
    assert not failures, failures
    decoded = analyze(payloads, service)
    assert decoded['received_spans'] == 16 and decoded['correlated_spans'] >= 8, decoded
    expected = {(trace, span): element for element, trace, span in re.findall(
        r'SPAN element=(\w+) trace_id=([0-9a-f]{32}) span_id=([0-9a-f]{16})', run.stdout)}
    assert len(expected) == 8
    assert expected.keys() <= {(s['trace_id'], s['span_id']) for s in decoded['span_cpu_ns']}
    selected = max((s for s in decoded['span_cpu_ns'] if expected.get((s['trace_id'], s['span_id'])) == 'heavy'),
                   key=lambda s: s['cpu_ns'])
    print(f"PASS: {decoded['received_spans']} decoded spans and matching profile IDs", flush=True)

    def check_trace():
        data = get_json(tempo_proxy + '/api/traces/' + selected['trace_id'])
        spans = [v for v in objects(data) if 'spanId' in v and identifier(v['spanId'], 8) == selected['span_id']]
        assert len(spans) == 1, 'selected span is not queryable yet'
        span = spans[0]
        assert identifier(span['traceId'], 16) == selected['trace_id']
        attrs = {a['key']: a['value'].get('stringValue') for a in span['attributes']}
        assert attrs['pyroscope.profile.id'] == selected['span_id'], attrs
        assert any(v.get('key') == 'service.name' and v.get('value', {}).get('stringValue') == service for v in objects(data))
        return data

    trace = eventually(check_trace, 'selected Tempo span and profile marker')
    common = dict(start=started - 2000, end=ended + 2000,
                  labelSelector='{service_name="' + service + '"}',
                  profileTypeID=config['profileTypeId'], maxNodes=10000)
    cases = [
        ('all', {}, decoded['profile_cpu_ns']),
        ('span', {'spanSelector': [selected['span_id']]}, selected['cpu_ns']),
        ('trace', {'traceIdSelector': [selected['trace_id']]},
         sum(s['cpu_ns'] for s in decoded['span_cpu_ns'] if s['trace_id'] == selected['trace_id'])),
        ('missing', {'spanSelector': ['ffffffffffffffff']}, 0),
    ]
    results = {}
    for name, selection, expected_cpu in cases:
        body = {**common, **selection}

        def check_profile():
            result = get_json(profile_proxy + '/querier.v1.QuerierService/SelectMergeStacktraces', body)
            actual = int(result.get('flamegraph', {}).get('total', 0))
            assert actual == expected_cpu, (name, actual, expected_cpu)
            return result

        data = eventually(check_profile, f'{name} CPU total through Grafana')
        if name == 'span':
            assert any('[element] heavy ' in value for value in data['flamegraph']['names']), data
        results[name] = dict(request=body, cpu_ns=expected_cpu, response=data)
    assert 0 < results['span']['cpu_ns'] < results['trace']['cpu_ns'] < results['all']['cpu_ns']
    print('PASS: Grafana proxy queries match aggregate, span, trace and unknown-span CPU totals', flush=True)

    panes = {'A': {'datasource': TEMPO_UID,
                   'queries': [{'refId': 'A', 'queryType': 'traceId', 'query': selected['trace_id']}],
                   'range': {'from': str(common['start']), 'to': str(common['end'])}}}
    explore = grafana + '/explore?' + urllib.parse.urlencode({'schemaVersion': '1', 'panes': json.dumps(panes), 'orgId': '1'})
    paths = [Path(__file__), Path(__file__).with_name('compose.yaml'), Path(__file__).with_name('tempo.yaml'),
             Path(__file__).with_name('datasources.yaml'), ROOT/'tracer/otel/examples/profile_correlation.rs',
             ROOT/'tracer/otel/bench/profiles.py']
    output = dict(revision=subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
                  source_sha256={str(p.relative_to(ROOT)): hashlib.sha256(p.read_bytes()).hexdigest() for p in paths},
                  binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(), grafana=health,
                  service_name=service, started_ms=started, ended_ms=ended, selected=selected,
                  provisioned_traces_to_profiles=config, decoded=decoded, tempo_trace=trace,
                  queries=results, explore_url=explore, browser_clickthrough_verified=False)
    args.output.write_text(json.dumps(output, indent=2) + '\n')
    print('Selected span:', selected['span_id'])
    print('Open trace:', explore)
    print('Results:', args.output)
    print('Browser click-through is a separate manual check; see the correlation guide.')


if __name__ == '__main__':
    main()
