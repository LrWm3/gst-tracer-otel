# Migrating to Mars tracers

For a first installation, start with the [README](README.md). This guide covers
changes for existing users.

## Tracer names

Update `GST_TRACERS` to use the Mars entry points. The previous names are no
longer registered by these plugins.

| Previous entry point | Current entry point |
| --- | --- |
| `gst-instrument` | `mars-instrument` |
| `prom-latency` | `mars-metrics` |
| `otel-tracer` | `mars-otel` |
| `pyroscope` | `mars-profiles` |
| `noop-latency` | `mars-noop` |

For retained packages, Cargo package names and shared-library filenames are
unchanged. For example, build `gst-otel-tracer` and set
`GST_TRACERS=mars-instrument`.

Existing configuration variables remain supported: `GST_INSTRUMENT_*`,
`GST_PROM_LATENCY_*`, `GST_OTEL_*`, and `OTEL_*`. Metric names and the Pyroscope
protocol attribute `pyroscope.profile.id` are also unchanged.

## Explicit signals and selectable metrics export

New configurations can use
`GST_TRACERS='mars-instrument(signals="metrics,traces,profiles")'`, or set
`MARS_SIGNALS` with plain `GST_TRACERS=mars-instrument`. The signal list controls
collection; endpoint variables do not enable additional signals.

This selects the full shared metrics collector with either OTLP (default) or
`metrics-exporter="prometheus"`. It includes push/list/pull timing, queue pressure,
and optional I/O and PTS-age diagnostics. Migrate dashboards from the old four
`gst.pad.push.*` histograms to the `gst_element_*` families and callee labels in
the [current guide](tracer/otel/INSTRUMENT.md). Sampling now applies to all timing
histograms; wall counters still measure every selected call. The old 1024-group
cap and CPU-only sampling setting apply only to the compatibility collector.

No-parameter `mars-instrument` with no `MARS_SIGNALS`, `mars-metrics`,
`mars-profiles`, and `mars-otel` preserve their existing behavior. The latter is
now presented as a compatibility tracing preset. New `MARS_METRIC_*` and
`MARS_PROFILE_*` variables take precedence over their documented legacy aliases.

## Standalone profiler replacement

The `gst-pyroscope-tracer` package has been removed. `mars-profiles` is now
provided by the OTel plugin using its shared embedded CPU sampler. Build it with:

```sh
cargo build --release -p gst-otel-tracer --features profiles
```

Remove old installed copies of `libgstpyroscopetracer.so` from your GStreamer
plugin path so they do not conflict with the new factory in `libgstoteltracer.so`.
Check existing build directories such as `target/release` and its `deps`
subdirectory too: rebuilding the remaining packages does not remove the old
library. A leftover copy can cause a `MarsProfilesTracer has already been
registered` error during plugin discovery.

Set `OTEL_SERVICE_NAME` to preserve your existing Pyroscope application name.

Old tracer parameters must be replaced with environment variables. The
[profiler migration guide](tracer/otel/PROFILES.md#migrating-from-the-standalone-plugin)
maps each setting and explains changes to sampling and shutdown. Arbitrary
series tags are not supported by the embedded backend.

Select one of `mars-profiles`, `mars-otel`, or `mars-instrument` per process.
They share one configuration; the first constructed instance selects the
signals. Use `mars-otel` or `mars-instrument` for profiles correlated with traces.

## Debug categories, GObject types, and OTLP scopes

Update any filters that refer to these names:

| Tracer | Debug category | GObject type |
| --- | --- | --- |
| `mars-instrument` | `mars-otel` | `MarsInstrumentTracer` |
| `mars-metrics` | `mars-metrics` | `MarsMetricsTracer` |
| `mars-otel` | `mars-otel` | `MarsOtelTracer` |
| `mars-profiles` | `mars-profiles` | `MarsProfilesTracer` |
| `mars-noop` | `mars-noop` | `MarsNoopTracer` |

OTLP trace and log scopes are named `mars-otel`; the metric scope is
`mars-instrument`.
