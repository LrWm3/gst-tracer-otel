# Current profiling and log overhead on 1080p video

Measured 2026-10-23. This report checks the current `mars-profiles` entry point
and `mars-otel` with profiles and logs enabled, including actual delivery and
correlation. It replaces the older prototype figures as the current profiling
comparison; the older measurements remain available as historical results.

## Default export settings

For a rough README figure on this workload, use **about 1% added CPU cycles and
41 MiB added peak RSS for profiles-only**, and **about 4% and 44 MiB for
`mars-otel` with profiles and logs**. These are CPU-cycle observations, not a
measured FPS loss or a general CPU-time guarantee. Profiling is 100 Hz and
logging emits one INFO record per frame. The baseline and profiles-only cycle
ranges overlap, so the sub-percent point estimate should not be treated as a
precise limit.

This separate check uses the default **200 ms trace export interval** and
**8,192-record queue**, with six repetitions per mode and the same driver binary.
Medians, compared only against this series' no-tracer baseline:

| Mode | CPU cycles (billions) | Added cycles | Added instructions | Peak RSS (MiB) | Added RSS (MiB) |
| --- | ---: | ---: | ---: | ---: | ---: |
| No tracer | 52.48 | +0.0% | +0.00% | 26.3 | 0.0 |
| `mars-profiles` | 52.84 | +0.7% | +0.42% | 67.4 | 41.1 |
| `mars-otel`: traces + profiles + logs | 54.35 | +3.6% | +0.60% | 70.2 | 43.9 |

| Mode | CPU cycles range (billions) | Process CPU median / range (s) | Median pipeline FPS |
| --- | ---: | ---: | ---: |
| No tracer | 51.95–53.48 | 13.12 / 12.36–17.76 | 91.5 |
| `mars-profiles` | 52.05–54.57 | 13.92 / 12.57–15.90 | 87.0 |
| `mars-otel`: traces + profiles + logs | 53.17–55.02 | 13.15 / 12.86–20.01 | 92.4 |

The default fully enabled mode made a median **92 HTTP requests** per run,
versus **1,167** with the earlier benchmark settings, and used **70.2 MiB** total
peak RSS rather than **77.1 MiB**. These are separate series on a shared host;
the small cycle difference between them is not evidence that faster export is
cheaper. CPU outside the source thread fell from a median 516 ms to 240 ms,
which includes trace/log export and profile processing, not just HTTP work.

All **18 measured runs** and three warm-ups passed validation, delivering
**28,800 spans**, **7,200 logs**, and **16,942 CPU samples** in the measured runs.
All **2,636 correlated samples** matched exported spans. There were no sample
losses, timer overruns, failed stack reads, truncated snapshots, missing logs,
or shutdown errors.

## Matched comparison with the earlier benchmark settings

Seven-run medians. Profiling is 100 Hz; traces are fully sampled; enabled logging
emits one INFO message per frame. This series uses a **10 ms trace export
interval and a 65,536-record queue**, matching the earlier profiling benchmark.
Those differ from the plugin defaults of 200 ms and 8,192 records. Peak memory
and export costs below are specific to this configuration.

| Mode | CPU cycles (billions) | Added cycles | Added instructions | Process CPU (s) | Peak RSS (MiB) |
| --- | ---: | ---: | ---: | ---: | ---: |
| No tracer | 52.47 | +0.0% | +0.00% | 12.74 | 26.2 |
| Native logs, no tracer | 53.03 | +1.1% | +0.01% | 12.81 | 26.5 |
| `mars-profiles` | 53.10 | +1.2% | +0.55% | 12.66 | 67.7 |
| `mars-otel`: traces | 53.55 | +2.1% | +0.15% | 14.80 | 37.9 |
| `mars-otel`: traces + logs | 54.31 | +3.5% | +0.17% | 12.50 | 38.5 |
| `mars-otel`: traces + profiles | 54.47 | +3.8% | +0.61% | 13.05 | 76.5 |
| `mars-otel`: traces + profiles + logs | 54.27 | +3.4% | +0.75% | 12.87 | 77.1 |

Profiles-only added **1.2% cycles** and **41.6 MiB** peak RSS in this series.
`mars-otel` with profiles and logs added **3.4% cycles** and **50.9 MiB** peak RSS.
These are observed medians, not precise overhead bounds. The native-log control
also differs by 1.1% in cycles, and the profiles-only and baseline cycle ranges
overlap. Small differences between tracing configurations do not establish that
adding a signal makes tracing faster.

CPU-time and throughput rankings are inconsistent with hardware work: for
example, traces-only appears much slower than traces plus profiles and logs.
Do not use those timings as a claim that adding profiles improves throughput,
or label the cycle percentages as FPS loss. The full observed ranges are:

| Mode | CPU cycles range (billions) | Process CPU range (s) | Median pipeline FPS |
| --- | ---: | ---: | ---: |
| No tracer | 51.99–53.61 | 12.28–16.46 | 94.2 |
| Native logs, no tracer | 52.35–53.48 | 11.75–15.84 | 93.6 |
| `mars-profiles` | 51.99–53.65 | 12.26–16.61 | 95.5 |
| `mars-otel`: traces | 52.92–53.70 | 12.54–17.06 | 82.8 |
| `mars-otel`: traces + logs | 53.35–54.42 | 12.24–17.91 | 98.0 |
| `mars-otel`: traces + profiles | 53.97–55.42 | 12.23–16.39 | 95.2 |
| `mars-otel`: traces + profiles + logs | 53.80–55.92 | 12.74–16.73 | 96.1 |

The measured no-tracer CPU-time range spans 12.28–16.46 seconds for identical
work, while its cycle range spans 51.99–53.61 billion. Frequency, scheduling,
and shared-host effects prevent a reliable small wall-time or CPU-time
percentage. Hardware counters reduce one source of uncertainty; they do not
remove cache, scheduling, or host variability.

## Output and shutdown checks

All **49 measured runs** and seven warm-ups passed output validation. The measured
runs delivered **134,400 spans**, **16,800 logs**, and **27,969 CPU samples**.
All **5,720 correlated samples** referred to exported spans with matching profile
IDs, and every exported log referred to a delivered span. Profiles-only produced
no OTLP requests and no trace/span sample labels. No-tracer modes produced no
telemetry. Exact per-frame log sequences and duplicate span IDs were checked.

No sample-buffer, processing-queue, or upload-queue drops, registration failures,
timer overruns, stack-read failures, or truncated snapshots were reported in
these runs. Final decoded profile weights exactly matched sampler counters.
All shutdowns completed; there were no observed hangs or missing final samples.
This short test does not establish long-term leak freedom or absence of rare
deadlocks. Trace queues also reported no pending, failed, or capacity-dropped spans.

## Timing variability and the pilot

The timing-only pilot was stopped after 26 completed runs because process CPU
varied too much for a useful small-overhead estimate. Its two measured no-tracer
runs took 16.39 and 18.11 CPU seconds, while other modes ranged from 12.58 to
26.19 seconds. The [pilot CSV](2026-10-23-profiles-video-pilot.csv) and
[metadata](2026-10-23-profiles-video-pilot.json) are retained; they are excluded
from the final table.

The final full series also records inherited hardware `cycles` and
`instructions` using `perf stat`. These cover the whole child process and all
its threads, including initialization and shutdown. Counters must have at least
99% scheduling coverage; unscheduled hybrid-CPU counter instances are ignored.
The hardware totals are useful alongside CPU-time measurements when clock
frequency varies. They are not a guarantee of identical throughput on another
machine. Every timing and hardware measurement in the final table comes from
the same instrumented series.

## Workload and method

Measured 2026-10-23 on an Intel Core i5-1345U, Linux x86-64, stock GStreamer
1.24.2 and Rust 1.89.0. The implementation is from `4377158`; this change adds
the benchmark driver and reports without changing tracer code. Every mode uses
the same release binary built with `-C force-frame-pointers=yes`. The driver
instantiates the same Rust tracer types as the plugin factories, allowing an
explicit `gstoteltracer::shutdown()` and exact checks of final sampler counters.

Each child generates 1,200 frames of 1920×1080 RGBA noise, converts to I420,
and discards the result without clock pacing:

```text
videotestsrc pattern=snow ! RGBA,1920×1080 ! videoconvert ! I420 ! fakesink sync=false
```

The two caps filters make four pushes per frame: 4,800 spans when tracing is
active. There is one streaming thread, no queue, decoding, inference, or GPU
work. The source's noise generation contributes substantial CPU outside active
push spans; profiling also samples that work after the thread registers.

The main series uses seven measured repetitions per mode after one discarded
warm-up per mode. Mode order rotates through all seven positions. The separate
default-settings check uses six measured repetitions and one warm-up for each
of its three modes, rotating through all three positions twice. Children run on
physical P cores 0 and 2; the Python HTTP receiver runs on CPU 4. Frequency is not pinned
(`powersave` governor), and other desktop services remain active. No builds or
test suites ran during the recorded series. Every repetition starts a new
process, so each includes a cold symbol cache and multiple reporting intervals.

The main series matches the older benchmark settings: 100% root sampling, a
65,536-record trace queue, batches of 512, and a 10 ms trace export interval.
The separate default-settings check uses the plugin defaults of 8,192 records
and a 200 ms interval; other settings and the binary are identical. Profiling
runs at 100 Hz. Logging modes emit one INFO message per frame from the sink-pad
probe, inside an active push,
using `GST_DEBUG=0,mars-profile-bench:4`. The ordinary GStreamer stderr handler
remains enabled; its output is captured by the parent. The no-tracer native-log
control emits the same messages. Other modes execute the same disabled log
call. No other categories are enabled. This is a defined moderate log workload,
not a benchmark of unrestricted debug logging.

The local HTTP receiver acknowledges and stores actual requests, then decodes
them after the child exits. Remote storage and Grafana are not part of timing.
Traces and logs use OTLP/HTTP protobuf; profiles use the Pyroscope push protocol.

- **Process CPU** covers all child threads from entering PLAYING through NULL
  and explicit exporter/profiler shutdown, including final symbolization and
  uploads. It excludes initialization and graph construction. `total_cpu_ns`
  in the raw data additionally includes those startup operations.
- **Streaming CPU** covers the source thread from its first buffer probe to EOS,
  excluding generation of the first frame. **Wall time** ends at EOS and excludes
  shutdown; FPS is 1,200 divided by that wall time.
- **Peak RSS** is the child's `/proc/self/status` `VmHWM`. It includes initialization,
  loaded code, sampler storage, symbolization, and allocator retention. It excludes
  the Python receiver. Added memory is the difference between mode medians.
- **Added cycles** is the percentage difference between each mode's median
  whole-process hardware cycles and the no-tracer median. Process CPU-time
  differences are also reported, with their observed ranges. Neither measure
  is FPS loss or percentage points of host CPU utilization.

## Scope

These measurements apply to this CPU-heavy conversion workload, build, and host.
Native libraries were not rebuilt with frame pointers; Rust frame-pointer flags
do not improve native unwinding. Different installed debug symbols and libraries
can change profile quality, symbolization cost, and memory. Higher sample rates,
more streaming threads, higher log volume, and network failures need separate
measurements. The current profiles-only implementation should not be assigned
the historical prototype's 6–8% figure without this matched comparison.

The earlier profiling tables compared tracing alone with an older correlated
profiler and later compared two profiler implementations. Their baselines,
driver, shutdown handling, and execution dates differ; they remain historical
results rather than interchangeable measurements.

## Reproduce

Run from the repository root, choosing CPU affinities available on your host.
The hardware-counter runs require Linux `perf` and permission to read counters:

```sh
RUSTFLAGS="-C force-frame-pointers=yes" cargo build --release \
  -p gst-otel-tracer --features profiles --example profile_video \
  --target-dir target/profile-video-build

taskset -c 4 python3 tracer/otel/bench/profile_video.py \
  --perf --cpus 0,2 --frames 1200 --repeats 7 \
  --output reports/otel/2026-10-23-profiles-video.csv
```

The default-settings check uses:

```sh
taskset -c 4 python3 tracer/otel/bench/profile_video.py \
  --perf --cpus 0,2 --frames 1200 --repeats 6 \
  --modes none profiles otel-profiles-logs \
  --export-interval-ms 200 --queue-capacity 8192 \
  --output reports/otel/2026-10-23-profiles-video-defaults.csv
```

The [runner](../../tracer/otel/bench/profile_video.py) and
[driver](../../tracer/otel/examples/profile_video.rs) fail on missing spans,
logs, or profile sample weights, mismatched correlation, drop counters, and
shutdown errors. The [raw CSV](2026-10-23-profiles-video.csv) retains warm-ups
(`repeat=-1`) and every measured run. [Build and environment metadata](2026-10-23-profiles-video.json)
includes the binary, driver, decoder and lockfile hashes. Per-run diagnostic
logs are written to `target/profile-video-runs/`.

The defaults check has its own [raw CSV](2026-10-23-profiles-video-defaults.csv)
and [metadata](2026-10-23-profiles-video-defaults.json). The runner gained explicit
export-interval and queue-capacity options between series; the driver binary
remained identical. The recorded source hashes reflect each measurement.
