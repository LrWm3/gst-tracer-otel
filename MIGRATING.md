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

Configuration variables keep their existing names: `GST_INSTRUMENT_*`,
`GST_PROM_LATENCY_*`, `GST_OTEL_*`, and `OTEL_*`. Metric names and the Pyroscope
protocol attribute `pyroscope.profile.id` are also unchanged.

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
