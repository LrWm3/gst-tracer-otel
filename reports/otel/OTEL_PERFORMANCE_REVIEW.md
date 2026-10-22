# OpenTelemetry tracer performance review

Reviewed commit `c5c45815bc24b256b97bb7c24b0ea21e278da1ef` on 2026-10-20.

The baseline review used an independent local clone at
`/home/will/wk/3rd/gst-tracer-otel-perf-review`, initially on
`review/otel-performance`. The original checkout was left untouched. Baseline
source snapshots and binaries remain under `target/perf-review/`.

The queued implementation is now on `impl/otel-compact-capture`; see
[implementation results](https://github.com/LrWm3/gst-tracer-otel/blob/c5c45815bc24b256b97bb7c24b0ea21e278da1ef/tracer/otel/bench/RESULTS.md) and the
[implementation queue](https://github.com/LrWm3/gst-tracer-otel/blob/8b519d80eee10f9184b1f049a8194943af0ca677/IMPL_QUEUE.md). The measurements below describe the
original code, before that implementation.

The broken Pyroscope span correlation is the first cost to remove. The pinned
backend rebuilds its profiler on each thread-tag change, synchronously on the
streaming thread. This is much larger than the ordinary allocation overhead in
the tracer. The implementation also fixes metadata ownership and pad reference
leaks, then replaces live SDK spans with bounded capture and direct export.

## What was measured

Release builds, Rust 1.89.0, GStreamer 1.24.2, Intel i5-1345U, `GST_DEBUG=0`.
All pipeline runs used separate processes. A local HTTP receiver acknowledged
OTLP protobuf requests and counted spans. This receiver is a transport test sink,
not a production collector. The only experimental source change removed the
creation and registration of `PyroscopeSpanProcessor`; all OTel spans and
attributes remained enabled.

Fixture: `fakesrc ! identity ! identity ! identity ! identity ! fakesink`, with
`sync=false async=false` on the sink. The C harness timed PLAYING through EOS,
excluding tracer initialization and the export-drain wait. Three runs per case;
numbers below are medians. These are exploratory measurements on one machine,
not production latency guarantees or isolated CPU/allocation profiles.

| Mode | Buffers | Spans received in every run | Median pipeline time |
|---|---:|---:|---:|
| Original OTel tracer | 10 | 50 / 50 | 794.090 ms |
| OTel, Pyroscope processor removed | 10 | 50 / 50 | 0.710 ms |

Removing that processor reduced elapsed time by approximately **1,118x in this
small fixture**, while preserving the expected exported span count. This does
not establish equivalence of every trace attribute or parent relationship, and
it intentionally removes profile correlation.

The longer follow-up used a two-second drain outside the timed interval:

| Mode | Buffers | Spans received in every run | Median pipeline time |
|---|---:|---:|---:|
| No tracer | 10,000 | N/A | 15.022 ms |
| Noop tracer | 10,000 | N/A | 22.956 ms |
| OTel, Pyroscope processor removed | 10,000 | 50,000 / 50,000 | 175.630 ms |

Full OTel tracing still took approximately **11.7x the untraced pipeline time**
in this minimal workload. The incremental time over noop was about 3.05 us per
pad crossing, including concurrent exporter effects. That is a useful starting
point for the allocation/cache work, not a measurement of isolated hook cost.
The range across the three OTel runs was 143–249 ms, so finer claims require a
more controlled benchmark. No expensive media processing was present to amortize
tracing overhead.

The original 10,000-buffer pipeline exceeded a 30-second smoke-test timeout.
An interrupted debugger run located the streaming thread in this stack:

```text
pad-push-post
  SpanRef::end_with_timestamp
    PyroscopeSpanProcessor::on_end
      PyroscopeAgent::remove_thread_tag
        Pprof::remove_rule
          Pprof::dump_report
            Pprof::reset
              ProfilerGuardBuilder::build
                Backtrace::new / symbol resolution / decompression
```

The video conversion fixture
`videotestsrc num-buffers=3 ! video/x-raw,format=I420 ! videoconvert ! video/x-raw,format=RGB ! fakesink`
crashed. A debugger run stopped with SIGBUS in `gst_spanbuf_free`; a direct run
returned SIGSEGV. This agrees with the metadata ownership defect below.

The harness also confirmed the pad reference leak: after NULL, the final sink
pad had **2 references without tracing, 12 after 10 traced buffers, and 10,002
after 10,000 traced buffers**. Removing Pyroscope does not fix that leak.

## Findings and proposed changes, in order

### 1. Remove profiler report generation from span start/end

Evidence: [initialization](https://github.com/LrWm3/gst-tracer-otel/blob/c5c45815bc24b256b97bb7c24b0ea21e278da1ef/tracer/otel/src/oteltracer.rs#L59),
[processor callbacks](https://github.com/LrWm3/gst-tracer-otel/blob/c5c45815bc24b256b97bb7c24b0ea21e278da1ef/tracer/otel/src/pyroscopespanprocessor.rs#L109), and the
debugger stack above. The lockfile resolves `pyroscope_pprofrs` to 0.2.10.
Its `add_rule` and `remove_rule` both call `dump_report` when profiling is active;
`dump_report` builds and processes the report and then calls `reset`, which
rebuilds the profiler. Both operations run through the root-span callbacks.

There are three separate costs:

- Report generation, profiler restart, and backtrace symbol resolution happen
  on the streaming thread.
- Root callbacks take an exclusive lock on the shared agent, serializing those
  operations across streams.
- OTel SDK 0.30.0 uses a single-processor move path, but clones its internal span
  data for **each processor** when more than one is registered. Registering
  Pyroscope plus the batch exporter therefore copies span payloads twice at end,
  including for child spans where Pyroscope immediately does nothing.

Make profiling and profile correlation explicit options; default OTel tracing
to one batch processor. Independent continuous profiling can remain available
through the separate Pyroscope tracer. If per-span correlation is required,
use a backend that can read cheap thread-local correlation state during sampling,
or redesign that integration after measuring it. Merely replacing `RwLock` with
a faster lock, or lowering the profiler sample frequency, does not remove the
report/reset operation per tag change. Moving those tag changes to a worker
would also change which execution interval gets attributed to each span.

The SDK's [SpanProcessor contract](https://docs.rs/opentelemetry_sdk/latest/opentelemetry_sdk/trace/trait.SpanProcessor.html)
documents synchronous callbacks; the same behavior was verified in the locally
downloaded, pinned 0.30.0 source.

### 2. Fix ownership before treating the tracer as a benchmark baseline

**Metadata copy double ownership.**
[The transform callback](https://github.com/LrWm3/gst-tracer-otel/blob/c5c45815bc24b256b97bb7c24b0ea21e278da1ef/tracer/otel/src/oteltracer.rs#L167) copies the same
`SpanContext` pointer into a second meta, while
[the free callback](https://github.com/LrWm3/gst-tracer-otel/blob/c5c45815bc24b256b97bb7c24b0ea21e278da1ef/tracer/otel/src/oteltracer.rs#L161) reconstructs and drops a
`Box` for every meta. This allows use-after-free and double free. Transform also
passes null initialization parameters; the init debug statement dereferences
that pointer if trace logging is enabled. Give each meta its own owned context,
or explicitly manage shared ownership. An inline context/compact carrier with
proper init/copy/drop callbacks can avoid the extra `Box` later.

**Unchecked metadata mutation.**
[`add_ptr`](https://github.com/LrWm3/gst-tracer-otel/blob/c5c45815bc24b256b97bb7c24b0ea21e278da1ef/tracer/otel/src/oteltracer.rs#L122) bypasses buffer writability and
ignores attachment failure, leaking its newly allocated context on failure.
GStreamer requires writable buffers for metadata mutation, and
[`gst_buffer_add_meta`](https://gstreamer.freedesktop.org/documentation/gstreamer/gstbuffer.html#gst_buffer_add_meta)
can return null. A tracer hook cannot transparently replace the caller's buffer
with a writable copy. Define safe behavior for shared buffers and an explicit
propagation fallback; copying video buffers to make tracing work is not a good
performance fix.

**One peer-pad reference leaked per push.**
[The post hook](https://github.com/LrWm3/gst-tracer-otel/blob/c5c45815bc24b256b97bb7c24b0ea21e278da1ef/tracer/otel/src/oteltracer.rs#L287) calls `gst_pad_get_peer`, wraps
the returned owned reference as borrowed, and never releases it. It also assumes
the peer is non-null. Use the safe owned API, or `from_glib_full` with null
handling. Better still, match post to its pre record without rediscovering a
possibly changed peer. The [GStreamer API](https://gstreamer.freedesktop.org/documentation/gstreamer/gstpad.html#gst_pad_get_peer)
specifies ownership and nullability.

**Active-state lifetime and thread ownership.**
[`GstSpanSink`](https://github.com/LrWm3/gst-tracer-otel/blob/c5c45815bc24b256b97bb7c24b0ea21e278da1ef/tracer/otel/src/oteltracer.rs#L50) stores a borrowed `SpanRef`
and a thread-affine `ContextGuard` in pad qdata through erased pointers.
[Construction](https://github.com/LrWm3/gst-tracer-otel/blob/c5c45815bc24b256b97bb7c24b0ea21e278da1ef/tracer/otel/src/oteltracer.rs#L588) bypasses the borrow checker's
normal lifetime guarantees. Normal execution relies on the TLS context still
owning the span; teardown and exceptional paths are difficult to reason about.
Keep owned active state in a per-thread stack, with guards restored on the same
thread. Keep immutable pad metadata separately. Match records by pad identity
and invocation nesting, and always clean them up on error or non-recording paths.

### 3. Cache the tracer and metadata outside the buffer path

[Every new push span](https://github.com/LrWm3/gst-tracer-otel/blob/c5c45815bc24b256b97bb7c24b0ea21e278da1ef/tracer/otel/src/oteltracer.rs#L458) calls `init_otlp`.
That function initializes a `OnceLock`, then ignores the cached tracer and calls
`global::tracer` again. In the pinned SDK this acquires a global read lock,
constructs a scope/tracer, and boxes the tracer. Retain a concrete `SdkTracer`
once, owned with its provider. Avoid replacing the host application's global
provider just to implement this plugin.

[Name construction](https://github.com/LrWm3/gst-tracer-otel/blob/c5c45815bc24b256b97bb7c24b0ea21e278da1ef/tracer/otel/src/oteltracer.rs#L459) and
[attribute construction](https://github.com/LrWm3/gst-tracer-otel/blob/c5c45815bc24b256b97bb7c24b0ea21e278da1ef/tracer/otel/src/oteltracer.rs#L541) repeatedly walk
parents, reference objects, copy names, format thread IDs, and allocate vectors.
The post hook repeats thread formatting and attribute allocation.

Cache a stable pad-pair ID, element/pad names, static attributes, and trace-name
information during setup or on a first-use slow path. Cache thread attributes in
TLS. OTel 0.30 supports `Arc<str>` attribute values, so cloning cached attributes
need not clone each string. A fixed span name such as `gst.pad.push` plus cached
attributes also avoids allocating a dynamic name per invocation. Fill the SDK's
required attribute storage once, rather than building intermediate vectors and
then copying strings again.

Use the Prometheus tracer's cache as a starting point, but account for its
documented ghost-pad and incomplete-path limitations. Invalidate on unlink,
relink, rename/reparent, and object destruction. Cache lifetime must remain safe
during active pushes; a raw pointer plus a destruction hook is insufficient if
another thread can still read it.

### 4. Sample once per buffer trace, before expensive work

The [sampler](https://github.com/LrWm3/gst-tracer-otel/blob/c5c45815bc24b256b97bb7c24b0ea21e278da1ef/tracer/otel/src/oteltracer.rs#L72) is explicitly hardcoded to 100%.
Changing environment sampling settings alone will not override this builder
choice. Expose parent-based root sampling and an element/pad inclusion filter.
Make the rejection path happen before span names, attributes, SDK spans, and
Pyroscope work are constructed.

The sampling decision must follow the buffer through queues and branches.
Currently [context propagation](https://github.com/LrWm3/gst-tracer-otel/blob/c5c45815bc24b256b97bb7c24b0ea21e278da1ef/tracer/otel/src/oteltracer.rs#L541) is inside
`is_recording()`. Simply reducing the SDK ratio therefore risks downstream pads
and queue threads starting new roots and sampling again. Propagate a valid
unsampled context or a distinct rejection marker as well as sampled context;
do not confuse "not sampled" with "no context yet." Never resample each pad
independently if the result is meant to describe a coherent trace.

For routine monitoring, keep cheap aggregate latency metrics and sample detailed
traces. For full tracing, expose a deliberate 100% mode. Choose the production
ratio from measured span volume and the acceptable diagnostic coverage, rather
than silently changing behavior to meet a speed target.

### 5. Simplify active state and timestamps

Replace the per-push qdata allocation/mutation with a reusable TLS stack for
synchronous pre/post pairs. Only cross-thread propagation should need a
buffer-associated carrier. Store either an owned SDK span, or an owned Context
when active-context logging is required. Avoid retaining a borrowed `SpanRef`
in foreign object storage. The current extra parent `ctx.attach()` followed by
another child attachment can also be simplified.

Define what is being timed: pad-push wall duration includes nested downstream
work; it is not automatically exclusive element processing time, queue residence,
or end-to-end buffer latency. Use hook timestamps as the measurement source.
Map their monotonic clock domain to wall-clock timestamps with a calibrated
anchor, preserving monotonic durations, and pass explicit start/end timestamps
to OTel. The current `ts.start`/`ts.end` attributes do not control OTel's actual
span timestamps. Cover queue waits and exclusive processing separately if those
are desired measurements.

### 6. Keep exporting bounded; only add another worker if needed

[`with_batch_exporter`](https://github.com/LrWm3/gst-tracer-otel/blob/c5c45815bc24b256b97bb7c24b0ea21e278da1ef/tracer/otel/src/oteltracer.rs#L81) already sends spans to
a background worker. Network serialization is not the first thing to move.
Tune queue/batch sizes after fixing the producer path, track generated, exported,
and dropped counts, and explicitly own provider flush/shutdown. OTel 0.30's
default batch queue is 2,048 spans and enqueue is nonblocking; a faster producer
can appear healthy while losing traces. Queue capacity cannot fix sustained
overload.

If full tracing is still too expensive after the preceding changes, benchmark a
second architecture: hooks emit compact completed-span records with cached IDs,
trace/parent/span IDs, timestamps, and a few numeric values into bounded
per-thread queues. Workers build SDK spans using explicit IDs and timestamps.
Keep immediate thread-local correlation and buffer propagation in the hooks so
logs and downstream work have context before the worker runs. Preserve sampling,
trace state, ordering, and overflow accounting. This is a larger implementation
with its own CPU, memory, and completeness costs; it should follow evidence that
SDK construction remains a bottleneck.

Logging should be optional and filtered early too.
[The current bridge](https://github.com/LrWm3/gst-tracer-otel/blob/c5c45815bc24b256b97bb7c24b0ea21e278da1ef/tracer/otel/src/otellogbridge.rs#L78) duplicates the message
string and formats/copies IDs as attributes. Use native trace/span fields and
cached thread metadata where possible. The shipped OTel tests enable verbose
logging, so they should not be treated as performance benchmarks.

## Baseline recommendations

The agreed implementation scope and acceptance checks are recorded in
[IMPL_QUEUE.md](https://github.com/LrWm3/gst-tracer-otel/blob/8b519d80eee10f9184b1f049a8194943af0ca677/IMPL_QUEUE.md). These were the broader baseline recommendations.

1. Fix metadata ownership, attachment failure, pad references, and active-state
   ownership. Add regressions for buffer copies/pools, non-writable buffers,
   unlinked pads, errors, queues, and teardown. Assert exported parent IDs and
   timestamps, not merely pipeline completion.
2. Remove the broken Pyroscope correlation integration. Keep the minimal
   reproducer for its report/reset cost; a correct replacement needs a design.
3. Cache the concrete tracer, pad metadata, and thread metadata. Replace dynamic
   names and intermediate allocation work. Measure this independently at 100%
   sampling to distinguish implementation speed from reduced telemetry.
4. Add coherent sampling and test 0%, partial, and 100% across queues, tee,
   buffer copies, bins, and reused buffers. Add explicit bounded export
   configuration and flush/shutdown.
5. Profile the remaining full-tracing path and only then decide whether compact
   records plus workers justify the complexity.

Benchmark no tracer, noop, Prometheus, original OTel where safe, and each OTel
change. Include long chains, multiple source threads, queues, tee, ghost pads,
buffer copying, and dynamic relinking. Run healthy, slow, and absent receivers.
Report CPU time, ns per buffer/pad crossing, allocation counts, p50/p99 pipeline
latency, RSS growth, throughput, span counts, and drops. Separate startup and
drain from steady state. Require flat pad reference counts and stable long-run
memory before accepting a speed improvement. There is no justified universal
"less than 1% overhead" claim from the measurements here.

## Baseline reproduction artifacts and validation

These checks describe the original review; current checks are recorded in the
[implementation results](RESULTS.md).

Local artifacts in `target/perf-review/` include `pipeline.c`, `compare.py`,
`comparison.json`, `no-pyroscope.patch`, original/experimental build logs, smoke
logs, and the two debugger backtraces. `ablation-src` is a snapshot with the
three-line processor removal; `ablation-target` holds its build. The unchanged
baseline binary is preserved in `target/perf-review/baseline`.

The comparison script uses a private local HTTP receiver, debug logging off,
`OTEL_BSP_SCHEDULE_DELAY=10`, and `OTEL_BSP_MAX_QUEUE_SIZE=65536`. The first
10,000-buffer attempt waited only 300 ms after EOS and sometimes exited before
all spans arrived; those incomplete runs are retained in `comparison.json` and
must not be interpreted as loss-free end-to-end results. The longer follow-up
is recorded separately in `comparison-large.json`.

- `cargo build --release --locked -p gst-otel-tracer -p gst-noop-tracer`: passed.
- `cargo fmt --all`: passed; no tracked source diff.
- `cargo clippy --workspace --all-features -- -D warnings`: passed on unchanged
  production source.
- `cargo test`: passed, five integration tests, after building the Prometheus
  plugin with `cargo build -p gst-prometheus-tracer`. The initial fresh-clone
  attempt failed because the tests require a discoverable plugin binary.
- `cargo audit`: attempted, unavailable because `cargo-audit` is not installed.
- `cargo +nightly udeps -p gst-otel-tracer -p gst-prometheus-tracer -p
  gst-pyroscope-tracer --all-targets`: attempted, unavailable because
  `cargo-udeps` is not installed.
- `git diff --check`: passed.

The workspace's default member is Prometheus: bare `cargo test` does **not**
validate the OTel tracer. The OTel findings above came from explicit release
plugin runs and debugger inspection, not a claimed passing OTel suite. Existing
[OTel tests](https://github.com/LrWm3/gst-tracer-otel/blob/c5c45815bc24b256b97bb7c24b0ea21e278da1ef/tracer/otel/tests/oteltracer.rs#L33) assert registration and run
pipelines without checking received traces; bus errors break the loop without
failing, and the bin test is empty. Replace those gaps with assertions before
relying on CI to protect this work.

Performance-counter sampling was not run: this host has
`kernel.perf_event_paranoid=4`. Debugger stacks established the synchronous
profiler work; allocation and CPU attribution still need dedicated profiling.
