# Compact tracing implementation results

Branch: `impl/otel-compact-capture`. Baseline:
`c5c45815bc24b256b97bb7c24b0ea21e278da1ef`. Measured 2026-10-20.

The implementation removes Pyroscope span callbacks, caches pad descriptors,
propagates owned sampling/context tokens, captures bounded compact records, and
constructs public `SpanData` batches on one export worker. It also fixes the
buffer-metadata double free and per-push peer-pad reference leak identified in
the baseline review. Prometheus and independent Pyroscope source are unchanged.

## Release benchmark

Five separate processes per case; 10000 buffers through four identities and a
sink, giving 50000 push spans at 100% sampling. Rust 1.89.0, GStreamer 1.24.2,
Intel i5-1345U, logging disabled. A local HTTP receiver counts protobuf spans.
The baseline plugin removes only Pyroscope creation/registration from the old
implementation. Both healthy implementations deliver all spans before exit.

Times below are medians in milliseconds. Pipeline wall time and source-thread
CPU exclude final drain. Process CPU includes drain and exporter work. Exporter
CPU comes from Linux per-thread accounting at 10 ms resolution; a reported zero
can mean less than one tick. HTTP-client helper threads contribute to process
CPU but are not included in the named export worker's CPU total.

| Case | Pipeline wall | Streaming CPU | Export worker CPU | Process CPU | Received spans per run |
| --- | ---: | ---: | ---: | ---: | ---: |
| No tracer | 21.35 | 20.98 | 0.00 | 21.74 | 0 |
| Old tracer, Pyroscope removed | 185.94 | 184.69 | 190.00 | 416.84 | 50000 |
| Compact capture, 100% | 33.45 | 33.03 | 130.00 | 198.28 | 50000 |
| Compact capture, 0% | 28.71 | 28.31 | 0.00 | 42.49 | 0 |
| Compact capture, 10% | 29.74 | 29.31 | 20.00 | 61.50 | 4830–5075 |
| Slow receiver, queue 128 | 34.28 | 33.88 | 0.00 | 47.83 | 192 |
| Absent receiver, queue 128 | 41.24 | 40.85 | 30.00 | 93.38 | 0 |

For full delivery, median streaming-thread CPU fell from
184.69 ms to
33.03 ms (5.59×).
The streaming CPU ranges were 144.81–192.08 ms for the old tracer and
31.50–56.66 ms for compact capture.
This is a shared-machine microbenchmark, not a general throughput guarantee.

Healthy 100% and partial-sampling runs had zero queue drops, export failures,
propagation misses, mismatched hooks, or pending records after drain. The 0%
case counted all 50000 pushes as unsampled and enqueued nothing. The sink pad's
reference count remained 2 in the new tracer; the baseline retained 10002.

The slow receiver delays each response by 50 ms. With queue 128 and batch 64,
each run delivered 192 spans and explicitly dropped 49808. Those timings are
not equivalent-delivery speedups. With an absent receiver, exported spans are
zero; failed-export plus queue-full counts sum to 50000 in every run. Pending
spans return to zero. The healthy queue is deliberately larger (65536) so the
throughput comparison does not reward telemetry loss.

[Raw measurements and accounting](results-2026-10-20.csv).
Reproduce with `python3 tracer/otel/bench/run.py --baseline DIRECTORY --repeats 5`
after building the release plugin and the baseline variant. The older review
contains the much larger additional cost of the original Pyroscope integration.

## Correctness and required checks

- `cargo fmt --all`: passed.
- `cargo clippy --workspace --all-features -- -D warnings`: passed.
- `cargo test`: passed the default Prometheus member, 6 tests.
- `cargo test -p gst-otel-tracer`: passed 9 unit tests and 7 collector-backed
  integration tests. The ignored child-process entry point is exercised by
  those integration tests.
- `cargo audit`: reports 22 vulnerabilities in existing locked dependencies;
  an audit of the original baseline lockfile returns the same advisory IDs.
  This change does not update unrelated workspace dependencies.
- Required full `cargo +nightly udeps ... --all-targets`: blocked compiling
  `rustix 0.37.28` with the installed nightly (reserved compiler attributes).
  Retrying with its supported `RUSTFLAGS='--cfg=rustix_use_libc'` completes the
  analysis and reports only existing `console` and `pyroscope_pyspy` dependencies
  in `gst-pyroscope-tracer`.
- `cargo +nightly udeps -p gst-otel-tracer --all-targets`: passed.

Tests inspect decoded OTLP IDs, parentage, sampling flags, resource, counts,
and timestamps. They cover 0%, partial, and 100% sampling through queues, tee
branches, converting/copying buffers, bins, pipeline restart and renaming,
pad-reference stability, optional native log correlation, external parent trace
state, failed pushes, depth limits, export failure, queue saturation, and
concurrent shutdown accounting.

## Remaining boundaries

The [tracer documentation](../README.md) describes the supported semantics.
Capture covers pad pushes, with inclusive durations. Arbitrary async buffer
replacement, exact shared-buffer branch parentage, fan-in links, buffer lists,
and pull-mode tracing need further designs. Missing async context is counted
and suppressed. No profile correlation is currently supplied. Logs remain an
opt-in SDK path with separate costs and accounting. Export success is transport
success, not proof of durable collector storage.
