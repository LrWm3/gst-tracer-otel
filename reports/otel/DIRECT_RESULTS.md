# Direct OTLP encoding and capture follow-up

Baseline: `21e0b7a`, OpenTelemetry 0.33.1 with compact capture and SDK `SpanData`
export. Implementation branch: `impl/otel-compact-capture`.

The retained implementation lowers sampled process CPU by **57–64%** in the
final comparison, with all 50000 expected spans delivered per run. Sampled
streaming CPU is approximately unchanged (+1–2%). The main gain is on the export
side. Producer batching was implemented, tested, measured, and removed after it
increased streaming CPU.

## Retained changes

- Encode compact records directly into OTLP protobuf, avoiding `SpanData`,
  attribute-vector construction, grouping maps, and per-span protobuf objects.
- Cache encoded site/thread attributes with their descriptors. Cache resource
  and instrumentation-scope headers in the exporter. Reuse worker batch and
  encoding buffers, including HTTP body allocations when ownership permits.
- Send reference-counted `Bytes` bodies without a payload clone. Keep one request
  in flight and the existing queue, sampling, flush, shutdown, and loss counters.
- Support blocking and worker-owned async reqwest adapters. Retain the existing
  **512-span/blocking** defaults: the experiment did not establish a consistently
  better alternative across conditions.
- Separate compact capture frames from sampled record storage. Avoid rewriting
  identical rejected tokens while preserving external identity, trace state,
  copies, and recycled-root decisions.

Trace transport now belongs to this crate. The upstream API/SDK still supply
context, ID generation, resource detection, and optional log export. The
production trace path contains no `SpanData`; the test reference converter still
uses the public SDK and protobuf conversion. Generated protobuf types remain in
use for static attributes/resources and decoding tests.

## Method and limitations

Intel i5-1345U, Rust 1.89, GStreamer 1.24.2; release builds restricted to CPUs
4–11. Each pipeline run processes 10000 buffers total through five pushes,
divided among 1, 4, or 16 source threads. Healthy queues hold 65536 records to
measure full delivery. Logs are disabled. Pipeline wall time excludes the final
drain; process CPU includes it. Source-thread CPU includes ordinary GStreamer
work. Exporter CPU uses coarse Linux scheduler ticks and excludes the blocking
adapter's separate reqwest helper thread, so process CPU is the useful transport
comparison.

This is a shared workstation. Concurrent Prometheus benchmark processes were
observed during later runs; no other work was stopped or modified. Affinity does
not isolate CPU frequency, caches, or scheduling. Variant order rotates, but
absolute timings and small differences remain noisy. In particular, persistent
connection results and late 16-stream partial-sampling measurements should not
be interpreted as isolated causal measurements.

[Raw pipeline results](DIRECT_RESULTS.csv) contain **445 runs**: 20 body-copy,
105 exploratory batched, 30 isolated batching, 90 transport matrix, 36 persistent
matrix, 105 final comparison, 20 persistent final comparison, 12 failure,
18 tee/queue, and 9 unrelated-metadata-churn runs. The exploratory batched series
also overlapped the initial clippy check; it is not used for the final claims.
[Encoding results](ENCODING_RESULTS.csv) contain five separate timing rounds.

## Final comparison

Medians of five repetitions, milliseconds. Each sampled run delivers 50000
spans, unsampled runs deliver zero, and partial sampling keeps roughly 10% of
whole buffer journeys. Queue-full, closed, and failed-export counts are zero in
these healthy runs; pending counts are zero after drain.

| Streams | Sampling | Source CPU before | Source CPU after | Process CPU before | Process CPU after |
| --- | --- | ---: | ---: | ---: | ---: |
| 1 | 100% | 57.28 | 58.37 | 270.98 | 96.91 |
| 4 | 100% | 73.49 | 74.95 | 282.25 | 117.07 |
| 16 | 100% | 88.01 | 88.99 | 358.80 | 154.97 |
| 1 | 10% | 51.63 | 50.71 | 79.41 | 61.88 |
| 4 | 10% | 62.46 | 61.77 | 89.32 | 70.26 |
| 16 | 10% | 71.07 | 82.46 | 106.52 | 94.95 |
| 1 | 0% | 46.03 | 44.60 | 47.19 | 45.71 |
| 4 | 0% | 55.55 | 55.63 | 57.81 | 57.89 |
| 16 | 0% | 63.95 | 65.80 | 71.50 | 74.17 |

The rejected path is a modest improvement in the single-stream run, effectively
flat at four streams, and slightly slower in the noisy 16-stream series. The
16-stream partial-sampling source CPU regressed in this series; these results do
not support a general streaming-speedup claim. Sampled worker CPU falls from
170–200 ms to about 10 ms at scheduler-tick resolution.

With persistent HTTP/1.1 connections, the final paired comparison also delivers
all spans. Median process CPU falls from 455.56 to 151.77 ms at one stream and
489.69 to 188.05 ms at four streams. Source CPU changes from 100.20 to 99.50 ms and
129.06 to 127.86 ms respectively. These runs occurred under heavier system load;
compare variants within the series, not absolute times against the table above.

## Isolated encoding

The manual release benchmark measures conversion plus protobuf encoding,
excluding capture, queueing, and HTTP. Both paths receive equivalent records;
input duplication is outside the timers. The reference consumes records through
`SpanData` and upstream grouping/protobuf conversion. The direct path reuses
buffers and warmed static-attribute caches. Batches contain 512 spans with six
descriptive and seven dynamic attributes, with empty trace state. Execution
order alternates within each round.

For 102400 spans per round, median reference time is **320.89 ms** and direct
encoding is **15.89 ms**, about **20.2× faster**. Each encoded batch is 241734
bytes. This is an encoding-stage measurement, not a whole-tracer speedup. A
nonempty trace-state header still requires formatting on the worker.

## HTTP body, batch size, and adapter experiments

The isolated SDK body experiment changes only the upstream retry body's storage
from `Vec<u8>` to `Bytes`; retries remain disabled. Process CPU changes from
274.85 to 267.98 ms at one stream (−2.5%), and 287.34 to 284.20 ms at four streams
(−1.1%). Streaming CPU changes are small. This does not explain most of the old
export cost. The production direct transport obtains the same ownership benefit
without vendoring the SDK.

The 90-run matrix compares six settings, rotating their order across five
repetitions. Every run delivers 50000 spans. Process CPU medians in milliseconds,
with the original connection-per-request HTTP/1.0 receiver:

| Batch | Adapter | 1 stream | 4 streams | 16 streams |
| ---: | --- | ---: | ---: | ---: |
| 512 | blocking | 99.64 | 118.47 | 140.04 |
| 512 | async | 114.34 | 130.11 | 138.76 |
| 1024 | blocking | 113.88 | 124.78 | 147.08 |
| 1024 | async | 95.96 | 111.05 | 129.41 |
| 2048 | blocking | 89.85 | 104.87 | 124.45 |
| 2048 | async | 88.95 | 106.14 | 122.74 |

2048-span batches save about 10–11% over 512/blocking in that particular setup.
Async alone does not consistently help. A separate 36-run persistent-connection
matrix, under substantial concurrent load, does not reproduce a consistent
larger-batch advantage. Keep the defaults; use `GST_OTEL_BATCH_SIZE` and
`GST_OTEL_HTTP_CLIENT` to tune against the actual collector and workload. Async
uses a single worker-owned Tokio runtime and still sends batches sequentially.

## Rejected producer batching

The prototype collected completed nested pushes until the outer push returned,
or a 32-record cap was reached. It used a bounded shared queue, a separate record
budget, and a bounded vector-recycling pool. Lifecycle tests covered deep-stack
partial batches, thread retirement, shutdown races, runtime destruction, and
splitting producer batches across exporter batches. It also introduced extra
partial-batch flush semantics while an outer push was still open.

In a dedicated 30-run comparison against the same direct encoder without
producer batching, source CPU medians were:

| Streams | Individual handoffs | Batched handoffs | Increase |
| --- | ---: | ---: | ---: |
| 1 | 58.76 ms | 69.83 ms | 18.8% |
| 4 | 74.75 ms | 99.44 ms | 33.0% |
| 16 | 93.01 ms | 119.44 ms | 28.4% |

The experiment measured the whole batching design, not each internal cost.
Extra ownership, buffer recycling, and record movement are plausible causes,
not separately established measurements. The prototype is preserved as
[producer-batching.patch](../../tracer/otel/bench/producer-batching.patch), but is not in the compiled
implementation. Individual handoffs retain the existing flush contract and
record bounds. Per-producer queues remain a separate design question.

## Correctness, bounds, and checks

Differential tests decode direct output and compare it with SDK output for
attribute limits including zero, omitted-attribute counts, local/remote parents,
invalid parent IDs, trace flags, trace state, negative integer values, error
status, end-before-start clamping, Unicode metadata, array resource attributes,
and resource schema URLs. Cached reuse and oversized-body recovery are covered.
Collector tests verify complete traces across queues, transformations, tee
branches and bins, descriptor refresh after rename, stable pad reference counts,
sampling, native log correlation, both HTTP adapters, custom endpoints/headers,
and exactly one received copy per failed 503 batch. Both adapters also have a
timeout test against an unresponsive receiver.

The transport preserves the existing supported subset of
[OTLP HTTP configuration](https://opentelemetry.io/docs/specs/otel/protocol/exporter/):
signal endpoint/header/timeout precedence, protobuf content type, resource
configuration, and platform-root HTTPS through reqwest/Rustls. It keeps the
existing explicit no-retry policy. A 64 MiB request limit and 4 MiB response limit
bound transport buffers. Failed requests count the entire batch; timeout
delivery remains uncertain, and successful HTTP status does not account for
collector partial rejection. See the [tracer README](../../tracer/otel/README.md) for the full
supported behavior and propagation boundaries.

All slow/absent receiver runs satisfy
`completed = exported + failed + queue-full + closed`, with zero pending after
drain. Slow runs receive 192–320 spans and
explicitly drop 49680–49808 at the queue. Absent runs receive zero, count
11225–43191 attempted spans as failed, and drop the remainder at the queue.
Healthy tee/queue and unrelated-rename workloads also pass delivery/accounting
assertions. No dropped telemetry is included as a speed gain.

Required checks:

- `cargo fmt --all`: pass.
- `cargo clippy --workspace --all-features -- -D warnings`: pass.
- `cargo test`: pass (the default Prometheus member: six tests).
- `cargo test -p gst-otel-tracer`: pass (21 unit and eight collector tests;
  benchmark and subprocess entry point are intentionally ignored by default).
- `cargo build --release -p gst-otel-tracer`: pass.
- `cargo audit`: the same 22 workspace vulnerabilities and 15 warnings recorded
  previously; dependency versions are unchanged by this pass.
- Required full nightly `udeps`: still blocked by the existing `rustix 0.37.28`
  reserved-compiler-attribute failure. With `RUSTFLAGS=--cfg=rustix_use_libc`, it
  reports only the existing Pyroscope `console` and `pyroscope_pyspy` findings.
  OTel-only nightly `udeps --all-targets` passes without the workaround.

## Reproduction

Build baseline `21e0b7a` separately and preserve its release plugin directory.
Build the current branch before running the comparisons:

```sh
cargo build --release -p gst-otel-tracer
taskset -c 4-11 python3 tracer/otel/bench/run.py \
  --baseline /path/to/21e0b7a/plugin --baseline-sampling \
  --streams 1 4 16 --modes none sampled unsampled partial \
  --repeats 5 --drain-ms 0 --output direct-final.json

# Repeat for batch sizes 512/1024/2048 and adapters blocking/async.
# Add --keep-alive for persistent HTTP/1.1 connections.
taskset -c 4-11 python3 tracer/otel/bench/run.py \
  --streams 1 4 16 --modes sampled --batch-size 2048 --http-client async \
  --repeats 5 --drain-ms 0 --output transport.json

taskset -c 4-11 cargo test --release -p gst-otel-tracer --lib \
  encoding_benchmark -- --ignored --nocapture
```

For the body-only experiment, use baseline `21e0b7a`, copy the published
`opentelemetry-otlp 0.33.1` source to a temporary directory, apply
[otlp-body-bytes.patch](../../tracer/otel/bench/otlp-body-bytes.patch) there with `git apply --unidiff-zero`,
and point a temporary
`[patch.crates-io]` entry at it. Compare that build with the untouched baseline;
do not include any other implementation changes.

For producer batching, preserve the current plugin, apply `producer-batching.patch`
with `git apply --unidiff-zero` to this branch in a separate checkout, format/build
it, and compare the two
plugins using identical 512/blocking settings. The patch is experimental evidence,
not an additional patch required to build the normal tracer.

## Bounded export tuning — 2026-10-25

Traces now support optional `GST_OTEL_EXPORT_CONCURRENCY=1..4` (default 1). Each request worker has its own encoder/client, with no waiting-job queue; capture still uses the existing bounded FIFO. Flush/shutdown barriers wait for every earlier request, including out-of-order completions. Standard OTLP `none|gzip` request compression is supported for traces, metrics and logs. Metrics/log export remains serialized. Partial acceptance is accounted per request without retries.

The defaults remain **512 spans, one request, no compression**. Against a receiver with 50 ms added request latency, increasing batches to 2048 eliminated loss in this workload with less memory than four concurrent 512-span requests. Gzip reduced bytes by about 90%, but approximately doubled process CPU on the local receiver; it is a bandwidth tradeoff. More concurrency is useful only when the collector can handle parallel work.

The synthetic workload emits 10,000 buffers through five push sites, with a 100 µs pause per buffer: 50,000 spans per run. Each configuration used one discarded warm-up and three measured repetitions in rotating order, an 8192-record queue, a 200 ms partial-batch interval, and a persistent local HTTP receiver on a separate CPU. The delayed case adds 50 ms per request and permits concurrent requests. This is a sustained, paced trace-delivery test, not a video-pipeline overhead estimate.

All values below are medians. CPU covers PLAYING through NULL; startup is excluded. NULL includes the final trace drain. Peak RSS covers the entire child. Receiver CPU/storage are excluded.

| Receiver delay | Batch | Compression | Concurrency | Accepted / 50,000 | Process CPU | Peak RSS | Final NULL/drain | Wire bytes |
| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 0 ms | 512 | none | 1 | 50,000 | 188.5 ms | 16.92 MiB | 0.81 ms | 28.86 MiB |
| 0 ms | 2048 | none | 1 | 50,000 | 187.8 ms | 18.68 MiB | 1.57 ms | 28.85 MiB |
| 0 ms | 512 | gzip | 1 | 50,000 | 413.1 ms | 19.52 MiB | 2.75 ms | 2.92 MiB |
| 0 ms | 512 | none | 4 | 50,000 | 197.9 ms | 20.00 MiB | 0.89 ms | 28.91 MiB |
| 0 ms | 2048 | none | 4 | 50,000 | 193.7 ms | 27.33 MiB | 1.47 ms | 28.89 MiB |
| 0 ms | 2048 | gzip | 4 | 50,000 | 398.8 ms | 54.18 MiB | 5.69 ms | 2.87 MiB |
| 50 ms | 512 | none | 1 | 24,064 | 101.8 ms | 16.93 MiB | 833.89 ms | 13.88 MiB |
| 50 ms | 2048 | none | 1 | 50,000 | 120.7 ms | 18.68 MiB | 78.03 ms | 28.85 MiB |
| 50 ms | 512 | gzip | 1 | 23,230 | 208.3 ms | 19.47 MiB | 916.34 ms | 1.35 MiB |
| 50 ms | 512 | none | 4 | 50,000 | 197.5 ms | 20.00 MiB | 51.74 ms | 28.91 MiB |
| 50 ms | 2048 | none | 4 | 50,000 | 192.3 ms | 27.27 MiB | 52.09 ms | 28.89 MiB |
| 50 ms | 2048 | gzip | 4 | 50,000 | 398.1 ms | 53.28 MiB | 56.11 ms | 2.87 MiB |

Every accepted span was decoded at the receiver, with zero pending spans or failed requests after drain. Missing spans in the two delayed 512-span/one-request cases were explicitly counted as queue-full drops. Their lower CPU and bytes reflect lost work, not better efficiency. Four concurrent 512-span requests reached four simultaneous receiver operations; 2048-span requests needed at most two at this producer rate. On the local receiver only one request was active at a time, so added concurrency gave no throughput benefit. The delayed default lost about 52% of generated spans; either 2048-span batches or four concurrent 512-span requests delivered all 50,000 in every measured repetition.

Concurrency adds one in-flight batch/encoder per worker plus the dispatcher batch. Gzip adds compressed-body and compressor allocations. The four-worker/2048/gzip case peaked near **54 MiB**, versus about **17 MiB** for the default. Queue growth alone would only postpone sustained overload. These results do not establish optimal settings for a remote collector or an application that produces bursts at a different rate.

Unit and decoded-receiver tests cover gzip on all three OTLP signals and both HTTP adapters, four independently blocked requests, out-of-order completion, partial/all rejection, malformed responses, exporter panic accounting, repeated cleanup deadlines, and no duplicate delivery. All required formatting, Clippy, test, audit and dependency-use checks passed; audit retains the existing allowed Tokio advisory.

Raw [CSV](export-throughput/2026-10-25/results.csv), [source/build settings](export-throughput/2026-10-25/metadata.json), and per-run counter output are retained. Reproduce from the repository root:

```sh
cargo build --release -p gst-otel-tracer --features profiles
mkdir -p target/otel-bench
cc -O2 -Wall -Wextra tracer/otel/bench/pipeline.c -o target/otel-bench/pipeline \
  $(pkg-config --cflags --libs gstreamer-1.0)
python3 tracer/otel/bench/export_throughput.py \
  --output reports/otel/export-throughput/recheck
```
