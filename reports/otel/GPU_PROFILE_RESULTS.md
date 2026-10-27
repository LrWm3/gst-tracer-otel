# Optional GPU profiling costs

In the **2026-10-27** processor recheck, GPU profiling with NVTX element
attribution added about **5.5% pipeline time**, **11.5% whole-job CPU time** and
**376 MiB peak RSS** over metrics + traces. CPU+GPU profiling at 100 Hz added
about **5.5%**, **15.6%** and **373 MiB**, respectively. Both profile modes used
about **29% more process memory**. GPU collection remains opt-in; see the
[setup and limits](../../tracer/otel/GPU_PROFILES.md).

## Current processor recheck — 2026-10-27

Four measured fresh-process runs per mode, balanced across four run orders,
with one excluded warm-up per mode. These are medians on an integrated memory
nvidia device:

| Mode | Pipeline time | Throughput | Whole-job CPU time | Peak process RSS |
| --- | ---: | ---: | ---: | ---: |
| Metrics + traces | 27.130 s | 134.91 FPS | 25.584 s | 1,316.4 MiB |
| GPU + NVTX, before removing driver activity | 28.584 s | 128.04 FPS | 28.645 s | 1,689.6 MiB |
| GPU + NVTX, current adapter | 28.615 s | 127.90 FPS | 28.517 s | 1,692.6 MiB |
| CPU + GPU + NVTX, current adapter | 28.622 s | 127.87 FPS | 29.577 s | 1,689.9 MiB |

Every profiling row also includes metrics and traces. Relative to the unrounded
control medians, the current GPU mode adds **5.48% pipeline time, 11.47% CPU
time and 376.1 MiB RSS (28.57%)**. Combined mode adds **5.50%, 15.61% and
373.4 MiB (28.37%)**. The latter extra memory is **22.10% of its final process
peak**, not a percentage of total device RAM. RSS includes shared mappings.

The slightly lower combined-mode RSS is within run variation: GPU-only peaks
ranged from 1,686.6 to 1,693.7 MiB and combined peaks from 1,687.2 to
1,694.2 MiB. It does not establish that enabling CPU profiling saves memory.

To isolate driver activity removal, compare the two GPU modes within each
round. Median paired changes were **+0.03% pipeline time, -0.45% CPU time and
+0.21 MiB RSS**. Individual CPU changes ranged from -1.14% to +0.19%; pipeline
changes ranged from -0.14% to +0.15%. There is no demonstrated material
throughput or memory improvement from that cleanup. Paired changes and ratios
of the mode medians are different summaries and should not be interchanged.

Pipeline time covers PLAYING to EOS. CPU time is consumed user+system seconds
for the whole job, including setup and shutdown. Peak RSS is the high-water
mark of the largest child process, the traced pipeline process here. Median
pipeline-NULL-to-child-exit time was **1.60 s** for control, **3.04 s** for current
GPU and **3.13 s** for combined profiling, including two explicit flushes and
shutdown. The combined exit interval ranged from 2.03 to 5.20 s; these are not
shutdown deadline guarantees.

The isolated offline 1080p workload performed decoding, conversion, batched
inference, tracking and local output. The cached model produced no detections,
so tracking work was light. Traces were fully sampled, metric timing sampled
one call in 16, queue polling used 100 ms, and OTLP metrics exported every
second. IO counts and PTS-age metrics were disabled, monitoring was `core`,
and logs were disabled. CPU profiles used 100 Hz and function symbolization.
NVTX attribution was enabled in every profiling mode. Backend resources are
outside these process measurements. Rust code was `f0a6e6c` in every mode;
the candidate native adapter is `a65dcf2`.

All **16 runs** produced identical normalized 3,660-frame outputs and exported
**34,097 spans** per run. Each of the 12 GPU runs captured and delivered
**45,517 kernels**, attributed **45,500** to elements and correlated **3,660**
with spans. There were no reported trace/GPU losses or pending records at
shutdown. The combined runs captured, reported and delivered
1,692 / 1,665 / 1,673 / 1,678 CPU samples, and Pyroscope retained exactly their
100 Hz weights. They recorded **14 / 14 / 12 / 5 timer overruns**; other CPU
failure, queue-drop, upload-drop and pending counters were zero. Successful
delivery of captured samples does not mean no sampling intervals were missed.

The first attempted batch had an invalid sampler setting and disabled
instrumentation. Its measurements are excluded. The corrected harness uses
`parentbased_always_on` and rejects disabled initialization, missing stats or
unsuccessful flush/shutdown reports even when the application exits successfully.

See the [aggregate run measurements](processing/2026-10-27/gpu-processor.json).
Raw processor evidence, configuration, output hashes and logs remain private
outside this repository.

### Processor memory and delivery diagnostics

Separate runs, excluded from timing results, captured memory maps ten seconds
after PLAYING. In the GPU-only run, the extra RSS at that point was about
408 MiB: 280 MiB of heap, 114 MiB of other anonymous mappings, 10 MiB of library
mappings and 4 MiB of shared DMA mappings. These are representative snapshots,
not matched peak measurements or proof of which allocator owns each page.

Captured GPU reports were at most **7,980 bytes before compression**; combined
CPU reports were at most 32,448 bytes. The GPU-only diagnostic produced 59 GPU
reports, and the combined diagnostic produced 59 GPU plus 31 CPU reports.
All 149 uploads were accepted with unique timestamps within each service/type;
queried Pyroscope totals and every element-owner weight matched the captured
payloads. The GPU totals were 28.649002763 s and 28.610567165 s; combined CPU
weight was 16.64 s. An isolated replay retained all original totals and owner
weights after four successful compaction jobs. Both deliberately duplicated-time
GPU controls lost one of their two reports, proving that GPU deduplication ran.
The CPU control also lost weight but did not meet the diagnostic's stricter
one-report expectation; the original CPU profiles still retained their exact
16.64 s. No stronger CPU-control claim is made.

Small upload payloads do not explain the large RSS increase.
The remaining allocation-level CUDA/CUPTI investigation and longer lifecycle
soak are follow-ups; buffer limits and application allocator policy are unchanged.

## Earlier processor measurements — 2026-10-26

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

### GPU collection

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

### Incremental NVTX attribution cost

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

### Earlier method and validation boundaries

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
The device recheck above supersedes those overhead estimates. A longer GPU
lifecycle soak remains a follow-up.

## Native memory and correlation recheck — 2026-10-27

The CUDA 11.4 adapter now omits unused driver activity records while preserving
selected launch callbacks and concurrent kernel activities. A separate native
probe partitions memory without GStreamer, Rust processing, CPU symbolization
or HTTP. It creates one CUDA context and runs 2,001 tiny kernels. Each mode ran
in three fresh processes, with reversed order in the second repetition, on an
integrated memory nvidia device.

Median RSS in MiB, using the adapter from `a65dcf2`:

| Phase | CUDA workload alone | Adapter library loaded, inactive | GPU collection active |
| --- | ---: | ---: | ---: |
| Initial, before adapter loading | 14.2 | 14.2 | 14.2 |
| CUDA context created | 33.1 | 35.5 | 89.6 |
| Kernels completed and flushed | 63.4 | 65.9 | 115.4 |
| Context destroyed, profiler stopped, one second idle | 54.2 | 56.7 | 100.8 |
| Diagnostic allocator trim after teardown | 43.0 | 45.5 | 57.3 |

The initial phase already maps the CUDA and NVRTC libraries. Loading the adapter
adds about 2.5 MiB; starting collection adds another 1.5 MiB before CUDA starts.
The larger increase appears during CUDA/CUPTI context initialization. At the
completed-kernel phase, collection adds **52.0 MiB** over the CUDA-only control.
One representative mapping comparison places about 35.5 MiB of that difference
in the heap, 7.2 MiB in the PTX compiler library, 4.3 MiB in shared DMA mappings
and 3.1 MiB in CUPTI mappings. Mapping categories identify where pages reside;
they do not identify individual internal allocation sites.

After context destruction, glibc allocator trimming reduced the profiled
process by 43.5 MiB versus 11.2 MiB in the control. A separate diagnostic that
trimmed while the context remained active still retained about 52 MiB of extra
RSS after kernel completion. This is evidence of retained allocations after
teardown, not an equivalent opportunity to reclaim memory during execution.
The production tracer does not call `malloc_trim` or change application-wide
allocator policy. Short fresh-process runs do not establish leak freedom.

Removing driver activity changed median completed-kernel RSS from **115.56 to
115.44 MiB** in this probe, which is too small to claim a meaningful memory
improvement. All six active before/after runs matched every launch and kernel,
with zero unmatched, collided or dropped records. The 2 MiB host buffer pool
does not explain or bound the measured CUDA/CUPTI costs. The SDK's timestamp
buffer attributes also default to pinned host storage in CUDA 11.4 despite
their `DEVICE_BUFFER` names.

The real two-element demo passed with both adapters and with the candidate in
CPU+GPU mode: **48/48 kernels**, **16/16 submitting spans**, exact aggregate,
element, span and trace totals through Tempo/Pyroscope, and no reported losses.
GPU-only mode stored no CPU profile; combined mode stored all 51 CPU samples
(510 ms). These are correctness checks, not throughput comparisons.

See the [native phase measurements](processing/2026-10-27/gpu-memory.json) and
[reproduction commands](../../tracer/otel/GPU_PROFILES.md#native-memory-diagnostic).
Detailed processor evidence remains private outside this repository.
