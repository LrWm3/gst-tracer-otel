# Correlated telemetry with mars-instrument

Use `mars-instrument` to connect a slow measurement to its trace, CPU profile,
and GStreamer logs. Choose the signals you need, then choose a metrics exporter.

```sh
cargo build --release -p gst-otel-tracer --features profiles

GST_PLUGIN_PATH="$PWD/target/release" \
GST_TRACERS='mars-instrument(signals="metrics,traces,profiles")' \
OTEL_SERVICE_NAME=video-pipeline \
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318 \
MARS_PROFILE_URL=http://localhost:4040 \
  gst-launch-1.0 videotestsrc num-buffers=300 ! videoconvert ! fakesink
```

The OTLP endpoint needs metrics and traces receivers; Pyroscope receives profiles.
For good native stacks, use frame pointers and debug symbols as described in the
[profiling guide](PROFILES.md). Profiles require Linux x86-64 or arm64, GStreamer
1.22 or newer, and the `profiles` build feature. Other signals do not.

## Pick signals and destinations

| Task | `GST_TRACERS` |
| --- | --- |
| Prometheus metrics | `mars-instrument(signals="metrics",metrics-exporter="prometheus",metrics-port=9188)` |
| OTLP metrics | `mars-instrument(signals="metrics")` |
| Traces | `mars-instrument(signals="traces")` |
| CPU profiles | `mars-instrument(signals="profiles")` |
| Metrics, traces and profiles | `mars-instrument(signals="metrics,traces,profiles")` |
| Include GStreamer logs too | `mars-instrument(signals="metrics,traces,profiles,logs")` |

Metrics default to OTLP. With `metrics-exporter="prometheus"`, scrape
`http://localhost:9188/metrics`; the server listens on all interfaces. Both
exporters use the **same full collector** as `mars-metrics`: synchronous push,
buffer-list and pull timing, queue pressure, and optional I/O and PTS-age metrics.
Changing the exporter does not change measurement or sampling semantics.

Instead of tracer parameters, set `GST_TRACERS=mars-instrument` and
`MARS_SIGNALS=metrics,traces,profiles`. Parameters take precedence over their
environment equivalents. An explicit signal list is exhaustive: a configured
profile or log endpoint does not enable that signal. Unselected signals do not
create their collectors or exporters, and their settings are not validated.
Duplicate or unknown signals and unknown tracer parameters produce an error in
`GST_DEBUG=mars-otel:3` and leave the tracer disabled.

| Setting | Default | Purpose |
| --- | --- | --- |
| `MARS_SIGNALS` | Unset: compatibility behavior | Comma-separated `metrics,traces,profiles,logs` |
| `MARS_METRICS_EXPORTER` | `otlp` | `otlp` or `prometheus`; parameter `metrics-exporter` |
| `MARS_METRICS_PORT` | `9188` | Prometheus listen port; parameter `metrics-port` |
| `MARS_MONITORING` | `core` | Instrumentation health: `none`, `core`, or `all`; parameter `monitoring` |
| `MARS_METRIC_INTERVAL_MS` | `10000` | OTLP metrics export interval, 1–60000 ms |
| `MARS_METRIC_SAMPLE_RATE` | `16` | Approximately one in N calls enters timing histograms |
| `MARS_METRIC_INCLUDE` | `*` | Comma-separated element-name glob filters |
| `MARS_METRIC_QUEUE_INTERVAL_MS` | `100` | Queue sampling period |
| `MARS_METRIC_IO_COUNTS` | `false` | Per-pad buffer input/output counters |
| `MARS_METRIC_PTS_AGE` | `false` | Sample buffer age relative to running time |
| `MARS_PROFILE_URL` | `http://localhost:4040` when selected | Pyroscope endpoint |
| `MARS_PROFILE_HZ` | `100` | CPU sampling frequency, 1–1000 Hz |
| `MARS_PROFILE_ELEMENTS` | `true` | Add element/task-owner roots to profiles |
| `MARS_PROFILE_SYMBOLIZATION` | `full` | `full` source/inline resolution or `functions` using ELF symbols only |
| `MARS_PROFILE_SIGNAL` | `SIGRTMIN+6` | Reserved realtime sampling signal |

`MARS_METRIC_*` collector controls also work with `mars-metrics`. Their original
`GST_PROM_LATENCY_*` names remain fallbacks (including
`GST_PROM_LATENCY_HISTOGRAM_SAMPLE_RATE` and
`GST_PROM_LATENCY_QUEUE_SAMPLE_INTERVAL_MS`). Profile controls fall back to
`GST_OTEL_PROFILE_*`. The new namespace takes precedence.

Traces and OTLP metrics retain the standard
`OTEL_EXPORTER_OTLP_{TRACES,METRICS}_{ENDPOINT,HEADERS,TIMEOUT,PROTOCOL}` settings,
with fallback to generic OTLP settings. Signal-specific endpoint URLs are used
literally. Use `http/protobuf`; compression defaults to `none`, with optional
`OTEL_EXPORTER_OTLP_COMPRESSION=gzip`. Profiles and OTLP signals
share `OTEL_SERVICE_NAME`; other OTLP resource fields come from
`OTEL_RESOURCE_ATTRIBUTES`. Trace sampling and queue controls are described in
the [trace guide](README.md#configuration).

For logs, also select useful `GST_DEBUG` categories. The log bridge preserves
GStreamer's normal log handler. Profile collection is independent of trace
sampling; unsampled work still appears in profiles without trace IDs.

## Monitor the instrumentation

Select `monitoring=none`, `monitoring=core`, or `monitoring=all` independently of
the application signals. The default is `core`:

```sh
GST_TRACERS='mars-instrument(signals="metrics,traces",monitoring=core)'
```

| Level | Health data |
| --- | --- |
| `none` | No health series or optional diagnostic timing. Correctness counters and explicit status APIs still work. |
| `core` | Delivery outcomes, export warnings/failures, pending work and capacity, capture failures, profile losses/capacity failures, and cleanup failures. |
| `all` | Core plus worker-duration histograms, encoded bytes and record totals, queue high-water marks, symbol-cache activity/retained size, and detailed trace/profile counters. |

Health series use the `mars_instrument_` prefix. Labels have fixed values such as
signal, outcome, stage, and failure reason; they never contain element paths,
trace IDs, or raw error messages. Core reuses existing counters and creates its
metric snapshots off streaming threads. Detailed timing and queue observations
are collected only in `all`. Profile `reported` samples mean handoff to the SDK,
not acknowledgement by the server; symbol-cache bytes exclude temporary debug
parsing and total process memory.

Health data follows the selected metrics exporter. With metrics disabled,
monitoring is inactive and does not create an exporter or listener. Standalone
`mars-metrics` accepts the same parameter/environment enum and exports collector
health through its existing HTTP/signal output. The compatibility instrumentation
preset also honors `MARS_MONITORING`. Configuration is process-wide.

The [local Grafana demo](CORRELATION.md) provisions a **Mars instrumentation
health** dashboard and [Prometheus alert rules](bench/correlation/health-alerts.yaml)
for telemetry loss, a backlog above 80% of queue capacity for five minutes,
profile capacity/sample loss, and cleanup failures. Detailed panels require
`all`. These are starter alert rules; choose durations appropriate to the
application and add backend/target availability alerts. Exporting health through
an unavailable backend cannot notify you until delivery recovers. The demo loads
rules but does not configure outbound notifications.

Pipeline-stop failures and explicit flush/shutdown results update cleanup
counters. Failures occurring after the final metrics export remain inspectable
through per-signal drain results and status APIs rather than being guaranteed
to appear in that final export.

## Follow a measurement

1. A histogram exemplar carries the trace and span IDs of an observed push.
2. Tempo shows the inclusive push call and its downstream spans.
3. **Profiles for this span** shows CPU samples taken during that span, grouped
   by element instance. Logs emitted inside the push carry the same IDs.

Follow the [Grafana setup and runnable demo](CORRELATION.md) to configure these
links. Backend addresses alone do not configure Grafana navigation.

Full metrics use the names and labels in the
[metrics reference](../prometheus/README.md#metric-semantics). For example,
`gst_element_sync_wall_seconds` measures exclusive synchronous time, with
`element`, `src_pad`, `sink_pad`, and `path` labels. Push labels describe the
callee. Histograms are sampled; the legacy wall-time counters cover every
measured call. Queue residence, GPU completion and end-to-end latency are
different measurements.

OTLP cumulative histograms preserve the same buckets, counts, sums and labels.
Prometheus export uses OpenMetrics 1.0 so it can carry exemplars. Counter sample
names use the standard `_total` suffix, as does normal OTLP-to-Prometheus
translation: for example, the legacy `gst_element_latency_count_count` becomes
`gst_element_latency_count_count_total`. `mars-metrics` keeps its existing text
format and names. No trace or span IDs are added to metric-series labels.

Ordinary pushes, buffer-list pushes, and pull reads produce trace spans and
metric exemplars.
A top-level `gst.pad.pull_range` span starts a trace before the buffer exists,
then attaches context to the returned buffer for downstream pushes. This covers
file sources read by demuxers without adding a queue to force push scheduling.
Nested reads inherit the active span; EOS ends the read normally. Reads can
contain part of a frame or several frames, so traces are not guaranteed to map
one-to-one to video frames. Elements must preserve buffer metadata across
asynchronous boundaries. A buffer-list push creates one `gst.pad.push_list`
span with `buffer.list.length` and total `buffer.size`, then propagates that
context to its members. A later individual push can inherit the list span.
Batches and outputs combining input metadata preserve one parent and up to eight
span links for other known inputs. Links do not change sampling decisions or
recover metadata that an element discarded; see [correlation boundaries](README.md#correlation-boundaries).
A locally rejected trace
cannot replace an exemplar. Later export failure or backend retention can still
leave a missing trace, and short spans may have no CPU samples.

## Lifecycle and resource use

Collection updates cached aggregates. Encoding and network I/O happen on a
worker. Exemplars use a nonblocking update and retain one recent sample per
histogram. The profiler's signal handler never touches metric locks.

Full-metrics cardinality follows live pipeline objects, as in `mars-metrics`;
it does not use the old four-histogram collector's 1024-group cap. Destroyed or
renamed objects retire their series. Flush **before dropping the pipeline** if
you need its final cumulative totals. Failed deliveries retry the next snapshot
without a growing event queue; totals for retired series are not retained for
later delivery. Snapshots become exact after streaming stops.

READY→NULL flushes traces and OTLP metrics within `GST_OTEL_FLUSH_TIMEOUT_MS`
(default 10000). Rust `flush(timeout)` and `shutdown(timeout)` share their deadline
across exporters; shutdown also stops queue sampling and maintenance. Stop
streaming first. A timed-out worker can finish in the background. Prometheus
flush does not cause an external scrape; scrape before shutdown. Restarting the
process is required to restart telemetry after shutdown.

With `GST_DEBUG=mars-otel:5`, pipeline stop reports pending spans and the time
spent draining traces/metrics and logs. A full trace queue can take several
seconds to drain over a slow connection; `dropped_queue_full` counts spans lost
before that drain. Reduce trace sampling or improve export throughput if these
counters grow. Shortening the flush timeout only limits the wait; it does not
make pending telemetry arrive sooner.

`stats()` reports trace accounting and `profile_stats()` reports sampler
accounting. For full metrics, `metric_stats()` reports completed measured push
and buffer-list calls, successful deliveries/scrapes, failed deliveries, and the
number of series in the latest snapshot. The old capacity/invalid/exemplar-drop
fields apply only to the compatibility collector and remain zero in this mode.

## Existing configurations and measured cost

The simpler `mars-metrics` and `mars-profiles` entry points remain available.
`mars-otel` is a compatibility tracing preset. Choose one telemetry entry point
per process; adding another does not change the first one's selected signals.
Do not combine `mars-metrics` with full metrics in `mars-instrument`, which would
measure the same work twice.

Plain `GST_TRACERS=mars-instrument` without `MARS_SIGNALS` retains its previous
four OTLP push histograms, defaults, limits and `GST_INSTRUMENT_*` switches.
See [compatibility configuration](LEGACY_INSTRUMENT.md). Opt into the full
collector with an explicit signal list, and update dashboards to its metric
names and labels. The old histogram limits and CPU-only sample setting do not
configure the full collector.

Existing [video overhead measurements](../../reports/otel/INSTRUMENT_VIDEO_RESULTS.md)
cover those compatibility defaults. Full-metrics signal configurations have not
yet been benchmarked; their costs should be measured on the target workload.
Tests exercise both exporters, actual list/pull/queue measurements, trace links,
disabled settings, collector rejection, and bounded shutdown. Feature-enabled
tests also match exemplars to real uploaded CPU samples and log records.
