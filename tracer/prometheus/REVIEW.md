# Latency tracer Rust review

Reviewed the Rust introduced by `19a07e4` on `task/latency-tracer-v2`: the
Prometheus implementation and its integration tests, including the surrounding
tracer wrapper and plugin registration needed to assess ownership and hooks.
This is not a review of the processor's Rust/CUDA implementation.

## Fixes made

- **Floating RTSP pad lifetime (found during follow-up benchmarking):** the
  ownership cleanup initially used `from_glib_none` for incoming raw pads. That
  conversion sinks floating GObjects, so dropping the wrapper could free a
  newly created RTSP pad still owned by C. Raw pads are now borrowed, with a
  normal reference-counted clone only when traversal needs ownership. Added a
  regression checking both floating state and reference count are unchanged.
- **DeepStream construction crash (found during follow-up benchmarking):**
  the element-new hook called `element.factory()` before construction finished.
  On an integrated memory nvidia device this crashed in `g_object_ref_sink` while reading the factory. Queue
  identification now checks the already-valid GType instead. Both pre-review
  and reviewed implementations required this guard for processor benchmarking.
- **Process abort with queue2:** reproduced with `fakesrc ! queue2 ! fakesink`.
  `queue2` exposes queue levels but not `queue`'s overrun/underrun signals.
  Signal existence is now checked before connecting. Added an EOS regression.
- **Cache lifetime during unlink:** streaming callbacks previously borrowed a
  raw qdata pointer that unlink could free. GLib's locked qdata duplication now
  acquires an `Arc`, and each in-flight span retains its cache until post-push.
  Post-push no longer depends on the pad still having qdata. Added a regression
  that clears qdata while a span exists and verifies final ownership release.
- **Leaked GObject references:** parent/proxy resolution mixed owning FFI
  returns with additional borrowed-wrapper references. It now uses owned Rust
  wrappers throughout, with a bounded walk rather than unbounded recursion.
  The ownership regression also checks source/sink reference counts.
- **Streaming-thread queue work:** signal handlers no longer create labels and
  perform Prometheus lookups. They increment atomics; the sampler exports them.
  Queue labels are deferred until PAUSED rather than captured during assembly.
- **Timing attribution:** optional PTS diagnostics run outside the mutable span
  borrow and before the measurement starts. Wall measurements use one monotonic
  clock at both boundaries; failed CPU clock reads suppress CPU/blocked samples
  and invalidate affected parent CPU attribution. Added a missing-clock test.
- **Span cleanup:** post hooks look for a recorded matching span, allowing the
  mismatch handler to discard an unbalanced stack rather than silently leave it.
- **Hook declarations:** push-post and pull callbacks now declare their full
  callback arguments, including unused flow results, offset and length.
- **Disabled-feature overhead:** PTS histogram series are no longer allocated
  when PTS age is disabled.
- **Documentation:** corrected the earlier unmatched overhead comparison,
  documented histogram-name migration and queue sampling limitations.

## Readability recommendations, in priority order

1. **Split responsibilities, not individual functions arbitrarily.** Move pure
   span accounting to `spans.rs`, metric declarations/observation to `metrics.rs`,
   queue registration/sampling to `queues.rs`, and qdata ownership to a small
   `pad_cache.rs`. Keep the GStreamer adapter thin. A reader should be able to
   understand exclusive timing without reading HTTP or unsafe FFI code.
2. **Prefer typed tracer hooks.** The wrapper already uses `TracerImpl` for
   element creation; use its typed push/link hooks too. This removes manual
   `transmute` registrations and makes callback-signature mistakes compiler
   errors. Benchmark the conversion before assuming its overhead is identical.
3. **Make ownership and accounting explicit in types.** Replace the raw cache
   builder's sentinel with `Option<Arc<PadCacheData>>`. Rename
   `get_real_pad_parent_ffi` to `resolve_pad_parent`. Let a `SpanStack::finish`
   operation return metrics ownership and timing together, avoiding repeated TLS
   borrows and the additional `Arc` clone in post-push. Keep any unsafe qdata
   conversions inside that module with stated lifetime invariants.
4. **Centralize configuration and units.** Parse one `TracerConfig` with named
   defaults and validation rather than scattered environment-backed globals.
   Accept it explicitly in sampling helpers so tests do not silently skip when
   an environment variable changes. Use a named nanoseconds-to-seconds helper
   and `PadMetrics::observe` to separate accounting from export details.
5. **Make tests describe contracts.** Use a shared bounded-EOS pipeline harness
   that fails on bus errors and always returns to NULL. Older tests can currently
   break out on an error and still pass metric checks. Match complete label sets,
   not any matching count from a previous pipeline. Replace loose timing
   tolerances and the hard one-second microbenchmark threshold with deterministic
   clock/accounting tests plus separately reported performance benchmarks.
   Run configuration variants in subprocesses because GStreamer and environment
   settings are initialized once per process.

These are deliberately follow-up suggestions, not a large module rewrite mixed
into the correctness fixes.

## Remaining limitations and qualification

- Synchronous push duration is not asynchronous GPU/VIC completion time or
  per-frame end-to-end latency. `wall - thread CPU` includes scheduler delay and
  backpressure; it cannot identify their individual causes. Sequential clock
  reads and nested tracer work still distort very short spans.
- Pad metrics retain the existing link-time naming behavior. Complex ghost-pad
  assembly, late bin insertion, and reparenting can produce incomplete or stale
  paths. The ordinary ghost-pad tests pass; arbitrary dynamic topology is not
  qualified. A state-aware cache refresh needs its own topology tests.
- Prometheus label series live for the process lifetime. Repeated creation of
  uniquely named graphs increases cardinality even after GObjects are released.
  That is distinct from the fixed GObject-reference leak. Define a retirement
  policy before using this in a process that continually rebuilds graphs.
- Queue gauges are samples, not peak/residence measurements. Queues created and
  destroyed between sampler ticks may never appear; pending signal counts at
  teardown are not a reliable lifetime event total. The sampler holds its
  registration lock while reading properties; snapshotting registrations outside
  that lock would make future dynamic-graph behavior easier to reason about.
- Pull latency is still unimplemented. Process-global sampler, registry and HTTP
  server assume a single tracer configuration and lack an explicit shutdown
  lifecycle. Do not treat multiple tracer instances as independent collectors.
- The former histogram name was not retained by the original changes. Existing
  gauge/sum/count queries remain supported; migrate histogram queries as noted
  in the README.
- No revised-build overhead on an integrated memory nvidia device or long-soak qualification has been run.
  Compare tracer-off/baseline/revised using identical configuration, then measure
  FPS, CPU, memory/cardinality and tail latency with actual scraping enabled.

## Validation

- `cargo fmt --all`: passed.
- `cargo clippy --workspace --all-features --all-targets -- -D warnings`: passed.
- `cargo build -p gst-prometheus-tracer`, then `cargo test -- --test-threads=1`
  with `GST_PLUGIN_PATH` pointing at this worktree's `target/debug`: passed,
  ten unit tests and eight integration tests. The workspace default member
  is the Prometheus tracer; this is not a claim that every workspace test ran.
- `cargo audit` and `cargo +nightly udeps ...`: attempted but unavailable;
  the corresponding Cargo extensions are not installed.

Tests used Rust 1.89.0 through the repository's mise executable. Changes remain
local to this tracer branch. Follow-up benchmarks stage isolated release plugins
on an integrated memory nvidia device; they do not replace its installed tracer.
