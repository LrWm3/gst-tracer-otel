# mars-instrument: correlated push telemetry

`mars-instrument` is an experimental entry point in `libgstoteltracer.so`. It
uses one set of push hooks, one nested call stack, and cached pad descriptions
for OTLP metrics and traces, optional correlated GStreamer logs, and optional
Pyroscope CPU profiles. `mars-otel` retains its existing defaults.
For CPU profiles alone, use the [mars-profiles entry point](PROFILES.md) in the
same plugin, built with `--features profiles`.

This first version covers ordinary `gst_pad_push` calls, including nested
pushes and propagation across queues. It does not replace the full
`mars-metrics` feature set: pull mode, buffer lists, queue pressure, per-pad I/O
counts, and PTS-age diagnostics remain in that plugin. The existing
[trace propagation boundaries](README.md#correlation-boundaries) still apply.

## Run

```sh
cargo build --release -p gst-otel-tracer
export GST_PLUGIN_PATH="$PWD/target/release"
export GST_TRACERS=mars-instrument
export OTEL_SERVICE_NAME=gstreamer-pipeline
export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318
export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf
gst-launch-1.0 videotestsrc num-buffers=300 ! videoconvert ! fakesink
```

Enable the collector's OTLP/HTTP metrics and traces pipelines. Metrics default
to `/v1/metrics` and traces to `/v1/traces`. Each signal honors its
`OTEL_EXPORTER_OTLP_{METRICS,TRACES}_{ENDPOINT,HEADERS,TIMEOUT,PROTOCOL}` settings,
with fallback to the corresponding generic OTLP setting. Signal-specific
endpoints are used literally. Timeout values are milliseconds; only
`http/protobuf` without compression is supported. Metrics reuse the existing
HTTP adapters selected by `GST_OTEL_HTTP_CLIENT=blocking|async`.

For CPU profiles, build with `--features profiles` and set
`GST_OTEL_PROFILE_URL=http://localhost:4040`. Use frame pointers and debug
symbols for better stack resolution; see the existing
[profile instructions](README.md#experimental-correlated-cpu-profiles).
Profiling currently requires Linux x86-64. To include GStreamer logs, set
`GST_INSTRUMENT_LOGS=true` and select the desired categories with `GST_DEBUG`.
The standard GStreamer log handler remains enabled.

## Signal controls

| Setting | Default | Meaning |
| --- | --- | --- |
| `GST_INSTRUMENT_METRICS` | `true` | Collect and export push histograms |
| `GST_INSTRUMENT_TRACES` | `true` | Capture, propagate, and export trace spans |
| `GST_INSTRUMENT_LOGS` | Existing `GST_OTEL_LOGS`, otherwise `false` | Enable the GStreamer log bridge |
| `GST_INSTRUMENT_PROFILES` | `true` | Allow the optional profiler; still requires its feature, platform, and URL |
| `GST_INSTRUMENT_METRIC_INTERVAL_MS` | `10000` | Export interval, 1–60000 ms |
| `GST_INSTRUMENT_METRIC_MAX_SERIES` | `1024` | Maximum distinct pad-description groups, 1–16384; each has four histograms |
| `GST_INSTRUMENT_CPU_SAMPLE_RATE` | `16` | Independently sample approximately one in N calls for CPU/blocked histograms; 1–1048576 |

Boolean settings accept `true`, `false`, `1`, and `0`. Existing `GST_OTEL_*`
trace queue/batch/flush controls and `OTEL_TRACES_SAMPLER*` settings still apply.
All signals use the same resource, including `OTEL_SERVICE_NAME` and
`OTEL_RESOURCE_ATTRIBUTES`. The first telemetry tracer instance selects the
process-wide configuration. Additional `mars-otel` or `mars-instrument`
instances warn and do not register duplicate hooks. Select one entry point.

Metrics-only operation (`GST_INSTRUMENT_TRACES=false`, profiles and logs off)
does not generate trace IDs, attach buffer context, or construct a trace HTTP
client. Setting a trace sampler to zero instead preserves the trace propagation
path and parent-based sampling semantics. Neither setting suppresses metrics.

## Measurements and correlation

All four metrics are cumulative OTLP histograms in seconds, with explicit
buckets from 1 microsecond to 10 seconds plus an overflow bucket:

| Metric | Population and measurement |
| --- | --- |
| `gst.pad.push.wall` | Every supported push, including nested downstream calls |
| `gst.pad.push.exclusive.wall` | Every supported push, subtracting all direct nested push durations |
| `gst.pad.push.exclusive.cpu` | Independently sampled calls, exclusive thread CPU time |
| `gst.pad.push.exclusive.blocked` | The same sampled calls, exclusive wall minus CPU, clamped to zero |

CPU clocks are read for sampled calls and their direct children only. CPU clock
failures omit the CPU/blocked observations. Their counts and sums describe the
sampled population; they are not total process CPU or total blocked time. Wall
histogram counts and sums cover all admitted, balanced calls regardless of trace
sampling or trace queue losses. Sleeping, scheduling delay, and downstream
backpressure can all contribute to the blocked estimate. These measurements do
not measure queue residence, GPU completion, or end-to-end buffer latency.

Each group uses the same six `src_pad.*` / `sink_pad.*` descriptions as traces.
A source pad push invokes downstream work: the source label names the call
site, not the element that performed all its work. This avoids claiming that
inclusive push duration is exclusive source-element processing time. Traces
retain their inclusive durations; metric timing excludes its own outer capture
and export bookkeeping, although nested instrumentation still adds noise.

Each histogram keeps at most one recent sampled exemplar, following the
[OTLP exemplar data model](https://opentelemetry.io/docs/specs/otel/metrics/data-model/#exemplars).
The exemplar copies the completed call's trace and span IDs, timestamp, and
measured value. Its value participates in that histogram. IDs are never metric
series attributes. Calls whose spans were rejected by the local trace queue
cannot replace the exemplar; they still update histogram counts and sums.
Exemplar contention skips only the exemplar, and exported snapshots consume it.
A failed export may therefore lose that interval's exemplar while retaining
cumulative measurements for the next attempt.

The navigation chain is:

1. A metric histogram exemplar identifies a trace and span.
2. That span can contain `pyroscope.profile.id`, equal to its span ID.
3. CPU profile samples carry the corresponding `span_id` and `trace_id` labels.
4. GStreamer logs emitted inside the active push carry native trace/span fields.

Configure your metrics backend and Grafana data sources to retain exemplars and
link their `trace_id` to Tempo, then use the existing Tempo-to-Pyroscope link.
Backend rejection, trace retention, and short spans with no CPU samples can
still leave missing counterparts. No exporter can guarantee that every
exemplar's trace/profile has reached durable storage. The integration tests
validate the actual OTLP/log/pprof identifiers; Grafana UI navigation has not
been exercised for this entry point.

## Bounds, lifecycle, and diagnostics

Streaming threads update cached atomic aggregates. There is no per-event metric
queue, protobuf allocation, symbolization, or network operation on that path.
Only cold descriptor registration takes the registry lock. Exemplar updates use
`try_lock`; a worker never holds registry/exemplar locks across HTTP or GObject
calls. The profiler's signal handler never accesses this module or its locks.

Series groups are capped, label values are bounded to 256 Unicode characters,
and export requests contain at most 32 groups. Truncation can merge descriptions
that become identical. The cap counts retained groups as well as active ones.
Excess groups increment `dropped_capacity`; their cached rejection avoids
repeated registration work until retirement frees capacity. An unused group is released after its final successful snapshot once
no cached descriptor or in-flight call owns it. Failed sends retain cumulative
aggregates within the same cap, and the next interval retries their snapshot.
There is one metrics worker, one in-flight request, and one queued control
command. HTTP response reads are capped at 4 MiB, including partial-success
responses; rejected metric points increment failure accounting.

Snapshots are concurrent, not atomic transactions across counters. Histogram
counts are computed from the exported buckets; sums can reflect a slightly
different instant while producers run. Counts and sums are exact after producers
stop. Retired/recreated series start a new cumulative interval.

`stats()` and the GObject `stats` property retain trace accounting.
`metric_stats()` and the `metric-stats` property expose `recorded_pushes`,
`dropped_capacity`, `dropped_invalid`, `skipped_closed`, `skipped_exemplars`,
`exported_batches`, `failed_exports`, and `active_series`. Unsupported hook
families are outside these counters. `profile_stats()` retains profiler
accounting. Logs still use the SDK's separate batch queue and diagnostics.

Pipeline READY→NULL drains traces and metrics within `GST_OTEL_FLUSH_TIMEOUT_MS`.
The optional log bridge subsequently uses its existing SDK flush timeout.
Rust `flush(timeout)` drains traces and metrics under one caller deadline;
`shutdown(timeout)` additionally closes capture and stops profiles and logs.
Stop streaming before the final drain. Metrics shutdown is signaled before
waiting for the trace worker, so a stalled trace backend cannot prevent metric
cleanup from starting. Timed-out workers may finish in the background. Process
shutdown is permanent; a bare process exit can lose pending telemetry.

## Validation and performance

For approximate overhead on a video pipeline, see the
[1080p video comparison](../../reports/otel/INSTRUMENT_VIDEO_RESULTS.md). It
measures the default metrics/traces configuration and profiles/logs enabled,
against no tracer and `mars-otel` using the same binary and export settings.
The report includes whole-process CPU cycles, peak RSS, and checks of delivered
histograms, exemplars, traces, logs, and CPU samples.

Tests decode real HTTP requests from subprocesses and cover zero/partial trace
sampling, metrics-only and traces-only operation, duplicate tracer instances,
queue boundaries, controlled downstream sleeps, trace queue overflow, metric
partial rejection, stalled collectors, bounded shutdown, topology refresh, and
series limits, and both HTTP adapters. Feature-enabled tests connect exemplars to actual exported spans,
logs, and CPU samples. Unit tests cover sequential child subtraction, CPU clock
failure propagation, cumulative retry, concurrent producers, and safe series
retirement when a descriptor is reused during an export.

Run the reproducible comparison after building the release plugin:

```sh
python3 tracer/otel/bench/instrument.py --repeats 5 --buffers 10000
```

It compares no tracer, `mars-otel`, metrics-only, combined metrics/traces, and
combined capture with traces unsampled. Profiles and logs are disabled for this
comparison. It decodes exports and checks every expected trace and metric count.
See [measured results](../../reports/otel/INSTRUMENT_RESULTS.md).

A separate [metrics-only comparison](../../reports/prometheus/METRICS_RESULTS.md) measures
`GST_INSTRUMENT_TRACES=false` against `mars-metrics` and no tracer, including
HTTP delivery, CPU cost per push, and peak memory on longer runs.
