# Tracer properties

Configure collection through `GST_TRACERS` or the same construction-time GObject
properties from application code. For example:

```sh
GST_TRACERS='mars-instrument(signals="metrics,traces,profiles",monitoring=core,metrics-sample-rate=16,profile-url="http://localhost:4040",profile-hz=100,profile-symbolization=functions)' \
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318 \
OTEL_SERVICE_NAME=video-pipeline \
  gst-launch-1.0 videotestsrc num-buffers=300 ! videoconvert ! fakesink
```

Build with `profiles` for CPU profiling, or `gpu-profiles` for optional GPU
collection. Choose one of `mars-instrument`, `mars-otel`, or `mars-profiles` per
process: the first instance owns the shared runtime. Restart to change settings.

## Precedence and inheritance

The order is **`params` / `GST_TRACERS(...)` → direct GObject property → environment
fallback → built-in default**. Existing environment configurations continue to
work. Properties never modify the process environment.

The new numeric properties use **0 to inherit**; string properties use **null /
unset**; enums use **auto**. On/off controls are `Toggle` enums with `auto`,
`true`, and `false`, so an explicit false can override an environment true.
[GObject supplies defaults for omitted construction properties](https://docs.gtk.org/gobject/ctor.Object.new_with_properties.html); these
inheritance values keep an omitted property distinct from an explicit default.
Reading a property returns its configured override or inheritance value, not the
resolved environment value. An explicit `0` or `auto` in `params` resets the
direct property override to inheritance. The tables below show effective
built-in defaults.

`profile-sources` still defaults directly to `cpu` and has no environment
fallback. `profile-nvtx-adapter` defaults to off and also has no environment
fallback. See [NVTX attribution](NVTX.md) for its empty-string behavior.

Unknown parameters, invalid explicit values, and invalid environment values for
enabled collectors produce initialization diagnostics in `GST_DEBUG=mars-otel:3`
(or `mars-metrics:3`). Disabled collectors do not read their environment settings.
Explicit parameters are still checked for type, range and supported build features.
An explicit signal list is exhaustive: configuring an endpoint or adapter does
not enable an omitted signal.

## Signals and metrics

`signals`, `metrics-exporter`, and `metrics-interval-ms` are on `mars-instrument`.
The other properties below are shared with `mars-metrics`. On `mars-instrument`,
set `signals` when supplying `metrics-*` overrides to select the full collector.
Omitting signals preserves the older compatibility preset.

| Property | Default | Values / purpose | Environment fallback |
| --- | --- | --- | --- |
| `signals` | Compatibility preset | Comma-separated `metrics,traces,profiles,logs` | `MARS_SIGNALS` |
| `monitoring` | `core` | `none`, `core`, `all`; instrumentation health | `MARS_MONITORING` |
| `metrics-exporter` | `otlp` | `otlp` or `prometheus` | `MARS_METRICS_EXPORTER` |
| `metrics-port` | `9188`; `8080` on mars-metrics | Prometheus port, 1–65535 | `MARS_METRICS_PORT` |
| `metrics-interval-ms` | `10000` | OTLP export interval, 1–60000 ms | `MARS_METRIC_INTERVAL_MS` |
| `metrics-sample-rate` | `16` | One in N calls enters histograms, 1–1048576 | `MARS_METRIC_SAMPLE_RATE` |
| `metrics-include` | `*` | Comma-separated element-name globs | `MARS_METRIC_INCLUDE` |
| `metrics-queue-interval-ms` | `100` | Queue observation interval, 1–60000 ms | `MARS_METRIC_QUEUE_INTERVAL_MS` |
| `metrics-io-counts` | `false` | Per-pad input/output counters | `MARS_METRIC_IO_COUNTS` |
| `metrics-pts-age` | `false` | Buffer age relative to pipeline running time | `MARS_METRIC_PTS_AGE` |

The original `mars-metrics(port=8080)` parameter remains an alias for
`metrics-port`, with one exception: **`port=0` disables HTTP** so applications
can read the `metrics` action directly. `metrics-port=0` instead inherits the
environment/default port. When both parameters are supplied, `metrics-port` wins. Original
`GST_PROM_LATENCY_*` environment aliases remain lower-priority fallbacks:
`HISTOGRAM_SAMPLE_RATE`, `INCLUDE`, `QUEUE_SAMPLE_INTERVAL_MS`, `IO_COUNTS`, and
`PTS_AGE`, respectively. `GST_INSTRUMENT_METRIC_INTERVAL_MS` is the fallback for
the export interval. Both metric exporters use the same resolved collector
configuration, installed before hooks run.

Legacy `GST_PROM_LATENCY_*` aliases retain their original permissive parsing:
numeric values accept any positive `u64`, with malformed/zero values falling back
to defaults; boolean values other than `true`, `1`, or `yes` (case-insensitive)
mean false. The new properties and `MARS_*` variables use the strict ranges and
values above. Explicit properties still override either environment namespace.

## Profiles

These properties are shared by `mars-instrument`, `mars-otel`, and
`mars-profiles`. CPU profiles require the `profiles` build feature on Linux
x86-64 or arm64. See [CPU profiles](PROFILES.md) and [GPU profiles](GPU_PROFILES.md).

| Property | Default | Values / purpose | Environment fallback |
| --- | --- | --- | --- |
| `profile-sources` | `cpu` | `cpu`; also `gpu` and `cpu,gpu` with `gpu-profiles` | None |
| `profile-url` | `http://localhost:4040` when profiles selected | Pyroscope HTTP(S) destination | `MARS_PROFILE_URL` |
| `profile-hz` | `100` | CPU samples per second per registered thread, 1–1000 | `MARS_PROFILE_HZ` |
| `profile-timeout-ms` | `10000` | CPU/GPU upload timeout, 1–60000 ms | `MARS_PROFILE_TIMEOUT_MS` |
| `profile-symbolization` | `full` | `full` or `functions`; CPU symbol detail | `MARS_PROFILE_SYMBOLIZATION` |
| `profile-memory-trim` | `true` | One background heap trim after the first pipeline reaches `PLAYING`; on by default to return retained startup allocations (about 450 MiB lower running RSS in the tested processor) | None |
| `profile-elements` | `true` | Element/task-owner roots; required for NVTX attribution | `MARS_PROFILE_ELEMENTS` |
| `profile-signal` | `SIGRTMIN+6` | Available realtime signal number; CPU only | `MARS_PROFILE_SIGNAL` |
| `profile-gpu-window-ms` | `0` (continuous) | GPU capture window in ms: `0`, or `10..=profile-gpu-period-ms`; Linux `gpu-profiles` builds only | None |
| `profile-gpu-period-ms` | `10000` | Window repetition interval in ms, up to `3600000`; `0` inherits the default | None |
| `profile-gpu-adapter` | `libmars-cupti.so` | CUPTI adapter library; property exists only in Linux `gpu-profiles` builds | `MARS_GPU_ADAPTER` |
| `profile-nvtx-adapter` | Off | NVTX worker-attribution library; requires only `profiles` on Linux | None |

`profile-memory-trim=true` is enabled by default because freed startup/model-loading
allocations can remain resident for the lifetime of the process. In the tested
GPU processor, a single trim reduced running RSS by about **450 MiB**, with a
**60–70 ms** allocator operation. Results depend on the workload; some savings
also apply without profiling. **The earlier startup peak is unchanged.**

On Linux/glibc with profiling active, the first top-level pipeline's committed
`PLAYING` state schedules one `malloc_trim(0)` on a short-lived background thread.
This handles asynchronous state changes and runs outside GStreamer/CUPTI locks.
It affects the whole process's glibc heap, and allocator contention can still
briefly delay other threads. It is not repeated on pause/resume or later pipelines.
`PLAYING` is a post-preroll boundary, not a guarantee that all application warmup
has finished; live sources and lazy model loading may allocate later. Reused
pages may become resident again. Other platforms/allocators do not gain this
reclamation, and profiling-disabled configurations do not run it.

Set `profile-memory-trim=false` to disable it, for example:

```sh
GST_TRACERS='mars-instrument(signals="traces,profiles",profile-memory-trim=false)'
```

The property uses the same `Toggle` enum as other on/off properties; `auto`
resolves to `true`, with no environment fallback. Shutdown cancels a queued trim
or waits for a running trim within the existing deadline. Health monitoring
exposes attempts, released-page responses (not bytes), elapsed nanoseconds and
pending state through `mars_instrument_profile_memory_trim_*`. A failure to
start the worker is reported as `memory_trim_worker` in profile failure counters
and does not disable profiling.

The `GST_OTEL_PROFILE_*` equivalents remain lower-priority fallbacks for the
`MARS_PROFILE_*` variables in this table. Source selection and NVTX adapter
selection have no environment aliases. GPU-only collection ignores CPU-only
environment settings; CPU-only collection does not load the GPU adapter.

For the compatibility `mars-otel` and original `mars-instrument` presets,
supplying `profile-url` or its environment fallback still opts into profiling.
An explicit `profiles` signal or `mars-profiles` enables it without a URL override.

## Export workers and shutdown

These properties are on `mars-instrument` and `mars-otel`. Dedicated
`mars-profiles` uses explicit flush/shutdown deadlines.

| Property | Default | Range / purpose | Environment fallback |
| --- | --- | --- | --- |
| `trace-root-min-period-us` | `16000` | Minimum interval between admitted local trace roots per pad, 0–4294967295 µs; `0` disables throttling | None |
| `trace-max-age-ms` | `5000` | Stop admitting spans when a local journey reaches this elapsed age, 0–4294967295 ms; `0` disables the age limit | None |
| `trace-max-spans` | `64` | Maximum admitted spans per local journey, including its root and all branches, 0–4294967295; `0` disables the count limit | None |
| `trace-queue-capacity` | `8192` | Pending records, 1–1048576 | `GST_OTEL_QUEUE_CAPACITY` |
| `trace-batch-size` | `min(512, queue capacity)` | Must not exceed queue capacity | `GST_OTEL_BATCH_SIZE` |
| `trace-export-interval-ms` | `200` | Partial batch interval, 1–60000 ms | `GST_OTEL_EXPORT_INTERVAL_MS` |
| `trace-export-concurrency` | `1` | Concurrent requests, 1–4 | `GST_OTEL_EXPORT_CONCURRENCY` |
| `flush-timeout-ms` | `10000` | Default pipeline-stop drain deadline, 1–60000 ms | `GST_OTEL_FLUSH_TIMEOUT_MS` |

Explicit deadlines passed to `flush` / `shutdown` still take precedence for those
calls. Standard `OTEL_*` configuration remains available for resource attributes,
service identity, trace sampling, span limits, OTLP destinations, headers,
compression, timeouts, and log batching. `GST_OTEL_HTTP_CLIENT` still selects
the blocking or async HTTP implementation. `GST_DEBUG` controls GStreamer log
generation; `NVTX_INJECTION64_PATH` remains a before-startup requirement for the
current NVIDIA injection mechanism.

## Application code

```rust
let tracer = glib::Object::builder::<gstoteltracer::InstrumentTracer>()
    .property("signals", "metrics,traces,profiles")
    .property("monitoring", gstoteltracer::MonitoringSetting::Core)
    .property("metrics-exporter", gstoteltracer::MetricsExporter::Otlp)
    .property("metrics-sample-rate", 16u32)
    .property("metrics-io-counts", gstoteltracer::Toggle::Off)
    .property("profile-url", "http://localhost:4040")
    .property("profile-hz", 100u32)
    .property("profile-symbolization", gstoteltracer::ProfileSymbolization::Functions)
    .property("profile-elements", gstoteltracer::Toggle::On)
    .property("flush-timeout-ms", 2000u32)
    .build();
```

The standalone crate exports `gstprometheustracer::MetricsTracer`, `Toggle`, and
`MonitoringSetting` for the equivalent GObject API. All numeric properties are
unsigned 32-bit values. Inspect properties and enum choices with GObject class
introspection; each property describes its effective default and inheritance.
