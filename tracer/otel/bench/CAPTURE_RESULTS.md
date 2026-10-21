# Capture propagation follow-up

Baseline: `22f2ecb` (direct OTLP encoding). Branch: `impl/otel-compact-capture`.

This iteration reduces source-thread CPU by **2–3% at one stream** and **6–9%
at four or sixteen streams** in the final identity-pipeline comparison. Every
healthy fully sampled run delivers all 50000 spans. These are modest capture
improvements; single-stream fully sampled process CPU is unchanged.

## Change

Keep a lazily initialized, synchronized shared-root override alongside the owned
context in `GstMeta`. Normal propagation with existing metadata now avoids
mini-object qdata lookup. GStreamer 1.24.2 takes a global mutex inside that
[qdata getter](https://raw.githubusercontent.com/GStreamer/gstreamer/1.24.2/subprojects/gstreamer/gst/gstminiobject.c).
Previously even a writable buffer with metadata and no override paid for it.

The private metadata layout is now V3. `OnceLock` initializes the optional
override; its `RwLock` protects complete owned tokens. The metadata destructor
releases both the base context and override. Shared descendants still retain the
common ancestor, and copies own the effective context independently. Initially
shared buffers without metadata retain the qdata fallback. Installing or copying
metadata into a writable buffer clears its old fallback, preventing stale context
from resurfacing after metadata removal.

Each context meta grows by one `OnceLock<Box<Fallback>>`; ordinary propagation
does not allocate the extra slot. A shared-root override allocates at most one
slot per meta, released with the buffer. Memory footprint was not benchmarked.

Sampling, external identity and trace state, export format, queue capacity,
flush/shutdown, and counters retain their existing behavior.

## Method

Intel i5-1345U, Rust 1.89, GStreamer 1.24.2. Release pipeline runs use CPUs 4–11.
Each run has 10000 buffers total and five pushes per buffer, split across 1, 4,
or 16 source threads. Logging is disabled. Healthy runs use a 65536-record queue,
512-span batches, blocking HTTP, and the local HTTP/1.0 collector. Pipeline stop
flushes before counters are read; no extra sleep is added for drain.

The baseline plugin was built and saved before production changes. The final
series uses seven repetitions per cell, with variant order rotating and disabled
controls using both plugin directories. No builds ran alongside these measured
pipelines. This remains a shared workstation: CPU affinity does not isolate
frequency, caches, or scheduling. Disabled source CPU shifts by up to about 3%,
so small single-stream differences deserve caution. The initial five-repetition
comparison also favored this change, but absolute times vary between series.

Source CPU is summed source-thread CPU, including normal GStreamer work. Process
CPU includes drain and all exporter/helper threads. Pipeline wall time and
coarse exporter CPU ticks are retained in the raw results. Exporter ticks alone
exclude the blocking adapter's separate helper thread.

Hardware profiling was unavailable (`perf_event_paranoid=4`, no performance
monitoring capability); isolated stage timings and paired pipeline measurements
were used instead.

## Final identity-pipeline comparison

Median milliseconds, seven repetitions. These freshly measured before/after
values use the same workload as the previous iteration.

| Streams | Sampling | Source CPU before | Source CPU after | Process CPU before | Process CPU after |
| --- | --- | ---: | ---: | ---: | ---: |
| 1 | Disabled | 20.74 | 21.40 | 21.50 | 22.22 |
| 1 | 0% | 46.10 | 44.99 | 47.12 | 46.10 |
| 1 | 10% | 50.65 | 49.65 | 62.15 | 60.75 |
| 1 | 100% | 58.60 | 56.90 | 96.63 | 96.63 |
| 4 | Disabled | 30.44 | 30.58 | 32.44 | 32.44 |
| 4 | 0% | 55.70 | 51.60 | 57.95 | 53.83 |
| 4 | 10% | 63.26 | 58.48 | 71.63 | 66.93 |
| 4 | 100% | 74.71 | 70.51 | 113.63 | 109.73 |
| 16 | Disabled | 56.30 | 57.92 | 61.63 | 63.23 |
| 16 | 0% | 72.46 | 66.11 | 79.64 | 72.72 |
| 16 | 10% | 86.80 | 79.79 | 99.61 | 92.56 |
| 16 | 100% | 93.31 | 87.22 | 140.98 | 134.09 |

All 100% runs deliver 50000 spans; 0% runs deliver zero and count 50000 rejected
pushes. Partial sampling delivers 4680–5235 spans across both versions, preserving
whole buffer journeys. Partial sampling is random, so these workloads are not
identical. Healthy runs have zero failed exports, queue/closed drops, pending
records, propagation misses, depth drops, and hook mismatches.

## GStreamer tracing floor

A benchmark-only C tracer registers two empty push callbacks. It does no OTel
capture, metadata work, or export. Registering hooks enables GStreamer's
[tracing subsystem](https://raw.githubusercontent.com/GStreamer/gstreamer/1.24.2/subprojects/gstreamer/gst/gsttracerutils.c).
Median source CPU in milliseconds, five repetitions per cell:

| Streams | Disabled | Empty push hooks | Updated 0% | Updated 100% |
| --- | ---: | ---: | ---: | ---: |
| 1 | 20.66 | 35.50 | 44.96 | 57.73 |
| 4 | 30.04 | 41.70 | 50.98 | 70.58 |
| 16 | 57.02 | 61.01 | 65.53 | 83.64 |

In the single-stream diagnostic, enabling empty hooks adds 14.84 ms, about 61%
of the 24.30 ms increment from disabled to 0% sampling. This suggests much of
the remaining rejected-path overhead comes from enabling GStreamer tracing.
It is not a claim about a hard limit for other GStreamer versions or pipelines.
This diagnostic used the initial metadata implementation, before the additional
cleanup for copying metadata into a previously shared destination.

## Isolated rejected capture

The ignored release microbenchmark reuses one writable buffer and initialized
source pad. Each of five rounds times 500000 propagation calls and 100000 nested
chains of five begin/end pairs. It excludes real GStreamer push dispatch, fresh
buffer/metadata allocation, and HTTP. Runs are restricted to CPU 4.

| Stage | Before ns/operation | After ns/operation | Reduction |
| --- | ---: | ---: | ---: |
| Propagate an unchanged rejected context | 32.69 | 18.59 | 43.1% |
| Rejected capture begin/end pair | 88.31 | 74.79 | 15.3% |

The capture benchmark was added to the baseline before changing production code.
Before and after are separate timing runs, not interleaved binaries. These
stage-level gains should not be described as whole-tracer speedups.
[Raw stage timings](CAPTURE_STAGES.csv) retain the initial candidate as well.

## Shared buffers and failure accounting

Tee/queue results are recorded separately because source CPU excludes queue
threads and the workload schedules differently from the identity pipeline.
All fully sampled tee runs deliver 50000 spans with zero losses or context
misses. The initial three-repetition four-stream sample was slower after the
change (source 61.31 → 63.93 ms, process 163.13 → 175.03 ms). A seven-repetition
confirmation did not reproduce that regression:

| Streams | Sampling | Source CPU before | Source CPU after | Process CPU before | Process CPU after |
| --- | --- | ---: | ---: | ---: | ---: |
| 1 | 0% | 47.53 | 46.26 | 119.42 | 114.76 |
| 1 | 100% | 62.47 | 60.77 | 189.14 | 185.92 |
| 4 | 0% | 48.66 | 47.72 | 103.71 | 101.24 |
| 4 | 100% | 61.91 | 61.24 | 158.94 | 158.84 |

These small tee differences overlap run-to-run variation. In particular,
four-stream sampled process CPU is effectively flat; the larger gains from the
identity pipeline should not be generalized to queued pipelines.

Unrelated metadata churn, renaming an element every ten source buffers, preserves
full sampled delivery and zero rejected delivery. These twelve runs validate the
changed implementation; they are not a paired performance claim.

Twelve slow/absent-receiver runs use a queue capacity of 128 and batches of 64.
Slow runs deliver 192–256 spans and count 49744–49808 queue drops. Absent receivers
deliver zero, count 11508–47553 failed-export spans and 2447–38492 queue drops.
Every run finishes with zero pending records and exact completed-span accounting.
None of these loss-heavy runs is presented as a speed improvement.

## Validation

- `cargo fmt --all`: passed.
- `cargo clippy --workspace --all-features -- -D warnings`: passed.
- `cargo test`: passed, including the default Prometheus member.
- `cargo test -p gst-otel-tracer`: 23 unit tests and 8 collector tests passed;
  the two manual benchmarks and subprocess entry point are intentionally ignored.
- Manual rejected-capture release benchmark: passed, including counter checks.
- `cargo build --release -p gst-otel-tracer`: passed.
- `cargo audit`: unchanged failure, 22 vulnerabilities and 15 warnings.
- Required workspace nightly `udeps`: blocked by the existing `rustix 0.37.28`
  compiler-attribute error. With `RUSTFLAGS='--cfg=rustix_use_libc'`, it reports
  the existing independent Pyroscope dependencies `console` and `pyroscope_pyspy`.
  OTel-only nightly `udeps --all-targets` passes without that workaround.

New tests exercise simultaneous shared-root overrides and context copies across
eight threads, ownership cleanup, and retirement of initially shared overrides
on both ordinary metadata installation and metadata copying. Existing collector
tests cover queue boundaries, transforms, tees, sampling, trace state, parentage,
native log correlation, both HTTP adapters, and failure behavior.

## Reproduction

Build baseline `22f2ecb` in a separate checkout and preserve its release plugin.
Then build this branch and run:

```sh
cargo build --release -p gst-otel-tracer
taskset -c 4-11 python3 tracer/otel/bench/run.py \
  --baseline /path/to/22f2ecb/plugin --baseline-sampling --baseline-disabled \
  --streams 1 4 16 --modes none unsampled partial sampled \
  --repeats 7 --drain-ms 0 --output capture-final.json
taskset -c 4-11 python3 tracer/otel/bench/run.py \
  --streams 1 4 16 --modes none hooks unsampled sampled \
  --repeats 5 --drain-ms 0 --output capture-hooks.json
cargo test --release -p gst-otel-tracer --lib rejected_capture_benchmark --no-run
# Run the executable printed above with taskset -c 4 and:
# --exact capture::benchmarks::rejected_capture_benchmark --ignored --nocapture
```

Use `--tee-queues`, `--churn-every 10`, and `--modes slow absent` for the additional
cases. [Raw pipeline measurements](CAPTURE_RESULTS.csv) retain the initial
candidate, diagnostic, final comparison, and validation series with every
counter, delivery count, and timing: 422 runs total (90 initial, 60 empty-hook
diagnostic, 168 final identity, 24 initial tee, 56 confirmation tee, 12 churn,
and 12 failure runs).
