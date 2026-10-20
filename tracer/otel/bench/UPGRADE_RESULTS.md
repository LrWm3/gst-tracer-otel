# OpenTelemetry 0.33.1 upgrade

Upgraded on 2026-10-20 on branch `impl/otel-compact-capture`. Comparison baseline
is `77475bf`, the optimized tracer using OpenTelemetry 0.30 and the local
protobuf conversion patch.

## Dependency and behavior changes

The API, SDK, OTLP exporter, protobuf definitions, and HTTP adapter now use
0.33.1, the latest published release verified through crates.io. The test decoder
uses prost 0.14.4. Cargo resolved dependencies compatible with the existing
Rust 1.89 toolchain; this change does not upgrade GStreamer.

The vendored protobuf crate and Cargo patch are removed. Upstream 0.33.1 now
consumes owned `SpanData` during conversion, including one scope clone per
distinct scope. See the [upstream conversion change](https://github.com/open-telemetry/opentelemetry-rust/pull/3652).

The [0.33 release](https://github.com/open-telemetry/opentelemetry-rust/blob/main/docs/release_0.33.md)
introduces defaults relevant to this tracer:

- Retries are enabled upstream. Both trace and log builders explicitly disable
  them to retain one attempt per batch and the existing failure/drain behavior.
- The old `reqwest-rustls-webpki-roots` feature is removed. `reqwest-rustls`
  uses reqwest 0.13 and platform trust roots; HTTPS trust behavior therefore
  changes from the previous bundled root set.
- HTTP requests have a default 64 MiB encoded-body limit. Oversized trace
  batches are counted as failed exports. Invalid endpoint environment values
  now fail initialization instead of silently falling back to localhost.

The SDK's new `SpanData.parent_span_is_remote` field is populated from the
captured parent. A remote external context applies to the first local span;
descendants reference local spans, including after a queue boundary. Sampling,
trace state, bounded capture, resource setup, and accounting are retained.

## Release comparison

There are 86 validated runs in the [raw measurements](upgrade-results-2026-10-20.csv):
54 comparisons across 1, 4, and 16 sources at 0%, 10%, and 100% sampling;
20 additional sampled comparisons restricted to CPU cores 4–11; and 12
slow/absent receiver checks. Each process handles 10000 buffers and five pushes
per buffer. Healthy queues hold 65536 records with batch size 512. Slow/absent
checks use queue 128 and batch 64. All sampled healthy cases delivered 50000
spans with zero drops, failures, propagation misses, or pending records after
drain. Rejected pushes remained unsampled across the pipeline.

Initial timings varied substantially on this shared Intel i5-1345U machine.
To reduce differences between its performance and efficiency cores, the extra
comparison restricted both versions to cores 4–11 (efficiency cores). This is
still a shared-machine microbenchmark. Medians from those five runs per cell,
in milliseconds:

| Sources | Streaming CPU 0.30 | Streaming CPU 0.33.1 | Process CPU 0.30 | Process CPU 0.33.1 | Export worker CPU 0.30 | Export worker CPU 0.33.1 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 57.62 | 57.61 | 235.92 | 272.21 | 140 | 170 |
| 4 | 73.84 | 73.54 | 262.01 | 281.88 | 150 | 160 |

Streaming CPU is essentially unchanged in this comparison. Total process CPU
is about 8–15% higher, and export worker CPU also increased. Worker measurements
have 10 ms resolution. This upgrade is not a measured performance improvement.
The upstream HTTP retry wrapper still clones the encoded request body for each
attempt even with retries disabled; this is a remaining cost, not a measured
explanation for the entire difference. No local exporter fork was introduced.

The slow receiver delivered 192–256 spans and explicitly dropped the remainder.
The absent receiver delivered zero spans; failed-export and queue-drop counts
sum to 50000 in every run. Pending records returned to zero. These loss checks
are not equivalent-delivery performance comparisons.

Reproduce after preserving the baseline release plugin in a separate directory:

```sh
cargo build --release -p gst-otel-tracer
python3 tracer/otel/bench/run.py --baseline /path/to/030-plugin \
  --baseline-sampling --streams 1 4 16 --modes sampled unsampled partial \
  --drain-ms 0 --repeats 3 --output upgrade-results.json
taskset -c 4-11 python3 tracer/otel/bench/run.py --baseline /path/to/030-plugin \
  --streams 1 4 --modes sampled --drain-ms 0 --repeats 5 --output upgrade-affinity.json
python3 tracer/otel/bench/run.py --streams 1 4 16 --modes slow absent \
  --drain-ms 0 --repeats 2 --output upgrade-failures.json
```

## Validation

- Formatting, workspace clippy with all features and warnings denied, and the
  six default-member tests pass.
- OTel passes 15 unit tests and seven collector-backed integration tests.
  The ignored child entry point is exercised by those integration tests.
- The new test checks remote/local parent flags through capture and protobuf
  conversion. Existing collector assertions now also inspect local-parent flags.
- The failure receiver now returns retryable HTTP 503; exactly one received
  copy of each failed span verifies the explicit no-retry configuration.
- Release build succeeds on Rust 1.89.0; all OTel crates resolve to 0.33.1
  from crates.io without a local override.
- `cargo audit` reports the same 22 vulnerabilities with the same advisory IDs
  as before this upgrade.
- Required full nightly `udeps` still fails compiling `rustix 0.37.28` because
  the installed nightly rejects its reserved compiler attributes. Retrying with
  `RUSTFLAGS='--cfg=rustix_use_libc'` completes analysis and finds only the existing
  unused `console` and `pyroscope_pyspy` dependencies in the independent Pyroscope
  tracer. OTel-only nightly `udeps` passes.
