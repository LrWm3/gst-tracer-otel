"""Check GPU-only demo data through Grafana's Tempo and Pyroscope proxies.

Input: gpu_correlation stdout and JSON containing service_name, started_ms,
ended_ms. These can be collected on a separate CUDA host; no host identity is
needed. The profile totals are checked across every span, not just a screenshot.
"""
import argparse
import importlib.util
import json
from pathlib import Path
import re
import sys
import urllib.parse

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location(
    'correlation_check', Path(__file__).resolve().parents[1] / 'correlation' / 'check.py')
common = importlib.util.module_from_spec(spec)
spec.loader.exec_module(common)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--log', type=Path, required=True)
    parser.add_argument('--meta', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--grafana', default='http://127.0.0.1:43002')
    args = parser.parse_args()
    meta = json.loads(args.meta.read_text())
    service = meta['service_name']
    spans = [dict(element=e, trace_id=t, span_id=s) for e, t, s in re.findall(
        r'SPAN element=(\w+) trace_id=([0-9a-f]{32}) span_id=([0-9a-f]{16})', args.log.read_text())]
    assert spans and {s['element'] for s in spans} == {'preprocess', 'inference'}
    proxy = args.grafana + '/api/datasources/proxy/uid/'
    base = dict(start=meta['started_ms'] - 2000, end=meta['ended_ms'] + 2000,
                labelSelector='{service_name="' + service + '"}',
                profileTypeID='gpu_kernel:gpu:nanoseconds:gpu:nanoseconds', maxNodes=10000)

    def query(**selection):
        return common.get_json(proxy + 'mars-correlation-pyroscope/querier.v1.QuerierService/SelectMergeStacktraces',
                               {**base, **selection})

    results = []
    traces = {}
    for selected in spans:
        def check():
            profile = query(spanSelector=[selected['span_id']])
            flame = profile.get('flamegraph', {})
            assert int(flame.get('total', 0)) > 0, selected
            names = flame['names']
            assert {n for n in names if n.endswith('_kernel')} == {'resize_kernel', 'normalize_kernel', 'convert_kernel'}, names
            elements = [n for n in names if n.startswith('[element]')]
            assert len(elements) == 1 and '[element] ' + selected['element'] + ' ' in elements[0], names
            assert len(flame['levels']) == 3, 'expected total -> element -> kernels only'
            return profile
        profile = common.eventually(check, 'span-specific GPU profile')
        if selected['trace_id'] not in traces:
            traces[selected['trace_id']] = common.eventually(
                lambda: common.get_json(proxy + 'mars-gpu-tempo/api/traces/' + selected['trace_id']), 'stored trace')
        trace = traces[selected['trace_id']]
        span, = [v for v in common.objects(trace) if 'spanId' in v and common.identifier(v['spanId'], 8) == selected['span_id']]
        attrs = {a['key']: a['value'].get('stringValue') for a in span['attributes']}
        assert attrs['pyroscope.profile.id'] == selected['span_id']
        assert span['name'] == 'gst.pad.push identity'
        results.append({**selected, 'gpu_ns': int(profile['flamegraph']['total']),
                        'span_ns': int(span['endTimeUnixNano']) - int(span['startTimeUnixNano']),
                        'profile': profile})
    aggregate = query()
    assert int(aggregate['flamegraph']['total']) == sum(s['gpu_ns'] for s in results)
    missing = query(spanSelector=['ffffffffffffffff'])
    assert int(missing.get('flamegraph', {}).get('total', 0)) == 0
    cpu = query(profileTypeID='process_cpu:cpu:nanoseconds:cpu:nanoseconds')
    assert int(cpu.get('flamegraph', {}).get('total', 0)) == 0
    trace_profiles = {}
    for trace_id in traces:
        profile = query(traceIdSelector=[trace_id])
        assert int(profile['flamegraph']['total']) == sum(s['gpu_ns'] for s in results if s['trace_id'] == trace_id)
        trace_profiles[trace_id] = profile
    selected = max((s for s in results if s['element'] == 'inference'), key=lambda s: s['gpu_ns'])
    heatmap = common.get_json(
        proxy + 'mars-correlation-pyroscope/querier.v1.QuerierService/SelectHeatmap',
        {k: v for k, v in {**base, 'step': 1, 'queryType': 2, 'exemplarType': 3}.items() if k != 'maxNodes'})
    exemplars = [v for v in common.objects(heatmap) if 'spanId' in v and 'value' in v]
    assert exemplars, 'span exemplars are needed for the reverse link'
    for exemplar in exemplars:
        span, = [s for s in results if s['span_id'] == exemplar['spanId']]
        assert exemplar['traceId'] == span['trace_id']
        assert 0 < int(exemplar['value']) <= span['gpu_ns']
    panes = {'A': {'datasource': 'mars-gpu-tempo', 'queries': [dict(refId='A', queryType='traceql', query=selected['trace_id'])],
                   'range': {'from': str(base['start']), 'to': str(base['end'])}}}
    url = args.grafana + '/explore?' + urllib.parse.urlencode(dict(schemaVersion='1', panes=json.dumps(panes), orgId='1'))
    output = {**meta, 'passed': True, 'selected': selected, 'spans': results,
              'aggregate': aggregate, 'traces': traces, 'trace_profiles': trace_profiles,
              'negative_span': missing, 'cpu_profile': cpu, 'heatmap': heatmap,
              'trace_url': url, 'query': base}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(output, indent=2) + '\n')
    print(f"PASS: {len(spans)} spans retain exclusive element/kernel attribution; aggregate and trace totals agree")
    print(f"PASS: unknown span and CPU profiles are empty; selected GPU work {selected['gpu_ns']/1e6:.3f} ms, host span {selected['span_ns']/1e6:.3f} ms")
    print(url)


if __name__ == '__main__':
    main()
