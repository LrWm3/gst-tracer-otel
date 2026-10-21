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

Both items were implemented in this branch using direct `SpanData` export.
Section 5 subsequently replaces that conversion path with direct OTLP encoding;
there are no live SDK spans in the capture path.

See [implementation results](tracer/otel/bench/RESULTS.md) for delivered-span
benchmarks, failure accounting, correctness coverage, and the results of every
required check. [Tracer documentation](tracer/otel/README.md) records configuration,
resource and limit handling, flush/shutdown, counters, and propagation boundaries.
The existing workspace audit and Pyroscope dependency-check findings are recorded
there; Prometheus and independent Pyroscope changes remain outside this queue.


## 3. Follow-up efficiency improvements

Authorized by the user: "ok work through these".
Baseline for this pass: `ab5b76f` (compact capture v1).

- [x] Reject unsampled descendants before loading descriptive metadata.
- [x] Shard producer counters and submission bookkeeping; evaluate queue contention.
- [x] Drain worker records in chunks and preallocate replacement batches.
- [x] Consume SpanData during exporter conversion instead of cloning it.
- [x] Target metadata invalidation and use per-buffer fallback synchronization.

Preserve sampling/context, bounded memory, flush/shutdown, and exact quiescent
loss accounting. Compare healthy delivery at 1, 4, and 16 streaming threads;
exercise metadata churn, shared buffers, slow/failed receivers, and retirement.

Completed with 64 fixed bookkeeping shards, chunked worker draining, a narrow
version-pinned protobuf conversion patch, weak object-local cache subscriptions,
and stable per-buffer sidecars. The bounded shared queue remains; per-producer
queues require a separate design and are not justified by an isolated queue-cost
measurement in this pass. See [follow-up results](tracer/otel/bench/FOLLOWUP_RESULTS.md)
for all 213 measured runs, including timing variability, full-delivery checks,
failure accounting, correctness tests, and the existing workspace check failures.

## 4. Upgrade OpenTelemetry

Authorized by the user: "can you upgrade otel as well".

- [x] Upgrade the OTel API, SDK, OTLP, and protobuf crates from 0.30 to the latest
  published release, 0.33.1, and update the protobuf test decoder.
- [x] Remove the local conversion patch now that upstream consumes owned spans.
- [x] Preserve one export attempt per batch and encode remote-parent flags.
- [x] Validate collector delivery, sampling, log correlation, failure behavior,
  release performance, and required repository checks.

See [upgrade results](tracer/otel/bench/UPGRADE_RESULTS.md) for migration details,
86 measured runs, tests, and the unchanged workspace audit/dependency findings.
The controlled sampled comparison retained streaming performance but increased
total process CPU by about 8–15%; the upgrade is not claimed as a speedup.

## 5. Reduce export allocation and producer synchronization

Authorized by the user: "ok work through these things". Baseline: `21e0b7a`.

- [x] Measure a reference-counted HTTP body in isolation.
- [x] Compare batch sizes and blocking/async HTTP adapters; retain 512/blocking defaults.
- [x] Encode compact records directly, caching static protobuf attributes.
- [x] Evaluate batched producer submissions with bounded records and lifecycle tests.
  The prototype increased streaming CPU and was removed; retain individual submissions.
- [x] Specialize rejected capture and avoid redundant token rewrites.
- [x] Validate equivalent OTLP, sampling, failure accounting, lifecycle, and performance.

Completed with direct OTLP encoding, descriptor-owned encoded attributes, reused
worker buffers, reference-counted HTTP bodies, selectable blocking/async transport,
and separate compact rejected frames. Producer batching was tested and rejected
because it increased streaming CPU by 19–33% in the isolated comparison. Keeping
individual submissions preserves the previous flush and pending-record semantics.

See [direct export results](tracer/otel/bench/DIRECT_RESULTS.md) for 445 pipeline
runs, the isolated encoding benchmark, reproducible experiment patches, unchanged
workspace check failures, and the limits of these shared-machine measurements.

## 6. Reduce capture propagation cost

Authorized by the user: "ok continue iterating and give me the same metrics
before and after the change". Baseline: `22f2ecb`.

- [x] Measure rejected propagation and capture separately from pipeline cost.
- [x] Remove routine mini-object qdata lookups when trace metadata is present.
- [x] Preserve concurrent shared-root overrides, copied token ownership, and
  retirement of stale overrides when writable metadata is installed or copied.
- [x] Measure the GStreamer tracing floor using benchmark-only empty push hooks.
- [x] Complete paired disabled/0%/10%/100% comparisons, delivery and loss checks,
  and repository validation; record the same source and process CPU metrics.

Shared-root overrides now live alongside owned trace metadata, initialized only
when needed. Initially shared buffers without metadata retain the synchronized
qdata fallback. The identity-pipeline comparison reduces source CPU by 2–3% at
one stream and 6–9% at four/sixteen streams; tee/queue differences are smaller
and noisy. See [capture results](tracer/otel/bench/CAPTURE_RESULTS.md) for all 422
runs, the empty-hook diagnostic, and unchanged workspace check failures.

## 7. Reduce rejected-push overhead

Authorized by the user: "ok see if it can be improved". Baseline: `8e0f524`.

- [x] Evaluate sharing rejected context across compact nested frames; discard
  the prototype because pipeline results do not establish a consistent gain.
- [x] Resolve capture TLS once per begin hook while retaining short borrows for
  reentrant GStreamer callbacks and native log correlation.
- [x] Prototype GStreamer filtering of unobserved tracepoints before timestamps
  and hook-table lookups, preserving specific and wildcard callbacks.
- [x] Complete final paired measurements and validation, with plugin-only gains
  separated from gains requiring the optional GStreamer patch.

The GStreamer experiment uses two private 1.24.2 builds with the same tracer
plugin. The optional patch and its callback regression check are retained as
reviewable artifacts. See [rejected-path results](tracer/otel/bench/REJECTED_RESULTS.md).
The final plugin comparison lowers single-stream 0% source CPU by 4.6%. The
private core comparison lowers it by 25.6%, with the same plugin on both sides.
Core baseline and candidate share three threaded-test failures, documented with
the optional patch's validation limits. All 598 pipeline runs preserve delivery
or account for loss explicitly.
