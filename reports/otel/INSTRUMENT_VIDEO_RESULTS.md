# Combined instrumentation on 1080p video

Measured 2026-10-23. For a rough planning figure on this workload, use **about
1% extra CPU cycles for default `mars-instrument` metrics and traces**, and
**about 3% with profiles and logs enabled**. Added peak RSS was about **5 MiB**
and **45 MiB**, respectively. These are workload-specific CPU-work observations,
not measured FPS loss or CPU-time guarantees.

The default mode's measured median increase was **0.4%**, within the baseline's
run-to-run variation. Rounding to about 1% avoids implying a reliable sub-percent
limit. Fully enabled instrumentation measured **2.7%**. The matched `mars-otel`
controls measured **0.8%** for traces and **3.2%** with profiles/logs. Their ranges
overlap the corresponding `mars-instrument` ranges; these results do not show
that adding metrics makes tracing faster.

## Measured costs

Five-run medians, each compared with this series' no-tracer median. All modes use
the same binary and the default trace-export settings; profiles are 100 Hz,
traces are fully sampled, and enabled logging emits one INFO record per frame.

| Mode | CPU cycles (billions) | Added cycles | Added instructions | Peak RSS (MiB) | Added RSS (MiB) |
| --- | ---: | ---: | ---: | ---: | ---: |
| No tracer | 52.50 | +0.0% | +0.00% | 26.1 | 0.0 |
| `mars-otel`: traces | 52.93 | +0.8% | +0.06% | 30.9 | 4.9 |
| `mars-otel`: traces + profiles + logs | 54.20 | +3.2% | +0.73% | 70.3 | 44.3 |
| `mars-instrument`: metrics + traces | 52.71 | +0.4% | +0.06% | 31.4 | 5.3 |
| `mars-instrument`: metrics + traces + profiles + logs | 53.91 | +2.7% | +0.85% | 70.8 | 44.7 |

The earlier [default-settings profile check](PROFILE_VIDEO_RESULTS.md#default-export-settings)
measured 3.6% added cycles and 44 MiB for OTel with profiles/logs, consistent
with using a rough **3–4%** figure across these two series. Its profiles-only
result remains **about 1% and 41 MiB**. Do not subtract medians from separate
series to estimate the cost of an individual signal.

## Timing variation and shutdown

| Mode | CPU cycles range (billions) | Process CPU median / range (s) | Median pipeline FPS | Median shutdown (ms) |
| --- | ---: | ---: | ---: | ---: |
| No tracer | 51.78–52.77 | 12.45 / 12.14–15.94 | 96.3 | 0.9 |
| `mars-otel`: traces | 52.39–55.22 | 13.19 / 12.67–15.67 | 91.4 | 2.1 |
| `mars-otel`: traces + profiles + logs | 53.71–56.62 | 12.91 / 12.77–17.56 | 94.5 | 8.2 |
| `mars-instrument`: metrics + traces | 52.56–53.26 | 12.65 / 12.46–16.50 | 95.3 | 2.9 |
| `mars-instrument`: metrics + traces + profiles + logs | 53.78–54.14 | 12.85 / 12.62–15.47 | 95.1 | 6.8 |

Identical no-tracer work varied from 12.14 to 15.94 CPU seconds. Timing differences
therefore cannot resolve small overhead percentages reliably. Hardware cycles
reduce sensitivity to clock frequency, but still include cache and scheduling
effects. The baseline and both default tracing modes have overlapping cycle
ranges. These five-run results are approximate costs, not statistical upper bounds.
No measured runs were discarded.

`mars-instrument` made a median 66 HTTP requests in default mode and 92 with
profiles/logs enabled. Median process CPU outside the source thread was 74 ms
and 291 ms, respectively; this includes exporter and profile worker activity,
initial state changes, and final cleanup. The source-thread measurement omits
first-frame generation, so this difference is only an approximate indicator of
background cost. Whole-process counters include that work directly.

## Delivery and correlation

All **25 measured runs** and five warm-ups passed validation. Measured runs
produced:

- **96,000 spans**, with exact started/completed/exported counts and no duplicate IDs.
- **48,000 metric pushes**, exactly represented in each of the inclusive and
  exclusive wall histogram families. CPU and blocked histograms contained the
  same **2,980 sampled observations**, approximately one call in 16.
- **320 metric exemplars**, each identifying an actual exported span. Histogram
  bucket counts matched point counts, and final cumulative snapshots matched
  the expected per-pad totals without double-counting periodic exports.
- **12,000 logs**, with every frame represented exactly once in each logging run
  and every record's trace/span fields matching a delivered span.
- **13,566 CPU samples**, with decoded pprof weights exactly matching sampler
  counters. All **4,030 correlated samples** matched exported spans and their
  `pyroscope.profile.id` attributes.

Across fully enabled instrumentation runs, **seven distinct exemplar spans**
(summed per process) also had actual CPU samples. Short spans often have no
100 Hz sample, so a metric exemplar is not guaranteed to have a corresponding
profile. The focused integration test also exercises the complete
metric → trace → profile/log relationship with controlled work.

No trace, metric, sample-buffer, processing-queue, or upload-queue drops,
metric export failures, skipped metric exemplars, registration failures, timer
overruns, failed stack reads, or truncated stacks were reported. Every shutdown
completed; the slowest fully enabled `mars-instrument` shutdown was 11.5 ms.
No hangs were observed in these short runs. They do not establish the absence of
rare deadlocks or long-term memory growth. Receiver acknowledgements establish
HTTP delivery, not durable storage by a production backend.

## Workload and method

Measured 2026-10-23 on an Intel Core i5-1345U, Linux x86-64, stock GStreamer
1.24.2, Rust 1.89.0, and perf 7.0.14. The tracer implementation is unchanged
from `6fa72ad`; this change extends the benchmark driver to instantiate
`InstrumentTracer` and validate its metrics output. All modes use the same
release binary built with `-C force-frame-pointers=yes`.

Each fresh process handles 1,200 frames, with one streaming thread and no clock
pacing:

```text
videotestsrc name=source0 num-buffers=1200 pattern=snow !
video/x-raw,format=RGBA,width=1920,height=1080 ! videoconvert !
video/x-raw,format=I420 ! fakesink name=sink0 sync=false async=false
```

The caps filters make four pushes per frame: 4,800 trace spans and, for
`mars-instrument`, 4,800 observations in each wall histogram family per run.
Noise generation contributes substantial CPU outside active push spans. This
is a CPU-heavy conversion pipeline, without decoding, inference, GPU work, or
queue boundaries. It does not estimate the overhead of a lightweight graph
with many pushes per second.

Each of the five modes gets one discarded warm-up and five measured runs.
Execution order rotates so each mode occupies each position once. Children run
on CPUs 0 and 2 (separate physical P cores); the local receiver runs on CPU 4.
The frequency governor is `powersave`, frequency is not pinned, and other
desktop services remain active. No builds or test suites run during timing.
Every child starts with a cold symbol cache.

The configuration uses default trace and metric export settings:

- 100% root trace sampling, 8,192 queued records, batches of 512, 200 ms trace
  export interval.
- Metrics enabled for `mars-instrument`, cumulative histograms every 10 seconds,
  and independent CPU/blocked sampling at approximately one call in 16. Every
  supported push contributes to both wall histogram families.
- Optional CPU profiles at 100 Hz. Optional logs emit one INFO message per frame
  from the sink-pad probe, inside a traced push. The default stderr handler
  remains enabled, with its output captured by the parent. Other debug categories
  are disabled. Disabled modes execute the same log call.

The driver constructs the same Rust types as the plugin factories, then enters
NULL and calls `gstoteltracer::shutdown()` explicitly. The local HTTP receiver
acknowledges and stores actual OTLP and Pyroscope requests; protobuf and pprof
validation happens after the child exits. Receiver memory and CPU, remote storage,
and Grafana are outside these measurements.

**CPU cycles and instructions** cover the entire child process and its threads,
including initialization, export, symbolization, and shutdown. `perf stat` must
report at least 99% counter scheduling coverage. Added cycles are the difference
between each mode's median and this series' no-tracer median, divided by the
no-tracer median. They are not percentage points of host CPU utilization.

**Process CPU** covers PLAYING through NULL and explicit telemetry shutdown;
`total_cpu_ns` additionally includes initialization. **Source CPU** starts at
the first source buffer probe and ends at EOS, excluding generation of the first
frame. **Wall time/FPS** ends at EOS and excludes shutdown. **Peak RSS** is the
child's `/proc/self/status` `VmHWM`; added RSS is the difference between medians.
The raw CSV retains all measurements and warm-ups (`repeat=-1`).

## Reproduce

From the repository root, choosing CPU affinities available on your host:

```sh
RUSTFLAGS="-C force-frame-pointers=yes" cargo build --release \
  -p gst-otel-tracer --features profiles --example profile_video \
  --target-dir target/profile-video-build

taskset -c 4 python3 tracer/otel/bench/profile_video.py \
  --perf --cpus 0,2 --frames 1200 --repeats 5 \
  --modes none otel otel-profiles-logs instrument instrument-profiles-logs \
  --export-interval-ms 200 --queue-capacity 8192 \
  --output reports/otel/2026-10-23-instrument-video.csv
```

See the [raw CSV](2026-10-23-instrument-video.csv),
[environment and build metadata](2026-10-23-instrument-video.json),
[runner](../../tracer/otel/bench/profile_video.py), and
[driver](../../tracer/otel/examples/profile_video.rs). Metadata records the base
revision plus hashes of the modified driver, runner, decoder, lockfile, and binary.
Per-run diagnostics are written to `target/profile-video-runs/`.

For metrics-only capture and a busier push workload, see the separate
[metrics-only report](../prometheus/METRICS_RESULTS.md) and
[original combined-instrumentation comparison](INSTRUMENT_RESULTS.md).
The [profile comparison](PROFILE_VIDEO_RESULTS.md) includes a separate
profiles-only measurement; its raw results remain unchanged.

## Validation

The benchmark smoke check and recorded series passed. Repository checks also
passed: `cargo fmt --all`, Clippy on Rust 1.99.1 with all workspace features and
targets and warnings denied, `cargo test`, the profiles-enabled OTel test suite,
`cargo audit`, and nightly `cargo udeps` for both tracer packages. Audit exited
successfully with the existing allowed Tokio `RUSTSEC-2026-0386` warning.

## Instrumentation monitoring — 2026-10-25

Compared `MARS_MONITORING=none|core|all` on the same release binary and 240-frame, 1080p RGBA-to-I420 workload. Each case had one excluded warm-up and five measured repetitions, with rotating order and CPU affinity 0,2. Metrics were sampled at 1/16 calls and exported every second; traces were fully sampled. Standalone metrics were scraped once per second. Receivers ran outside the measured process.

| Selected signals / collector | Monitoring | Median cycles (billions) | Cycle change vs `none` | Median process CPU | CPU range | Median peak RSS | Median shutdown |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Full OTLP metrics | `none` | 10.784 | +0.00% | 3.200 s | 2.418–5.469 s | 30.39 MiB | 3.33 ms |
| Full OTLP metrics | `core` | 10.693 | -0.84% | 3.226 s | 2.430–4.008 s | 30.43 MiB | 3.36 ms |
| Full OTLP metrics | `all` | 10.549 | -2.18% | 2.656 s | 2.419–4.120 s | 30.61 MiB | 2.92 ms |
| Full OTLP metrics + traces | `none` | 10.892 | +0.00% | 2.675 s | 2.511–4.411 s | 32.13 MiB | 2.90 ms |
| Full OTLP metrics + traces | `core` | 10.695 | -1.81% | 2.694 s | 2.432–3.761 s | 32.07 MiB | 3.37 ms |
| Full OTLP metrics + traces | `all` | 10.753 | -1.28% | 2.598 s | 2.409–3.806 s | 32.24 MiB | 3.35 ms |
| Standalone Prometheus metrics | `none` | 10.567 | +0.00% | 2.548 s | 2.371–3.264 s | 27.52 MiB | 0.93 ms |
| Standalone Prometheus metrics | `core` | 10.528 | -0.37% | 2.560 s | 2.387–3.278 s | 27.53 MiB | 1.18 ms |
| Standalone Prometheus metrics | `all` | 10.598 | +0.29% | 2.564 s | 2.411–3.115 s | 27.58 MiB | 1.11 ms |

These differences overlap run variation; this test does not establish a sub-percent monitoring overhead or a speedup. CPU time varied substantially with host conditions, so the cycle comparison is more useful here. These are changes relative to the same instrumented collector with monitoring disabled, **not total tracer overhead**. The cases do not include logs or profiles. All completed with zero trace loss, pending spans, and metric export failures.

`core` adds health payloads to the existing export schedule. `all` adds fixed-size counters/histograms and optional timing; snapshots and encoding run outside streaming threads. Both retain the same queue bounds. Peak RSS is measured for the entire short process, not isolated monitoring allocations or settled long-running memory. Standalone shutdown measures pipeline teardown; its embedded HTTP server exits with the process.

Raw [CSV](monitoring/2026-10-25/results.csv), [build/settings metadata](monitoring/2026-10-25/metadata.json), and per-run diagnostics are retained. The build starts from `3a1bf2d` with the monitoring implementation; metadata records binary/plugin hashes and the source diff hash. Hardware events on hybrid CPU types are summed only when their scheduling coverage is at least 99%.

```sh
cargo build --release -p gst-prometheus-tracer -p gst-otel-tracer --features gst-otel-tracer/profiles
cargo build --release -p gst-otel-tracer --features profiles --example profile_video
python3 tracer/otel/bench/monitoring.py --perf --frames 240 --repeats 5 \
  --output reports/otel/monitoring/recheck
```

Validation includes enum/default/routing tests for OTLP and Prometheus, real failed trace exports reflected in health metrics, all three standalone monitoring levels, five Prometheus alert-rule tests (plus a healthy backlog control), dashboard provisioning, and actual health series received by Prometheus. Required repository formatting, Clippy, tests, audit and unused-dependency checks passed; audit retains the existing allowed Tokio advisory.
