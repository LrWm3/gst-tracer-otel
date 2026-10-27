# Optional GPU profiling costs — 2026-10-26

On an integrated memory nvidia device, adding GPU profiles to metrics + traces
increased pipeline time by about **5.4%** and peak process RSS by **373 MiB** in
the tested 1080p inference workload. Collecting CPU profiles too, at 100 Hz,
added **377 MiB** over the same control: about **29% more process memory**.
The optional NVTX attribution bridge was measured separately and added little
relative to the existing profiling costs.

These are measurements of earlier revisions, not a benchmark of the latest
configuration or compaction fixes. GPU collection is disabled by default; see
the [GPU setup and limits](../../tracer/otel/GPU_PROFILES.md) and
[NVTX setup](../../tracer/otel/NVTX.md).

## GPU collection

Four measured runs per mode, with balanced run order and excluded warm-ups.
All values below are medians:

| Mode | Pipeline time | Throughput | Whole-job CPU time | Peak process RSS |
| --- | ---: | ---: | ---: | ---: |
| No instrumentation | 27.221 s | 134.46 FPS | 24.957 s | 1,313.7 MiB |
| Metrics + traces | 27.225 s | 134.43 FPS | 26.030 s | 1,317.5 MiB |
| Metrics + traces + GPU profiles | 28.685 s | 127.59 FPS | 28.932 s | 1,690.4 MiB |
| Metrics + traces + CPU/GPU profiles | 28.693 s | 127.56 FPS | 30.029 s | 1,694.2 MiB |

Relative to **metrics + traces**, using the unrounded medians:

| Added sources | Pipeline time increase | CPU time increase | Added peak RSS | Peak RSS increase |
| --- | ---: | ---: | ---: | ---: |
| GPU | 5.36% | 11.15% | 372.8 MiB | 28.3% |
| CPU + GPU | 5.39% | 15.36% | 376.7 MiB | 28.6% |

The combined profiler's additional memory accounts for **22.2% of the final
1,694.2 MiB process peak**. This uses process RSS as the denominator, not total
device RAM. RSS includes shared mappings and CUDA/CUPTI internal allocations;
it is not unique physical memory or an isolated GPU allocation measurement.
The adapter's small bounded buffer pools do not bound all CUPTI memory costs.

Pipeline time covers PLAYING to EOS. CPU time covers the whole job, including
setup and shutdown; it is consumed CPU seconds, not utilization or CPU cycles.
Median pipeline-NULL-to-process-exit time was 0.625 s for metrics + traces,
2.089 s with GPU profiles, and 1.749 s with both sources. These runs explicitly
flushed profiles before shutdown, so delivery time is included in those exit
measurements and excluded from pipeline throughput.

## Incremental NVTX attribution cost

A separate comparison enabled and disabled the bridge for each profile mode.
Metrics and traces remained enabled throughout. Each setting had two runs,
with pair order reversed in the second round:

| Profile sources | Pipeline time change | Whole-job CPU time change | Peak RSS change |
| --- | ---: | ---: | ---: |
| CPU | -0.11% | +0.62% | +1.2 MiB |
| GPU | +0.10% | -0.17% | -4.4 MiB |
| CPU + GPU | +0.12% | +0.63% | +2.3 MiB |

These are changes relative to the same profiling mode with NVTX disabled.
Small differences overlap run variation; negative values do not establish a
speedup, and two repetitions do not establish a general overhead bound. Do not
add these percentages to the first table as if they came from matched runs.

In this workload, the bridge increased element-attributed GPU time from about
3% to more than 99.999%, based on captured uploads. It also registered a
previously unsampled inference worker for CPU sampling. This supplies element
attribution; exact asynchronous per-batch span links still require a context
handoff through the application's job queue.

## Method and validation boundaries

The installed processor ran an isolated offline 1080p H.264 workload through
decoding, conversion, batched inference, tracking and local output. The cached
model produced no detections, so tracker work was light. Traces were fully
sampled, metrics timing used one call in 16, metrics exported every second,
monitoring was `core`, and logs were disabled. CPU profiles used 100 Hz and
function symbolization. Backend CPU and memory are outside the measurements.
The GPU comparison used direct delivery; the separate NVTX comparison used
the same recording proxy for both settings. Source and raw evidence remain in
private reports outside this repository; this page contains aggregate results.

GPU measurements used the launch-context fix in `9599aab`; NVTX measurements
used `49594c9`. Every measured run completed with matching normalized outputs
and no reported GPU record or upload losses. No hangs were observed. Short
fresh-process runs do not establish long-running memory or deadlock freedom.

Those builds had a backend timestamp collision, subsequently fixed in
`dceb124`. It affected retained profile totals, not these process resource
measurements. The real Pyroscope compaction regression retained all **80 ms of
80 ms** of submitted GPU weights, including element/span/trace totals; its
comparison using the old timestamps retained only **10 ms**. That regression
uses a mock GPU adapter and validates storage correctness, not device overhead.
See the [compaction check](../../tracer/otel/GPU_PROFILES.md#demo-and-checks).

The latest fixes resolve properties at construction and allocate report
timestamps in the background worker. Their incremental application cost is
expected to be small but has not been measured on the device. Correctly
retaining reports can increase backend storage relative to the buggy version.
A fresh device benchmark and a longer GPU lifecycle soak remain follow-ups.
