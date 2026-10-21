# OpenTelemetry push tracing for GStreamer

`otel-tracer` records `gst_pad_push` calls and exports OTLP/HTTP protobuf traces.
It does not create live SDK spans, install a global tracer provider, or start a
Pyroscope agent. Profile correlation needs a separate implementation; the old
span processor has been removed. The independent Pyroscope tracer is unchanged.

## Run

```sh
cargo build --release -p gst-otel-tracer
export GST_PLUGIN_PATH="$PWD/target/release"
export GST_TRACERS=otel-tracer
export OTEL_SERVICE_NAME=gstreamer-pipeline
export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318
export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf
gst-launch-1.0 videotestsrc num-buffers=100 ! videoconvert ! fakesink
```

The tracer uses OpenTelemetry Rust 0.33.1 for context, IDs, resource detection,
and the optional log exporter. Traces use a dedicated OTLP/HTTP protobuf encoder
and reqwest 0.13 transport. HTTPS uses Rustls with the platform's native trust
roots. This build supports HTTP protobuf without compression. `OTEL_SERVICE_NAME`
and `OTEL_RESOURCE_ATTRIBUTES` are read by the SDK's resource detectors.

`OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` is used exactly; otherwise `/v1/traces` is
appended to `OTEL_EXPORTER_OTLP_ENDPOINT` (default `http://localhost:4318`).
`OTEL_EXPORTER_OTLP_TRACES_HEADERS` and `_TIMEOUT` override their general OTLP
counterparts. Headers use comma-separated, percent-encoded key/value pairs;
timeouts are milliseconds and default to 10000. Unsupported protocol or
compression settings disable tracing with an initialization error.

## Capture and export

A source starts a trace for each buffer journey. Synchronous nested pushes use a
bounded thread-local stack. Buffer metadata carries an owned correlation token
across queues and clones ownership when buffers are copied or transformed. A
recycled source buffer starts a fresh journey. An external valid OTel context at
the source supplies the parent, sampling decision, and trace state. OTLP records
whether that parent is remote; subsequent locally captured spans have local
parents, including across queues.

Pad/element names, paths, and peer descriptions are cached per pad. Weak
subscriptions to the pad, its peer, and their ancestors invalidate descriptions
when those objects change. Unrelated pipelines do not invalidate the cache.
Pending records retain their captured description. Cached descriptors own
strings, not pad references. First sampled use and invalidation rebuild metadata
on the streaming thread; the steady path does no descriptive formatting. Thread
descriptions are cached on first sampled use of each thread.

Rejected traces propagate a rejection token without loading descriptions,
reading buffer sizes, creating attributes, or enqueuing records. Source
classification is cached separately; rejected synchronous descendants do not
consult the pad cache at all. Roots with a zero sampling ratio need no random
IDs. Rejected frames store just pad identity and the correlation token; sampled
records use a separate stack. Unchanged rejected metadata tokens are not rewritten,
while external identity and recycled-root decisions remain intact.
Partial sampling generates a trace ID once per root using the SDK 0.33 ratio
algorithm; descendants inherit the result.

Sampled pushes capture IDs, monotonic timestamps, buffer identity/size, thread
identity, a descriptor reference, and the flow result. Post hooks use a bounded
nonblocking queue. One worker drains compact records into reusable batches and
encodes them directly into OTLP protobuf. Static site and thread attributes are
encoded once on the worker and cached with their descriptions. Resource and scope
headers are encoded once per exporter. Dynamic integers, IDs, timestamps, status,
and flags are written directly, without `SpanData` or per-span protobuf objects.
The SDK conversion remains a test oracle for decoded message equivalence.

The worker reuses its encoding buffers and sends reference-counted `Bytes`
bodies, without cloning the encoded payload. A single request is in flight at a
time. Both blocking and worker-owned asynchronous reqwest adapters are available;
neither depends on an application Tokio runtime. Logs retain the upstream SDK
exporter. Producer handoffs remain individual records: a measured batching
experiment increased streaming CPU and was removed.
The first sampled hook anchors GStreamer's monotonic clock to wall time; later
wall-clock adjustments do not change recorded durations.

Spans are named `gst.pad.push`, with `src_pad.*`, `sink_pad.*`, `buffer.id`,
`buffer.size`, `thread.id`, `thread.name`, `ts.start`, `ts.end`, and
`gst.flow_return` attributes. A negative flow return sets error status. Duration
is the inclusive push call, including synchronous downstream work; it is not
exclusive element processing time or end-to-end buffer latency.

## Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `OTEL_TRACES_SAMPLER` | `parentbased_always_on` | Also accepts `parentbased_always_off` and `parentbased_traceidratio` |
| `OTEL_TRACES_SAMPLER_ARG` | Required for ratio sampler | Finite number between 0 and 1 |
| `GST_OTEL_QUEUE_CAPACITY` | 8192 | Pending compact records, 1–1048576 |
| `GST_OTEL_BATCH_SIZE` | 512, capped by queue size | Worker batch, 1–queue capacity |
| `GST_OTEL_HTTP_CLIENT` | blocking | `blocking` or `async`; both send one batch at a time |
| `GST_OTEL_EXPORT_INTERVAL_MS` | 200 | Partial batch export interval, 1–60000 ms |
| `GST_OTEL_FLUSH_TIMEOUT_MS` | 10000 | Pipeline-stop drain deadline, 1–60000 ms |
| `OTEL_SPAN_ATTRIBUTE_COUNT_LIMIT` | 128 | 0–4096; omitted attributes increment OTLP's dropped count |
| `OTEL_SPAN_ATTRIBUTE_VALUE_LENGTH_LIMIT` | 4096 | 1–1048576 Unicode characters per descriptive value |
| `GST_OTEL_LOGS` | false | `true` or `1` opts into the separate debug-log bridge |

Invalid tracing configuration disables hooks and emits an `otel-tracer` error.
Non-parent-based and custom samplers are not supported. `OTEL_BSP_*` does not
configure this worker; there is no SDK batch span processor. No events or links
are currently produced. SDK span processors and custom global tracer providers
do not intercept this export path.

The memory bound is a queue plus one export batch, up to 256 nested frames per
streaming thread, and metadata for live pads/buffers. Counters and in-progress
submission counts use 64 fixed, cache-aligned shards selected per thread;
additional threads share shards safely, and retired threads leave their totals
available without an accumulating registry. The worker's reusable OTLP buffers
are bounded by the batch and a 64 MiB encoded request limit. Cached encoded
attributes live with their descriptors; invalidation creates a new descriptor
while pending records retain the old one. Thread and pipeline counts remain
application-controlled. Overflow drops the new record; it never waits for the
receiver. A failed export counts the entire attempted batch as failed, with no
additional retry queue. Trace transport makes one export attempt per batch; SDK log-exporter retries
are explicitly disabled. Responses are consumed for connection reuse and capped
at 4 MiB. Non-success HTTP status, request timeout, oversized responses, and
encoded bodies over 64 MiB count the attempted trace batch as failed. A timeout can leave
delivery uncertain, and exporter
success does not guarantee durable storage or detect collector partial rejection.

## Drain and counters

A successful pipeline transition from READY to NULL flushes completed records.
Stop streaming before requesting the final drain. Flush covers records enqueued
before its queue marker, not open pushes or concurrent later submissions. Rust
applications can use:

```rust,ignore
let counters = gstoteltracer::flush(std::time::Duration::from_secs(5))?;
let snapshot = gstoteltracer::stats()?;
gstoteltracer::shutdown(std::time::Duration::from_secs(5))?;
```

Shutdown closes submissions, drains accepted records, and shuts down the
exporter. It is process-wide and cannot be restarted. Records completing after
closure increment `dropped_closed`. On a caller timeout, the worker may still be
finishing a request; counters remain inspectable, and shutdown may be retried.
Abrupt process termination cannot guarantee a drain.

C and other GStreamer applications can find the tracer through
`gst_tracing_get_active_tracers()` and read its `stats` property, a `GstStructure`.
The same cumulative fields are available through the Rust API:

- `started_spans`, `completed_spans`, `enqueued_spans`, `exported_spans`.
- `dropped_queue_full`, `dropped_closed`, `failed_export_spans`, `failed_exports`.
- `pending_spans`, including queued, batched, and currently exporting records.
- `unsampled_pushes`, `skipped_closed` (sampled pushes begun after shutdown),
  `propagation_misses`, `propagation_fallbacks`,
  `dropped_depth`, `mismatched_hooks`.

At quiescence, completed = exported + failed-export + queue-full + closed +
pending. Started − completed identifies open pushes. Counters are a concurrent
snapshot, not an atomic transaction. `pending_spans` is derived from enqueued
minus exported/failed spans and clamps transient inconsistent reads to zero;
it is exact after producers stop and a successful drain completes.
Record loss can produce incomplete traces
and missing parents; sampling decisions do not change when telemetry is dropped.
`flush` reports export failures since the previous flush; cumulative counters
retain all failures, including ones already reported during pipeline stop.

## Correlation boundaries

- Shared buffers cannot safely accept mutable metadata. Branches retain the
  last common ancestor; exact immediate parentage across every branch is not
  promised. An initially shared buffer without our metadata uses a synchronized
  sidecar. Its slot is installed once under a global installation lock; later
  access synchronizes only on that buffer. Copying that buffer cannot copy the
  sidecar. A later asynchronous
  push with missing context is counted and suppressed, never resampled as a root.
- Elements that strip metadata or create unrelated buffers asynchronously need
  element-specific propagation. Generic fan-in retains the first copied input
  context and does not reconstruct multi-input links. Buffer lists and pull-mode
  operations are outside the current push-hook coverage.
- Capture does not attach SDK contexts per push. Application instrumentation can
  explicitly bridge `current_span_context()` if it needs a child SDK span. The
  optional log bridge uses native log trace/span fields from the capture stack.
- Debug log formatting still runs on the caller thread when opted in. Logs use
  their own SDK batch processor, queue, and accounting; the trace limits and
  counters do not cover logs. The standard GStreamer log handler remains active.

## Validation and benchmark

```sh
cargo test -p gst-otel-tracer
cargo build --release -p gst-otel-tracer
python3 tracer/otel/bench/run.py
# Compare against a separately built old plugin with Pyroscope removed:
python3 tracer/otel/bench/run.py --baseline /path/to/old/plugin/directory
# Compare compact-capture versions, including parallel streams and sampling:
python3 tracer/otel/bench/run.py --baseline /path/to/compact-v1 \
  --baseline-sampling --streams 1 4 16 --repeats 5 --drain-ms 0
```

The Linux benchmark needs a C compiler, GStreamer development files, and Python.
It measures 10000 buffers total, divided between `--streams` independent sources,
with five pushes per buffer, against a local receiver. It records pipeline wall
time, summed source-thread CPU, process CPU including export
drain, exporter CPU at Linux scheduler-tick resolution, actual received spans,
and the tracer's counters. Logging is disabled. Healthy cases use a 65536-record
queue to compare full delivery; slow/absent cases deliberately use a queue of
128 and batches of 64. `--churn-every 10` renames an unrelated element every ten
source buffers; `--tee-queues` substitutes tee/queue stages to exercise shared
buffers and asynchronous propagation (source CPU then excludes queue threads).
Results and logs go under `target/otel-bench`. See the
[follow-up measurements](bench/FOLLOWUP_RESULTS.md) for results and limitations.
The [0.33.1 upgrade results](bench/UPGRADE_RESULTS.md) cover the previous SDK
export path. [Direct export measurements](bench/DIRECT_RESULTS.md) compare the
current encoder and transport, rejected producer batching, adapter/batch-size
experiments, and isolated encoding cost. Use `--http-client`, `--batch-size`, and
`--keep-alive` to reproduce transport comparisons. The default benchmark collector
uses HTTP/1.0; `--keep-alive` tests persistent HTTP/1.1 connections.
