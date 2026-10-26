"""Capture actual GPU trace/profile navigation after check.py has passed.

Requires agent-browser with Chrome installed and the GPU Compose overlay.
Writes screenshots and verification JSON to --output, without modifying data.
"""
import argparse
import importlib.util
import json
from pathlib import Path
import re
import shutil
import time
import urllib.parse

spec = importlib.util.spec_from_file_location(
    'correlation_record', Path(__file__).resolve().parents[1] / 'correlation' / 'record.py')
common = importlib.util.module_from_spec(spec)
spec.loader.exec_module(common)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--result', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--session', default='mars-gpu-capture')
    args = parser.parse_args()
    data = json.loads(args.result.read_text())
    assert data['passed']
    selected = data['selected']
    args.output.mkdir(parents=True, exist_ok=True)
    browser = common.Browser(shutil.which('agent-browser') or 'agent-browser', args.session,
                             args.output / 'browser-profile')
    browser.run('open', data['trace_url'])
    # Bring the page forward: Chrome can defer painting a background tab.
    tabs = browser.run('tab', 'list')
    tab = re.search(r'→ \[(t\d+)\]', tabs)[1]
    browser.run('tab', tab)
    browser.run('set', 'viewport', 1600, 1000)
    browser.wait(f'document.body.innerText.includes({json.dumps(selected["trace_id"])})', 'stored trace')
    browser.wait('document.body.innerText.includes("gst.pad.push identity (")', 'trace span rows')
    browser.click('button', 'Close menu', optional=True)
    while browser.click('button', 'Collapse outline', optional=True):
        pass
    browser.collapse_queries()
    browser.click('button', 'Overview', optional=True, expanded=True)
    browser.click('switch', r'^gst\.pad\.push identity \(', pattern=True)
    browser.click('link', 'Profiles for this span')
    browser.wait_datasource('mars-correlation-pyroscope')
    _, panes = browser.panes()
    query, = [q for p in panes.values() if p['datasource'] == 'mars-correlation-pyroscope'
              for q in p['queries']]
    assert query['spanSelector'] == [selected['span_id']]
    assert query['profileTypeId'] == data['query']['profileTypeID']
    browser.wait('document.querySelectorAll("canvas").length >= 1', 'profile flame graph')
    browser.collapse_queries()
    while browser.click('button', 'Collapse outline', optional=True):
        pass
    time.sleep(1)
    for i in range(len(browser.refs('button', 'Expand all groups'))):
        browser.click('button', 'Expand all groups', index=i)

    def capture(name):
        browser.run('tab', tab)
        browser.run('mouse', 'move', 1580, 980)
        browser.run('screenshot', args.output / name)

    capture('trace-to-gpu-profile.png')
    browser.close_left()
    browser.collapse_queries()
    browser.click('radio', 'Flame Graph', optional=True)
    browser.click('button', 'Expand all groups', optional=True)
    browser.run('set', 'viewport', 1600, 500)
    capture('element-gpu-kernels.png')
    browser.run('set', 'viewport', 1600, 1000)
    base = urllib.parse.urlsplit(data['trace_url'])
    params = {
        'var-dataSource': 'mars-correlation-pyroscope', 'var-serviceName': data['service_name'],
        'var-profileMetricId': data['query']['profileTypeID'], 'explorationType': 'flame-graph',
        # Match the normal trace-to-profiles padding. Drilldown's service
        # catalog can miss very short windows even when the span query succeeds.
        'from': str(data['query']['start'] - 60000), 'to': str(data['query']['end'] + 60000),
        'showSpanHeatmap': 'true',
    }
    browser.run('open', urllib.parse.urlunsplit((base.scheme, base.netloc,
                '/a/grafana-pyroscope-app/explore', urllib.parse.urlencode(params), '')))
    browser.wait('Array.from(document.querySelectorAll("button"))'
                 '.some(b => b.getAttribute("aria-label") === "Open trace")', 'span exemplars')
    browser.click('combobox', 'Tempo data source')
    browser.click('option', 'Mars GPU traces')
    browser.wait('Array.from(document.querySelectorAll("button"))'
                 '.some(b => b.getAttribute("aria-label") === "Open trace")', 'trace lookup')
    # The demo's largest exemplar must be the selected inference span.
    exemplar_spans = browser.js('Array.from(document.querySelectorAll("td"))'
                               '.map(x=>x.innerText).filter(x=>/^[0-9a-f]{16}$/.test(x))')
    assert exemplar_spans[0] == selected['span_id'], exemplar_spans
    browser.click('button', 'Expand all groups', optional=True)
    browser.run('press', 'Control+Home')
    capture('gpu-span-exemplars.png')
    browser.click('button', 'Open trace', index=0)
    browser.run('tab', tab)
    # Give the preview drawer a fresh layout measurement after its transition.
    # Without this its embedded trace panel can remain blank in headless Chrome.
    time.sleep(0.5)
    browser.run('set', 'viewport', 1600, 1001)
    browser.run('set', 'viewport', 1600, 1000)
    browser.wait('document.body.innerText.includes("gst.pad.push identity")', 'reverse trace render')
    assert 'Trace ' + selected['trace_id'] in browser.text()
    capture('gpu-profile-to-trace.png')
    (args.output / 'browser.json').write_text(json.dumps({
        'passed': True, 'service_name': data['service_name'],
        'span_id': selected['span_id'], 'trace_id': selected['trace_id'],
        'forward_query': query, 'reverse_url': browser.run('get', 'url'),
    }, indent=2) + '\n')
    print('PASS: trace -> GPU profile and GPU span exemplar -> original trace')


if __name__ == '__main__':
    main()
