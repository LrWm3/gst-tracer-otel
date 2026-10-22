# Metrics-only collector measurements

Measured 2026-10-22 on an Intel Core i5-1345U, Linux x86-64, stock GStreamer
1.24.2 and Rust 1.89.0 release builds at `6ffb062`. GitHub `main` matched the
local checkout before building. No collector implementation changes were
needed for this benchmark.

On this push-only workload, `prom-latency` added **0.36–0.39 microseconds of
source-thread CPU per push**, with **10.9–11.5 MiB total peak process RSS**.
`gst-instrument` with tracing disabled added **0.42–0.58 microseconds per push**,
with **13.8–14.2 MiB total peak RSS**. The collectors produce different metrics,
as described below; these are overall capture/export costs, not a comparison
of identical outputs.

## Workload and measurement

Each run sends **1,000,000 buffers through four identities and a fakesink**:
5,000,000 synchronous push calls in total. One source processes all buffers,
or four independent source chains process 250,000 buffers each. There are five
pad groups per chain. Sinks use `sync=false async=false`; buffers have no useful
processing work. This is the same pipeline shape as the earlier
[combined-capture comparison](../otel/bench/INSTRUMENT_RESULTS.md), with 100
times as many buffers to reduce short-run noise. Absolute times from the two
reports should not be compared directly.

Each mode/source-count combination gets one unrecorded warmup, then seven
measured repetitions with rotated mode ordering. All modes use the same driver,
release plugin directory, and warmed GStreamer registry. The scheduler can
use all 12 logical CPUs; affinity and frequency are not pinned, and the CPU
governor is `powersave`. No builds or test suites ran alongside these timings.

The modes are:

- **No tracer:** `GST_TRACERS` is empty.
- **Prometheus:** `GST_TRACERS='prom-latency(port=...)'`, all elements selected,
  histogram sampling approximately 1/16, PTS age and I/O counts disabled.
  Count, sum, and last exclusive wall time cover every push; exclusive wall,
  CPU, and blocked-time histograms cover sampled pushes.
- **OTLP metrics only:** `GST_TRACERS=gst-instrument` and
  `GST_INSTRUMENT_TRACES=false`, with logs and profiles disabled. Inclusive and
  exclusive wall histograms cover every push; exclusive CPU and blocked-time
  histograms sample approximately 1/16. There is no trace queue or trace traffic.

The Prometheus endpoint is scraped once per second while the pipeline runs.
The OTLP metric export interval is also one second. Both get a final HTTP
scrape/export after pipeline shutdown. The driver keeps the graph alive until
the final Prometheus scrape completes, preserving its series. Requests go to
local HTTP receivers; there is no remote storage or Grafana query in this test.

**Source CPU** is summed thread CPU time from the first source buffer to EOS.
**Process CPU** includes all pipeline-process threads from entering PLAYING
through NULL and the final scrape/export; it excludes GStreamer initialization,
graph construction, and the external Python receiver. **Wall time** ends at EOS,
before final delivery. **Peak RSS** is the pipeline process's `/proc/self/status`
`VmHWM`, including exporter memory and excluding the Python launcher/receiver.

## Results

Medians of seven runs. Added CPU per push subtracts the matching no-tracer
median source CPU and divides by 5,000,000; it is not a measurement of element
latency or a production CPU percentage.

| Sources | Mode | Source CPU (ms) | Process CPU (ms) | Wall (ms) | Peak RSS (MiB) | Added source CPU (µs/push) |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| 1 | No tracer | 1,408.4 | 1,409.1 | 1,409.9 | 8.9 | — |
| 1 | `prom-latency` | 3,201.7 | 3,205.8 | 3,202.6 | 10.9 | 0.359 |
| 1 | `gst-instrument`, metrics only | 3,522.2 | 3,525.1 | 3,529.7 | 13.8 | 0.423 |
| 4 | No tracer | 2,663.9 | 2,665.7 | 732.8 | 8.9 | — |
| 4 | `prom-latency` | 4,633.0 | 4,639.2 | 1,355.3 | 11.5 | 0.394 |
| 4 | `gst-instrument`, metrics only | 5,575.0 | 5,578.8 | 1,662.4 | 14.2 | 0.582 |

Relative to no tracer, Prometheus added about **2.0 MiB** peak RSS for one source
and **2.6 MiB** for four sources. OTLP metrics added about **4.9 MiB** and
**5.3 MiB**, respectively. These figures describe a fixed graph of five or twenty
pad groups, not memory growth with arbitrary pipeline cardinality or a leak test.

Source CPU increased by about 127%/74% for Prometheus and 150%/109% for OTLP
metrics at one/four sources. These percentages are large because the pipeline
does almost no processing work. They should not be applied to a decoding or
inference pipeline; the per-push costs and actual push rate are more useful
inputs for estimating that workload's cost.

All observed source-CPU ranges are retained:

| Sources | No tracer (ms) | `prom-latency` (ms) | OTLP metrics only (ms) |
| --- | ---: | ---: | ---: |
| 1 | 1,364.8–1,415.2 | 3,046.1–3,262.2 | 3,445.4–3,754.1 |
| 4 | 2,557.9–2,790.1 | 4,510.6–5,121.1 | 5,351.7–9,462.5 |

The final four-source OTLP run was an outlier: 9,462.5 ms source CPU versus
5,351.7–5,905.4 ms in the other six runs. It completed with correct counts and
no export errors; its cause was not isolated. It remains in the raw data and
median calculation. These runs do not establish precise scaling ratios,
production throughput, or rare-tail behavior.

## Output checks

All **42 measured runs** completed successfully. Each collector delivered
**70,000,000 counted pushes**, for **140,000,000** observations across both
collectors; the 14 no-tracer runs produced no telemetry.

The harness checks the final Prometheus all-push counter and decoded cumulative
OTLP wall-histogram count against exactly 5,000,000 for every instrumented run.
It also checks the expected number of wall-histogram groups, matching CPU and
blocked sample counts, OTLP histogram bucket totals, absence of other OTLP
signals/exemplars, and zero OTLP metric drop/failure counters. Earlier cumulative
snapshots are replaced by the latest value per group, not added together.

One-source runs made four HTTP requests per collector. Four-source runs made
two, except the longer OTLP outlier, which made three. The median total HTTP
payload per run was 262/519 KiB for Prometheus at one/four sources, versus
54/106 KiB for OTLP. These are payload bytes, excluding HTTP headers; the metric
sets and encodings differ. Final serialization and HTTP serving/exporting are
included in process CPU and peak RSS.

This workload has no queues, pull-mode pads, buffer lists, topology churn, logs,
profiles, trace exemplars, or collector failures. Prometheus supports additional
diagnostics beyond the push metrics measured here. The older
[Benchmarks on an integrated memory nvidia device and lifecycle checks](BENCHMARK.md) cover different workloads and
revisions; they were not rerun for this report.

## Reproduce

```sh
cargo build --release -p gst-otel-tracer -p gst-prometheus-tracer
python3 tracer/otel/bench/metrics.py --repeats 7 --buffers 1000000 \
  --output tracer/prometheus/benchmarks/2026-10-22-metrics.csv
```

The [harness](../otel/bench/metrics.py) uses the shared
[pipeline driver](../otel/bench/pipeline.c). It accepts `--streams`,
`--sample-rate`, `--plugin-dir`, and optional `--cpus` for controlled affinity
experiments. Warmups and every measured run undergo the same count checks;
timeouts or missing output fail the benchmark.

See the [raw per-run CSV](benchmarks/2026-10-22-metrics.csv) and
[environment/build metadata](benchmarks/2026-10-22-metrics.json), including the
Git revision, plugin SHA-256 hashes, and command line.
