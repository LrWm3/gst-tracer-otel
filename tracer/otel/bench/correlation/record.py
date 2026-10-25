"""Record real Grafana navigation from a successful check.py --showcase result.

Requires agent-browser with Chrome installed. The recording and screenshots are
actual UI output; data links, IDs and time ranges are checked while navigating.
Use --dry-run to validate the clicks without recording a WebM.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import time
import urllib.parse


class Browser:
    def __init__(self, binary, session, profile):
        self.command = [binary, '--session', session, '--profile', str(profile.resolve()),
                        '--headed', 'false', '--args', '--no-sandbox']

    def run(self, *args):
        return subprocess.check_output([*self.command, *map(str, args)], text=True, timeout=40).strip()

    def js(self, expression):
        return json.loads(self.run('eval', expression))

    def wait(self, expression, description):
        deadline = time.monotonic() + 30
        while not self.js(expression):
            assert time.monotonic() < deadline, description
            time.sleep(0.2)

    def text(self):
        return self.js('document.body.innerText')

    def refs(self, role, name, expanded=None, pattern=False):
        snapshot = self.run('snapshot', '-i', '-c')
        result = []
        for line in snapshot.splitlines():
            match = re.match(r'\s*- ' + re.escape(role) + r' "([^"]*)".*?\bref=(e\d+)', line)
            if not match or not (re.search(name, match[1]) if pattern else name == match[1]):
                continue
            if expanded is not None and f'expanded={str(expanded).lower()}' not in line:
                continue
            result.append('@' + match[2])
        return result

    def click(self, role, name, index=0, optional=False, expanded=None, pattern=False):
        refs = self.refs(role, name, expanded, pattern)
        if optional and not refs:
            return False
        assert len(refs) > index, (role, name, refs)
        self.run('click', refs[index])
        return True

    def collapse_queries(self):
        while self.click('button', 'Collapse query row', optional=True):
            pass

    def close_left(self):
        self.click('button', 'Show more items')
        self.click('button', 'Close split pane')
        self.wait('document.querySelectorAll(\'nav[aria-label="Explore toolbar"]\').length === 1', 'close split pane')
        self.run('press', 'Escape')
        time.sleep(0.2)  # Let the toolbar popover finish closing before the next click.

    def select_heavy(self):
        self.collapse_queries()
        self.click('button', 'Overview', optional=True, expanded=True)
        self.click('switch', r'^gst\.pad\.push identity \(', pattern=True)

    def panes(self):
        url = self.run('get', 'url')
        return url, json.loads(urllib.parse.parse_qs(urllib.parse.urlsplit(url).query)['panes'][0])

    def wait_datasource(self, uid):
        self.wait('Object.values(JSON.parse(new URL(location.href).searchParams.get("panes") || "{}"))'
                  f'.some(p => p.datasource === {json.dumps(uid)})', 'Explore URL update: ' + uid)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--result', type=Path, required=True)
    parser.add_argument('--output', type=Path, default=Path('target/correlation/video-logs'))
    parser.add_argument('--agent-browser', default=shutil.which('agent-browser'))
    parser.add_argument('--session', default='mars-correlation-record')
    parser.add_argument('--dry-run', action='store_true')
    args = parser.parse_args()
    assert args.agent_browser, 'Install agent-browser or pass --agent-browser'
    args.output.mkdir(parents=True, exist_ok=True)
    result = json.loads(args.result.read_text())
    selected = result['selected']
    assert result['selected_log']['body'].startswith('slow frame:'), 'Use check.py --showcase'
    browser = Browser(args.agent_browser, args.session, args.output / 'browser-profile')
    browser.run('set', 'viewport', 1440, 900)
    checks = dict(input_sha256=hashlib.sha256(args.result.read_bytes()).hexdigest(),
                  service_name=result['service_name'], selected=selected, passed=False)
    events = []
    start = None

    def capture(name, hold=0):
        seconds = None if start is None else round(time.monotonic() - start, 3)
        browser.run('screenshot', args.output / (name + '.png'))
        events.append(dict(name=name, seconds=seconds))
        print(name, seconds, flush=True)
        time.sleep(hold)

    unrelated = next(log for log in result['logs'] if log['trace_id'] and log['trace_id'] != selected['trace_id'])
    for name, old, new in [('unknown-trace', selected['trace_id'], 'f' * 32),
                            ('unrelated-span', selected['span_id'], unrelated['span_id'])]:
        browser.run('open', result['logs_explore_url'].replace(old, new))
        browser.wait('document.body.innerText.includes("No logs found.")', name)
        checks[name] = dict(url=browser.run('get', 'url'), no_logs_found=True)
        capture(name)

    browser.run('open', result['metrics_explore_url'])
    marker_selector = '[data-testid="data-testid Exemplar marker"]'
    browser.wait(f'document.querySelectorAll({json.dumps(marker_selector)}).length > 1', 'metric exemplars')
    browser.click('button', 'Close menu', optional=True)
    browser.click('button', 'Collapse outline', optional=True)
    browser.collapse_queries()
    browser.run('mouse', 'move', 1400, 890)
    recording = False
    try:
        start = time.monotonic()
        if not args.dry_run:
            browser.run('record', 'start', args.output / 'correlation-raw.webm')
            recording = True
        browser.wait(f'document.querySelectorAll({json.dumps(marker_selector)}).length > 1', 'recording page readiness')
        browser.collapse_queries()
        capture('01-metrics', 4)
        # The controlled example has one high exemplar. Choose its rendered
        # marker, then verify the tooltip IDs before following its actual link.
        point = browser.js(f'''[...document.querySelectorAll({json.dumps(marker_selector)})]
            .map(e => e.getBoundingClientRect().toJSON()).sort((a,b) => a.y-b.y)[0]''')
        browser.run('mouse', 'move', round(point['x'] + point['width']/2), round(point['y'] + 3))
        browser.wait(f'document.body.innerText.includes({json.dumps(selected["span_id"])})', 'exemplar tooltip')
        assert selected['trace_id'] in browser.text()
        capture('02-exemplar', 4)
        browser.click('link', 'Inspect trace')
        browser.wait('document.body.innerText.includes("gst.pad.push identity")', 'metric to trace')
        browser.wait_datasource('mars-correlation-tempo')
        url, panes = browser.panes()
        assert any(p['datasource'] == 'mars-correlation-tempo' and p['queries'][0]['query'] == selected['trace_id'] for p in panes.values())
        checks['metric-to-trace'] = dict(url=url, trace_id=selected['trace_id'])
        capture('03-trace-split')
        browser.close_left()
        browser.select_heavy()
        capture('04-selected-span', 5)
        browser.click('link', 'Logs for this span')
        browser.wait('document.body.innerText.includes("slow frame:")', 'trace to log')
        browser.wait_datasource('mars-correlation-loki')
        url, panes = browser.panes()
        log_pane, = [p for p in panes.values() if p['datasource'] == 'mars-correlation-loki']
        query = log_pane['queries'][0]['expr']
        assert all(value in query for value in [result['service_name'], selected['trace_id'], selected['span_id']])
        assert [int(log_pane['range'][key]) for key in ('from','to')] == [v // 1_000_000 for v in result['selected_log_range_ns']]
        checks['trace-to-log'] = dict(url=url, query=query, range=log_pane['range'])
        capture('05-log-split')
        browser.close_left()
        browser.collapse_queries()
        browser.click('button', 'Logs volume', optional=True, expanded=True)
        browser.click('button', 'Collapse sidebar', optional=True)
        browser.click('generic', r'^\d{4}-\d{2}-\d{2} .*slow frame:', pattern=True)
        refs = browser.refs('textbox', 'Search field names and values')
        assert len(refs) == 1, 'log-details field filter'
        browser.run('fill', refs[0], 'id')
        browser.wait('document.body.innerText.includes("Inspect trace")', 'log derived link')
        assert selected['trace_id'] in browser.text() and selected['span_id'] in browser.text()
        capture('06-matching-log', 7)
        browser.click('link', 'Inspect trace')
        browser.wait('document.body.innerText.includes("gst.pad.push identity")', 'log to trace')
        browser.wait_datasource('mars-correlation-tempo')
        url, panes = browser.panes()
        assert any(p['datasource'] == 'mars-correlation-tempo' and p['queries'][0]['query'] == selected['trace_id'] for p in panes.values())
        checks['log-to-trace'] = dict(url=url, trace_id=selected['trace_id'])
        capture('07-return-trace')
        browser.close_left()
        browser.select_heavy()
        capture('08-same-span', 4)
        browser.click('link', 'Profiles for this span')
        browser.wait('document.body.innerText.includes("Mars profiles")', 'trace to profile')
        browser.wait_datasource('mars-correlation-pyroscope')
        url, panes = browser.panes()
        profile_pane, = [p for p in panes.values() if p['datasource'] == 'mars-correlation-pyroscope']
        profile_query = profile_pane['queries'][0]
        assert profile_query['spanSelector'] == [selected['span_id']]
        assert profile_query['labelSelector'] == '{service_name="' + result['service_name'] + '"}'
        assert profile_query['profileTypeId'] == result['provisioned_traces_to_profiles']['profileTypeId']
        capture('09-profile-query')
        browser.close_left()
        browser.collapse_queries()
        browser.wait('document.body.innerText.includes("profile_correlation::simulated_cpu_work")', 'profile result')
        browser.click('button', 'Expand all groups')
        browser.run('mouse', 'move', 1400, 890)
        assert '[element] heavy [identity]' in browser.text()
        assert f'{selected["cpu_ns"] / 1_000_000:g} ms' in browser.text()
        capture('10-profile', 8)
        checks['profile'] = dict(url=browser.run('get', 'url'), query=profile_query,
                                 range=profile_pane['range'], text=browser.text())
        capture('end')
        checks['passed'] = True
    finally:
        if recording:
            browser.run('record', 'stop')
        (args.output / 'capture-events.json').write_text(json.dumps(events, indent=2) + '\n')
        (args.output / 'browser-check.json').write_text(json.dumps(checks, indent=2) + '\n')
    print('PASS: real Grafana links and negative log controls', flush=True)


if __name__ == '__main__':
    main()
