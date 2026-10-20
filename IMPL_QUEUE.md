# Implementation queue

Status: completed on branch `impl/otel-compact-capture`.

Start authorized by the user: "add this to the impl queue then work through the
impl queue on a branch."

Working checkout: `/home/will/wk/3rd/gst-tracer-otel-perf-review`

Review baseline: `c5c45815bc24b256b97bb7c24b0ea21e278da1ef`

## 1. Remove the broken Pyroscope span-correlation integration

- [x] Completed.

The existing correlation implementation is incorrect, not merely an expensive
implementation of the intended design. Its profiler report/reset work must be
removed from the OTel span start/end path before establishing a useful tracing
performance baseline.

Scope:

- Remove automatic Pyroscope agent creation and registration of the existing
  `PyroscopeSpanProcessor` from OTel initialization.
- Remove obsolete correlation code, OTel-only dependencies, and misleading
  configuration/documentation as appropriate after checking their uses.
- Preserve the independent Pyroscope tracer and normal OTel trace export.
- Treat a correct replacement for profile correlation as separate future work
  requiring a design; do not present the current implementation as supported
  simply by making it opt-in.
- Keep Prometheus work outside this task; it is being handled elsewhere.

Acceptance:

- Ordinary OTel tracing never starts the profiler or generates profiler reports
  from span callbacks.
- Explicit OTel pipeline checks verify expected exported span counts and
  parent/child relationships, including a queue boundary.
- Repeat the tracing benchmark with a healthy receiver, logging disabled, and
  an export drain outside the timed interval; report delivered and dropped spans
  alongside timing.
- Run the repository's required checks and report unavailable tooling or
  pre-existing failures accurately. Bare `cargo test` covers the default
  Prometheus member, so it is insufficient as OTel validation.

Supporting review: [OTel performance review](OTEL_PERFORMANCE_REVIEW.md).

## 2. Build the tracing path around compact capture and bounded processing

- [x] Completed after removal of the broken Pyroscope correlation integration.

Agreed direction:

- Cache descriptive metadata once.
- Make the unsampled path extremely cheap while preserving the sampling decision.
- Keep sampled hook work to compact identity, timing, and correlation records.
- Move enrichment and serialization off streaming threads.
- Bound pending data and account for dropped records explicitly.

Implementation requirements:

- Cache metadata for its valid lifetime, with safe invalidation when pipeline
  topology or descriptions change.
- Preserve sampled and unsampled decisions across thread and buffer boundaries;
  downstream stages must not mistake a rejected trace for an unobserved root.
- Keep dynamic formatting, descriptive attribute construction, and export work
  out of the common streaming hook path.
- Define the OTel integration boundary after reviewing the pinned API/SDK:
  compact records must retain enough information to preserve identities,
  relationships, timestamps, and trace state during later enrichment/export.
- Use bounded, nonblocking handoff and account for incomplete traces, dropped
  records, and pending data. Avoid presenting dropped telemetry as a speed gain.
- Validate sampling at 0%, partial, and 100%, and benchmark healthy, slow, and
  absent receivers. Measure streaming-thread cost separately from worker cost.
- Keep this scoped to OTel tracing; Prometheus changes remain separate.

Selected OTel integration: construct public `SpanData` batches on the worker and
call the OTLP exporter directly, without live SDK spans or a second SDK batch
queue. Own sampling, span limits, resource setup, flush/shutdown, and failure
accounting explicitly. Preserve OTel IDs, timestamps, parentage, and trace state.


## Completion evidence

Both items are implemented in this branch. The selected direct-`SpanData`
export design is in use; there are no live SDK spans in the capture path.

See [implementation results](tracer/otel/bench/RESULTS.md) for delivered-span
benchmarks, failure accounting, correctness coverage, and the results of every
required check. [Tracer documentation](tracer/otel/README.md) records configuration,
resource and limit handling, flush/shutdown, counters, and propagation boundaries.
The existing workspace audit and Pyroscope dependency-check findings are recorded
there; Prometheus and independent Pyroscope changes remain outside this queue.
