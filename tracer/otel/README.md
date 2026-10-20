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

The exporter uses OpenTelemetry Rust 0.30, including its endpoint, headers,
request timeout, and resource environment configuration. HTTPS uses Rustls with
WebPKI roots. This build supports HTTP protobuf, not gRPC. `OTEL_SERVICE_NAME`
and `OTEL_RESOURCE_ATTRIBUTES` are read by the SDK's resource detectors.

## Capture and export

A source starts a trace for each buffer journey. Synchronous nested pushes use a
bounded thread-local stack. Buffer metadata carries an owned correlation token
across queues and clones ownership when buffers are copied or transformed. A
recycled source buffer starts a fresh journey. An external valid OTel context at
the source supplies the parent, sampling decision, and trace state.

Pad/element names, paths, and peer descriptions are cached per pad. Topology and
name notifications invalidate these descriptions; pending records keep the
snapshot valid at capture time. Cached descriptors own strings, not pad
references. First use and invalidation rebuild metadata on the streaming thread;
the steady path does no descriptive formatting. Thread descriptions are cached
on first sampled use of each thread.

Rejected traces propagate a rejection token without allocating live spans,
reading buffer sizes, creating attributes, or enqueuing records. Roots with a
zero sampling ratio need no random IDs. Partial sampling generates a trace ID
once per root using the SDK 0.30 ratio algorithm; descendants inherit the result.

Sampled pushes capture IDs, monotonic timestamps, buffer identity/size, thread
identity, a descriptor reference, and the flow result. Post hooks use a bounded
nonblocking queue. One worker enriches records into public `SpanData` batches,
sets the resource on the OTLP exporter, and serializes/exports them directly.
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
streaming thread, and metadata for live pads/buffers. OTLP encoding temporarily
allocates proportional to the bounded batch. Thread and pipeline counts remain
application-controlled. Overflow drops the new record; it never waits for the
receiver. A failed export counts the entire attempted batch as failed, with no
additional retry queue. A timeout can leave delivery uncertain, and exporter
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
snapshot, not an atomic transaction. Record loss can produce incomplete traces
and missing parents; sampling decisions do not change when telemetry is dropped.
`flush` reports export failures since the previous flush; cumulative counters
retain all failures, including ones already reported during pipeline stop.

## Correlation boundaries

- Shared buffers cannot safely accept mutable metadata. Branches retain the
  last common ancestor; exact immediate parentage across every branch is not
  promised. An initially shared buffer without our metadata uses a synchronized
  sidecar. Copying that buffer cannot copy the sidecar. A later asynchronous
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
```

The Linux benchmark needs a C compiler, GStreamer development files, and Python.
It measures a 10000-buffer, five-push-per-buffer pipeline against a local receiver.
It records pipeline wall time, source-thread CPU, process CPU including export
drain, exporter CPU at Linux scheduler-tick resolution, actual received spans,
and the tracer's counters. Logging is disabled. Healthy cases use a 65536-record
queue to compare full delivery; slow/absent cases deliberately use a queue of
128 and batches of 64. Results and logs go under `target/otel-bench`.
