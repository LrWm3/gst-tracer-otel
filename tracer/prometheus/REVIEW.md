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
3. **Keep ownership boundaries explicit.** The follow-up now returns owned
   optional caches from the builder and moves metrics ownership and timing out
   of span completion together. Qdata replacement documents its ownership
   contract, and a bounded thread cache avoids steady-state qdata locking.
   Preserve those invariants if these responsibilities move into separate modules.
4. **Centralize configuration and units.** Parse one `TracerConfig` with named
   defaults and validation rather than scattered environment-backed globals.
   Accept it explicitly in sampling helpers so tests do not silently skip when
   an environment variable changes. Use a named nanoseconds-to-seconds helper
   and `PadMetrics::observe` to separate accounting from export details.
5. **Keep tests focused on contracts.** The follow-up adds a bounded-EOS harness,
   NULL-state cleanup, complete label/count assertions, useful latency bounds,
   ownership checks and separately reported performance measurements. Keep
   configuration variants in subprocesses because GStreamer and environment
   settings are initialized once per process.

These are deliberately follow-up suggestions, not a large module rewrite mixed
into the correctness fixes.

## Remaining limitations and qualification

- Synchronous push duration is not asynchronous GPU/VIC completion time or
  per-frame end-to-end latency. `wall - thread CPU` includes scheduler delay and
  backpressure; it cannot identify their individual causes. Sequential clock
  reads and nested tracer work still distort very short spans.
- Follow-up fixes resolve pad labels on first push and refresh them after link,
  bin-parenting and activation changes. Tests cover late bin insertion,
  reparenting and real-pad labels through ghost forwarding. In-flight spans keep
  their original cache snapshot across changes. Arbitrary concurrent topology
  mutation is not fully qualified; live root renaming now has regression coverage.
- Series now retire on scrape or periodic maintenance when their pads/elements
  disappear or change path. Live graphs with many distinct pads still create
  many histogram series; use element selection to limit instrumentation.
- Queue gauges are samples, not peak/residence measurements. Queues created and
  destroyed between sampler ticks may never appear; pending signal counts at
  teardown are not a reliable lifetime event total. The sampler now reads
  properties outside the registration lock and refreshes labels after moves.
- Pull timing now measures exclusive upstream synchronous work, with separate
  histograms and call counts. Process-global sampler, registry and HTTP server
  still assume one tracer configuration and lack an explicit shutdown lifecycle.
  Do not treat multiple tracer instances as independent collectors.
- The former histogram name was not retained by the original changes. Existing
  gauge/sum/count queries remain supported; migrate histogram queries as noted
  in the README.
- [Matched overhead checks on an integrated memory nvidia device](BENCHMARK.md) now cover synthetic pushes,
  processor capacity and two-stream RTSP with actual scraping. No material
  throughput regression appeared in the short processor checks. Long-soak,
  memory growth and rare-tail qualification remain outstanding; an off-run
  shutdown SIGSEGV is documented separately rather than counted as a clean pass.

## Original validation

- `cargo fmt --all`: passed.
- `cargo clippy --workspace --all-features --all-targets -- -D warnings`: passed.
- `cargo build -p gst-prometheus-tracer`, then `cargo test -- --test-threads=1`
  with `GST_PLUGIN_PATH` pointing at this worktree's `target/debug`: passed,
  ten unit tests and eight integration tests. The workspace default member
  is the Prometheus tracer; this is not a claim that every workspace test ran.
- `cargo audit` and `cargo +nightly udeps ...`: attempted but unavailable;
  the corresponding Cargo extensions are not installed.

These results predate the later fixes. Current validation and matched performance
results are recorded in [BENCHMARK.md](BENCHMARK.md).
