# Explicit signals and Grafana correlation validation

The [current four-signal walkthrough](#four-signal-walkthrough--2026-10-25)
adds real Loki log correlation and replaces the earlier README recording.
The initial implementation checks below remain historical results.

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

## Browser walkthrough

On 2026-10-24, a separate fresh run was followed through Grafana with
`agent-browser` 0.31.1 and headless Chrome. That earlier recording followed
actual UI navigation:

1. Click the heavy element's 0.502-second exemplar, then **Inspect trace**.
2. Open the 501.83 ms span and its attributes, including `sink_pad.element=heavy`
   and `pyroscope.profile.id=26e211d035b47311`.
3. Click **Profiles for this span**. The resulting Pyroscope query retains that
   span ID and the matching service name. Expand the flame graph's groups.
4. Observe **500 ms** of sampled CPU beneath `[element] heavy`, including
   `profile_correlation::simulated_cpu_work`.

The recording uses trace `cb3ddd89f29375d9abe15479809062ee`; its separate backend
check again matched 192 spans, 6160 ms total CPU, 500 ms for the selected span,
520 ms for the trace, and zero for an unknown span. This is a deliberately slow
demo workload, not an overhead measurement.

The browser check exposed a provisioning issue that API checks missed: the
anonymous Viewer role could query data but could not access Explore. The local
Compose demo now uses Editor access, with all ports still bound to loopback.
The generated metric query also has a readable legend.

Evidence: [recording dataset](correlation/2026-10-24-recording.json),
[browser verification and media hashes](correlation/2026-10-24-browser.json).
That video trimmed initial loading and added captions; the Grafana data and
navigation were unaltered. Its GIF was a smaller, lower-frame-rate version.
Archived Explore URLs expire with backend retention. The demo does not deploy
a logs backend.

## Checks and remaining measurements

Passed formatting; Rust 1.99.1 workspace/all-features/all-targets Clippy with
warnings denied; default tests; OTel tests without profiles; workspace tests
with all features; and nightly unused-dependency checks for both plugin packages
and the shared core. `cargo audit` exited zero with the existing allowed Tokio
`RUSTSEC-2026-0386` warning. Python syntax and Docker Compose configuration checked.

At the time of this check, the CPU-overhead table measured compatibility presets.
The newer [18-configuration matrix](INSTRUMENT_VIDEO_RESULTS.md#current-full-metrics-signal-matrix--2026-10-25)
now covers explicit signal combinations and monitoring levels. Active
series follow live pipeline objects and have no fixed 1024-group cap; the old
cap belongs to the compatibility collector. Stop streaming and flush before
dropping a pipeline to capture its final cumulative metrics. Longer production
soaks and new arm64 validation were outside this local functional check; see the
subsequent [native lifecycle results](PROFILE_VIDEO_RESULTS.md#thirty-minute-lifecycle-soaks--2026-10-25).

## Four-signal walkthrough — 2026-10-25

The [current MP4](../../docs/media/correlation.mp4) and
[README GIF](../../docs/media/correlation.gif) follow a real metric exemplar to
its factory-named span, open the matching warning log, return to the same trace,
and open that span's CPU profile. Grafana needs data-source configuration for
these links; no Grafana code changes are required.

The local stack adds Loki 3.5.0 to Grafana 13.1.1, Prometheus 3.3.0, Tempo 2.10.0
and Pyroscope 2.4.0. It uses ephemeral storage and loopback-only published ports.
The native OTLP log endpoint stores `trace_id` and `span_id` as structured
metadata. Only `service_name` is an indexed Loki stream label in this test.
The [guide and complete provisioning](../../tracer/otel/CORRELATION.md)
document both trace-to-log and log-to-trace links.

The controlled 48-buffer pipeline still runs light and heavy `identity` elements
on opposite sides of a queue. Each handoff emits one GStreamer message inside
its sampled span. The heavy element's 25th frame deliberately does 500 ms of
thread CPU work and logs:

```text
slow frame: element=heavy frame=25 cpu_work_ms=500
```

Two additional messages outside sampled calls provide an uncorrelated control.
The source waits before sending frame 26 so the slow observation remains in the
bounded recent-exemplar slot for a metric export interval. This is demo pacing,
not a change to the collector's exemplar retention or an overhead benchmark.

The successful recording dataset delivered **192 spans**, **192 wall histogram
observations**, **192 stored wall-time exemplars**, **98 logs**, and **616 CPU
samples** in 14 profile uploads. All 96 handoff logs retained their native
trace/span IDs; the two outside-call logs had neither ID. Every log's timestamp
and body matched the emitted OTLP record when queried through Grafana's Loki
proxy. The workload reported zero trace/log/profile queue losses and export
failures, and zero sampler overruns, truncated stacks, failed reads or
registration failures. No sampler registrations remained after shutdown.

The chosen metric observation was **500.071645 ms**, and its inclusive
`gst.pad.push identity` span lasted **500.085652 ms**. The span includes downstream
work subtracted by the synchronous wall metric, and measurement boundaries
differ slightly. The log
timestamp falls inside that span. Pyroscope's returned totals match the decoded
uploads:

| Selection | Decoded CPU | Queried CPU |
| --- | ---: | ---: |
| Whole run | 6160 ms | 6160 ms |
| Selected span | 500 ms | 500 ms |
| Selected trace | 520 ms | 520 ms |
| Unknown span | 0 ms | 0 ms |

The Tempo-to-Loki link uses the selected service, trace ID and span ID, with a
one-second expansion around each span boundary. It returns exactly one warning.
An unknown trace, unknown span, and a real span from an unrelated trace each
return zero logs. The reverse link reads native structured metadata rather than
extracting IDs from message text. Both the short 16-span/10-log check and the
full recording check passed.

The separate [browser recorder](../../tracer/otel/bench/correlation/record.py)
uses agent-browser 0.31.1 and headless Chrome with an isolated browser profile.
It verifies the actual exemplar link, the generated log query and exact
millisecond time range, the return trace ID, and Pyroscope's service/span
selection. It also opens unknown-trace and unrelated-span controls in Grafana
and requires “No logs found.” The final flame graph exposes `[element] heavy
[identity]` and `profile_correlation::simulated_cpu_work` under the selected
500 ms total.

The [renderer](../../tracer/otel/bench/correlation/render.py) adds captions and
trims initial loading. It aligns captions with captured screenshots because
screencast frame timing can differ from CLI wall time. The recorded Grafana
content and navigation are unchanged; the GIF has lower resolution and frame
rate. These local Explore URLs expire with backend retention.

Evidence includes the [backend dataset](correlation/2026-10-25/recording.json),
[workload counters](correlation/2026-10-25/workload.log),
[browser assertions](correlation/2026-10-25/browser-check.json),
[capture events](correlation/2026-10-25/capture-events.json), and
[media hashes/timing](correlation/2026-10-25/media.json). Screenshots show the
[factory-named span](correlation/2026-10-25/selected-span.png),
[matching log and native IDs](correlation/2026-10-25/matching-log.png), and
[element-attributed profile](correlation/2026-10-25/profile.png).
The dataset records base revision `e51d653` plus hashes of the working-tree
implementation, example, and configuration used for the run.

Formatting, workspace all-feature Clippy, default and workspace all-feature
tests, audit, nightly dependency-use checks, and all-target OTel Clippy passed.
Audit retains the existing allowed Tokio `RUSTSEC-2026-0386` warning. The Compose
stack, Python scripts, real backend checks and browser navigation were exercised
locally. This functional demo does not measure production backend retention,
durability, GPU execution, or application overhead.
