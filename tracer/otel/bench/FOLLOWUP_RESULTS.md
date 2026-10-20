# Compact capture: follow-up optimizations

Measured 2026-10-20 on `impl/otel-compact-capture`, comparing the preserved
release plugin from `ab5b76f` (v1) with this implementation (v2).

## Changes

- Reject unsampled descendants before consulting descriptive metadata. Cache
  source classification separately from descriptions.
- Replace shared counters and submission registration with 64 fixed,
  cache-aligned shards. Snapshots retain retired-thread counts. Pending records
  are derived from cumulative counts, removing a producer/worker shared write.
- Drain up to 64 available records between worker clock checks; preallocate
  replacement export batches and preserve FIFO flush/shutdown markers.
- Consume owned `SpanData` during protobuf grouping/conversion through a narrow
  patch of `opentelemetry-proto` 0.30. The OTLP exporter and transport are unchanged.
- Invalidate descriptions through weak subscriptions to relevant objects,
  instead of a process-wide generation. Keep shared-buffer sidecar slots stable
  for the buffer lifetime and synchronize subsequent access per buffer.

The bounded MPSC queue remains. These measurements exercise queue contention
with 1, 4, and 16 sources but do not isolate it from scheduling, propagation, or
capture cost. Per-producer queues and producer-side record chunks remain a
possible later change, with additional flush/retirement complexity. There is
no direct protobuf transport replacement or new profile correlation here.

## Healthy delivery

Rust 1.89.0, GStreamer 1.24.2, Intel i5-1345U, shared machine with unrestricted
CPU affinity. Each process sends 10000 buffers total, divided between the
sources, with five pushes per buffer. Logging is disabled. Queue capacity is
65536, batches are 512, and the export interval is 10 ms. Pipeline NULL flushes
both plugins, so no additional sleep is needed. Actual protobuf spans are
counted at a local HTTP receiver.

Times are medians in milliseconds. Streaming CPU sums source-thread CPU;
process CPU includes export drain and HTTP helper threads. Five processes per
cell, except the single-source 100% case, which pools all 20 processes per
version from the initial run and follow-up investigation.

| Sources | Sampling | Streaming CPU v1 | Streaming CPU v2 | Process CPU v1 | Process CPU v2 |
| --- | --- | ---: | ---: | ---: | ---: |
| 1 | 100% | 43.09 | 39.56 | 209.74 | 197.55 |
| 4 | 100% | 77.96 | 74.37 | 265.12 | 236.26 |
| 16 | 100% | 175.07 | 155.37 | 492.39 | 490.87 |
| 1 | 0% | 37.70 | 29.95 | 39.01 | 30.95 |
| 4 | 0% | 64.18 | 53.59 | 66.45 | 55.96 |
| 16 | 0% | 149.84 | 136.40 | 159.46 | 149.57 |
| 1 | 10% | 50.37 | 49.00 | 77.23 | 73.52 |
| 4 | 10% | 63.79 | 59.99 | 88.36 | 83.58 |
| 16 | 10% | 160.48 | 152.50 | 219.44 | 201.23 |

All 100% runs delivered 50000 spans. Zero sampling delivered none and counted
50000 unsampled pushes. Partial sampling delivered 4610–5220 spans per run
across both versions; random sampling does not produce identical workloads.
Healthy runs had no queue drops, export failures, propagation misses, mismatched
hooks, or pending records after drain. Sink-pad reference counts stayed at 2.

Single-source sampled results were noisy: the initial five-run median regressed
from 45.81 to 56.60 ms, while 15 subsequent comparisons measured 39.79 to
33.88 ms. Across all 20 runs, v1 ranged from 31.18–57.48 ms and v2 from
30.82–58.55 ms. The pooled result is reported above rather than selecting the
favorable rerun. It is not evidence of a reliable single-thread speedup.
Named-worker CPU has 10 ms resolution; its pooled single-source medians were
130 ms and 120 ms. These microbenchmarks are not general throughput guarantees.

## Metadata churn and shared buffers

The churn fixture renames an unrelated element every ten source buffers. The
tee fixture uses two queued branches per source, still totaling five pushes per
buffer. Five runs per version and source count, all delivering 50000 spans.

| Sources | Churn streaming CPU v1 | Churn streaming CPU v2 | Tee process CPU v1 | Tee process CPU v2 |
| --- | ---: | ---: | ---: | ---: |
| 1 | 57.17 | 35.98 | 287.04 | 260.18 |
| 4 | 133.43 | 64.42 | 289.34 | 280.94 |
| 16 | 363.75 | 99.80 | 298.95 | 250.44 |

Targeted invalidation avoids rebuilding unrelated descriptors during churn.
The 16-source churn case used about 3.6× less summed streaming CPU. Changes to
an actual ancestor or peer still refresh the affected descriptions, as tested.

Tee runs recorded 30336–39371 shared-buffer fallback observations, with zero
propagation misses. Process CPU includes the queue threads; source CPU does not.
This is not an isolated sidecar-lock benchmark: shared buffers can already have
GstMeta and retain their common ancestor without using a sidecar. A separate
eight-thread unit test exercises concurrent sidecar installation, reads, and
reuse after the buffer becomes writable again. Tee source CPU did not improve
in every case (single-source median 44.00 to 47.97 ms).

## Bounded failure behavior

Three runs each at 1, 4, and 16 sources used queue capacity 128 and batches of
64. A receiver delaying each response by 50 ms received 192–256 spans per run;
the remaining 49744–49808 completed spans were explicitly counted as queue
drops. An absent receiver delivered zero spans; failed-export and queue-drop
counts sum to 50000 in every run. Pending records returned to zero throughout.
These are bounded-loss checks, not equivalent-delivery performance comparisons.

## Validation

- `cargo fmt --all`: passed.
- `cargo clippy --workspace --all-features -- -D warnings`: passed.
- `cargo test`: passed all six default-member tests.
- `cargo test -p gst-otel-tracer`: passed 14 unit tests and seven collector-backed
  integration tests. The ignored child entry point is exercised by those tests.
- Vendored trace-conversion tests: both passed, including multiple scopes.
- `cargo audit`: the same 22 vulnerabilities and advisory IDs as the original
  review baseline. No dependency versions changed in this pass.
- Required full nightly `udeps`: still blocked by `rustix 0.37.28` using reserved
  compiler attributes with the installed nightly. Its `rustix_use_libc`
  workaround completes analysis and reports only the pre-existing unused
  `console` and `pyroscope_pyspy` dependencies in the independent Pyroscope tracer.
- OTel-only nightly `udeps`: passed.

Added coverage verifies that rejected pushes create no descriptions, unrelated
pipelines retain cached descriptions, ancestor rename and relink refresh the
right values, weak subscriptions do not retain pads, concurrent sidecars remain
valid, and 129 producer retirements preserve counts across shard reuse. Existing
tests cover actual OTLP IDs, parentage, sampling, trace state, clocks, limits,
buffer copies, log correlation, queue saturation, and concurrent shutdown.

## Reproduce

Preserve the `ab5b76f` release plugin in a separate directory before building v2:

```sh
cargo build --release -p gst-otel-tracer
python3 tracer/otel/bench/run.py --baseline /path/to/v1 --baseline-sampling \
  --streams 1 4 16 --modes none sampled unsampled partial --drain-ms 0 \
  --repeats 5 --output followup-results.json
python3 tracer/otel/bench/run.py --baseline /path/to/v1 --streams 1 \
  --modes sampled --drain-ms 0 --repeats 15 --output sampled-repeat.json
python3 tracer/otel/bench/run.py --baseline /path/to/v1 --streams 1 4 16 \
  --modes sampled --drain-ms 0 --repeats 5 --churn-every 10 --output followup-churn.json
python3 tracer/otel/bench/run.py --baseline /path/to/v1 --streams 1 4 16 \
  --modes sampled --drain-ms 0 --repeats 5 --tee-queues --output followup-tee.json
python3 tracer/otel/bench/run.py --streams 1 4 16 --modes slow absent \
  --drain-ms 0 --repeats 3 --output followup-failures.json
```

[All 213 measurements and counters](followup-results-2026-10-20.csv), including
the initial regression and subsequent comparisons. The [original results](RESULTS.md)
remain available as the separate comparison against the pre-rewrite tracer.
