# Explicit signals and Grafana correlation validation

Validated on 2026-10-24 on Linux x86-64. Runtime commit: `5f99703`.
This is a functionality and lifecycle review, not an overhead benchmark.

The README now presents three tasks: monitoring with `mars-metrics`, CPU
investigation with `mars-profiles`, and correlated investigation with
`mars-instrument`. `mars-otel` remains a compatibility tracing preset.

Explicit `signals="metrics,traces,profiles,logs"` selects collectors independently.
Metrics use the full shared collector with either OTLP or Prometheus export.
Existing no-parameter configurations retain their previous behavior. The
[user guide](../../tracer/otel/INSTRUMENT.md) explains settings, measurement
semantics and migration; the [shared collector notes](../../tracer/metrics-core/README.md)
identify the implementation boundaries.

## Pipeline and lifecycle tests

Isolated-process integration tests exercised real GStreamer hooks and decoded
actual HTTP deliveries:

- 100 buffers crossing a three-push pipeline produced exactly 300 trace spans.
  An additional buffer-list call brought the full collector's push/list count
  to 301; two pull calls, including EOS, produced two pull observations.
- OTLP and OpenMetrics exposed the full push/list/pull, queue and I/O families.
  Histogram bucket totals matched their counts. Exported/scraped exemplars
  identified actual exported trace spans.
- Disabled tracing ignored invalid trace endpoints and sampler settings.
  Disabled metrics ignored invalid metric settings. A profile URL alone did not
  enable profiles. Unsampled traces left metric counts intact without exemplars.
- Full metrics, traces, logs and CPU profiles shared matching IDs in decoded
  OTLP and pprof uploads. Explicit profiles-only mode emitted no OTLP requests.
- A rejecting metrics collector produced failure accounting. A stalled receiver
  returned a shutdown timeout within the caller's deadline; a later shutdown
  completed. A conflicting Prometheus port left the tracer uninitialized.
- Existing standalone metrics and compatibility tracing tests still passed,
  including topology changes, weak-owner retirement and profiler lifecycle tests.

The actual `GST_TRACERS='mars-instrument(signals="metrics",metrics-exporter="prometheus",metrics-port=49108)'`
launch syntax was also checked with `gst-launch-1.0`. Its endpoint served
OpenMetrics and full timing histograms.

## Grafana backend walkthrough

The [demo](../../tracer/otel/CORRELATION.md) now includes Prometheus 3.3.0 alongside
Grafana 13.1.1, Tempo 2.10.0 and Pyroscope 2.4.0. The Prometheus data source has an
exemplar link to Tempo; Tempo has a span-profile link to Pyroscope.

The `--showcase` workload sends 48 buffers through two `identity` elements on
different threads. The light stage performs 20 ms of thread CPU and waits
250 ms per frame. The heavy stage performs 100 ms of CPU, with one 500 ms frame.
Sampling is 100 Hz, with every metric observation included for this demo.

The final run delivered **192 spans**, **192 wall histogram observations** and
**616 CPU samples** across 14 profile uploads. Prometheus returned 190 stored
wall-time exemplars whose IDs and values matched the emitted observations.
The generated metric query returned a nonempty graph through Grafana.

The selected exemplar identifies span `d6547d99d838393c`, trace
`a91e8b23176f65a225825e580b9c7535`. Grafana's Pyroscope proxy returned exactly the
CPU totals decoded from the uploads:

| Query | Decoded upload | Backend |
| --- | ---: | ---: |
| Entire run | 6160 ms | 6160 ms |
| Selected span | 500 ms | 500 ms |
| Selected trace | 520 ms | 520 ms |
| Unknown span | 0 ms | 0 ms |

The selected span contains `pyroscope.profile.id`, and its profile includes
`[element] heavy` with native frames beneath it. The trace's other 20 ms belongs
to its light-stage span. The workload reported no trace or profile queue drops,
export failures, timer overruns, registration failures, failed stack reads or
truncated stacks; no profiler threads remained registered after shutdown.

Evidence:

- [Decoded exports, actual API responses, and source/binary hashes](correlation/2026-10-24.json)
- [Workload output and final counters](correlation/2026-10-24.txt)
- [Reproduction and navigation steps](../../tracer/otel/CORRELATION.md#try-the-local-demo)

A separate real Prometheus scrape check also accepted the OpenMetrics endpoint
and retained 120 wall-time exemplars. It observed 47 of the heavy stage's 48 calls
before the process shut down; an external scrape is required to capture the
last observation. OTLP's explicit final flush captured all observations in the
main check above. Neither a successful HTTP export nor this test guarantees
durable production storage.

**Browser navigation and recording remain unverified.** No browser was connected,
so these are backend/API checks, not a recorded Grafana click-through. There is
no new GIF or video to embed yet. The local demo generates fresh Explore URLs
for recording once a browser is connected; archived URLs expire with retention.
The demo does not deploy a logs backend.

## Checks and remaining measurements

Passed formatting; Rust 1.99.1 workspace/all-features/all-targets Clippy with
warnings denied; default tests; OTel tests without profiles; workspace tests
with all features; and nightly unused-dependency checks for both plugin packages
and the shared core. `cargo audit` exited zero with the existing allowed Tokio
`RUSTSEC-2026-0386` warning. Python syntax and Docker Compose configuration checked.

The existing CPU-overhead table measures the compatibility presets. The full
collector's explicit signal combinations have not yet been benchmarked. Active
series follow live pipeline objects and have no fixed 1024-group cap; the old
cap belongs to the compatibility collector. Stop streaming and flush before
dropping a pipeline to capture its final cumulative metrics. Longer production
soaks and new arm64 validation remain outside this local functional check.
