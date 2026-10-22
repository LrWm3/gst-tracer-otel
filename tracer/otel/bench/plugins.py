"""Resolve tracer names from prebuilt plugins, including historical baselines."""
import re
import subprocess


def otel_tracer(directory, env):
    # Inspect the exact binary, not a potentially stale factory in the registry.
    inspection = subprocess.check_output(
        ['gst-inspect-1.0', str(directory / 'libgstoteltracer.so')],
        env=dict(env, GST_TRACERS=''), text=True, timeout=30)
    names = set(re.findall(r'^\s+(\S+) \(GstTracerFactory\)$', inspection, re.MULTILINE))
    for name in ('mars-otel', 'otel-tracer'):
        if name in names:
            return name
    raise RuntimeError(f'No OTel tracer in {directory}: {inspection}')
