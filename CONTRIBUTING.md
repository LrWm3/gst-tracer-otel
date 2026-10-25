# Contributing

For tracer selection and a first run, see the [README](README.md). Run the
commands below from the repository root.

## Environment

Install a stable Rust toolchain, a C compiler, `pkg-config`, GStreamer libraries
and development headers, GLib development headers, GStreamer command-line tools
and base plugins, and `curl` for the quickstart. The
[development container](.devcontainer/devcontainer.json) provides a configured
environment using Docker and VS Code's Dev Containers extension.

For Ubuntu 24.04, the repository also provides setup helpers. Review them before
running: the system helper installs development packages and configures Ubuntu
debug-symbol repositories; the user helper installs Rust toolchains and
development tools, with a prompt for additional Cargo tools.

```sh
sudo bash .devcontainer/setup-root.sh
bash .devcontainer/setup-user.sh
```

The system helper skips package installation if `gst-launch-1.0` is already
present, so ensure development headers are installed too. Adapt the helpers for
other distributions.

The `just` task runner is optional. Run `just --list` to see the shortcuts in the
[justfile](justfile); the Cargo commands below work without it.

## Build and test

The workspace defaults to `gst-prometheus-tracer`. Build OTel explicitly, and
enable `profiles` to include the embedded sampler and `mars-profiles` entry point:

```sh
cargo build --release -p gst-prometheus-tracer
cargo build --release -p gst-otel-tracer --features profiles
```

Before submitting changes, run the checks required by [AGENTS.md](AGENTS.md),
plus the OTel tests when changing that plugin:

```sh
cargo fmt --all
cargo clippy --workspace --all-features -- -D warnings
cargo test
cargo test -p gst-otel-tracer
cargo test -p gst-otel-tracer --features profiles
cargo test -p gst-mars-metrics-core
cargo audit
cargo +nightly udeps -p gst-otel-tracer -p gst-prometheus-tracer -p gst-mars-metrics-core --all-targets --all-features
```

`cargo test` alone covers the default Prometheus member. The explicit OTel
commands exercise builds with and without profiling; feature-enabled integration
tests check metrics, traces, logs, and profile correlation. Profiler tests require
Linux x86-64 or AArch64. `cargo audit` and `cargo udeps` require the corresponding Cargo
tools; `udeps` also requires a nightly toolchain.

The native caller-chain test additionally requires frame pointers and is run
explicitly by [ARM CI](.github/workflows/profiles-arm64.yaml):

```sh
RUSTFLAGS='-C force-frame-pointers=yes' cargo test -p gst-otel-tracer \
  --features profiles --test profiles \
  native_frame_chain_retains_known_callers_under_an_element_root -- --ignored --nocapture
```

For the Prometheus address-sanitizer checks used in CI:

```sh
just test-address-sanitizer
```

Keep changes focused, add tests for behavior changes, and update the relevant
user guide. Use commit messages of the form `<type>(<component>): <summary>`,
where `type` is `doc`, `impl`, `fix`, or `test`. Reference relevant issues in pull
requests.

## Benchmarks and implementation notes

Store performance reviews, measurements, and supporting data under
[reports](reports/README.md). Reports include reproduction commands; the
benchmark harnesses live alongside the tracer implementations. Build in release
mode for performance comparisons.

- [Metrics implementation and semantics](tracer/prometheus/README.md)
- [OTel capture, export, and validation](tracer/otel/README.md)
- [Combined instrumentation and correlation](tracer/otel/INSTRUMENT.md)
- [Embedded CPU profiler](tracer/otel/PROFILES.md)
- [Trace/profile integration investigation](TRACE_PROFILE_INTEGRATION.md)

[mars-noop](tracer/noop/README.md) is a hook-testing plugin and basic template,
built from `gst-noop-tracer`. It is not a telemetry collector.

## Remaining work

Unchecked items are planned changes, not currently supported options. Prioritize delivery
correctness, lifecycle handling, and instrumentation health before export and
profiling optimizations.

### Delivery and lifecycle

- [x] Decode OTLP trace export responses. HTTP 200 can include rejected spans;
  count accepted and rejected records accurately, surface warning messages, and
  reject malformed responses. Do not retry partially accepted batches, as
  required by the [OTLP specification](https://opentelemetry.io/docs/specs/otlp/#partial-success-1).
  Cover full acceptance, partial/all rejection, warning-only responses, malformed
  bodies, and transport failures with receiver tests.
- [x] Apply a shared deadline to trace, metric, and log flushes, and to shutdown
  of all signals including profiles. Log export now uses a bounded worker and
  reports failures. `flush_report` and `shutdown_report` retain per-signal
  results; tests cover slow/failing receivers, repeated shutdown, and concurrent
  callers.
- [x] Provide a final profile drain accessible to C/Python applications as well
  as Rust. Distinguish local queue drain from confirmed backend delivery, and
  preserve other active pipelines when one pipeline transitions to NULL.

### Instrumentation health

- [x] Add one enum for instrumentation monitoring: `none`, `core`, or `all`,
  with `core` as the default. Keep this separate from selecting which application
  signals to collect.

  | Level | Behavior |
  | --- | --- |
  | `none` | Disable self-monitoring collection/export beyond bookkeeping required for correctness and existing explicit status APIs. |
  | `core` | Export inexpensive, bounded health counters and gauges: accepted/rejected/dropped records, export failures, pending work, profile sample losses, attribution exhaustion, and cleanup failures/timeouts. |
  | `all` | Include `core` plus detailed diagnostics such as export duration histograms, batch sizes, bytes sent, queue high-water marks, symbol-cache activity, and worker processing costs. |

  Use the configured metrics exporter when metrics are enabled; do not silently
  enable an additional signal, listener, or endpoint. Keep labels bounded by
  component, signal, and fixed failure reason, without element paths, trace IDs,
  or error-message labels. Keep timing and allocation-heavy diagnostics out of
  `core`, avoid blocking streaming threads, and prevent monitoring its own export
  from generating a telemetry feedback loop. Add a health dashboard and alerts
  for loss, backlog, and exhausted capacity. Test enum validation, defaults,
  export routing, and all three levels; measure their overhead separately.
  See the [configuration and health dashboard](tracer/otel/INSTRUMENT.md#monitor-the-instrumentation)
  and [measured comparison](reports/otel/INSTRUMENT_VIDEO_RESULTS.md#instrumentation-monitoring--2026-10-25).

### Readable spans and the correlation demo

- [x] Include the element factory in span names by default, for example
  `gst.pad.push videoconvert` and `gst.pad.pull_range filesrc`. Use the callee:
  the downstream receiving element for a push and the upstream serving element
  for a pull. Do not add an element-instance-name mode. Keep instance names,
  pad names, and paths in attributes, and add a stable operation attribute
  for queries that should not depend on display names. Fall back to the operation
  name when the factory cannot be resolved; omit dynamic IDs and full paths from
  names. This follows OpenTelemetry's guidance to use
  [general, human-readable span names](https://opentelemetry.io/docs/specs/otel/trace/api/#span).
  Cache bounded names with the site metadata so each buffer does not require
  string formatting or GObject lookups. Test push/pull direction, relinking,
  missing peers, and factory-less elements, and update queries and documentation
  that match the old names. The duration remains the inclusive synchronous call
  duration; a factory name does not make it exclusive element CPU time.
- [ ] Extend the README correlation video with logs. Add a logs backend to the
  demo and provision trace-to-logs and log-to-trace navigation. Emit a useful log
  inside the selected sampled span, then show metric exemplar → trace → matching
  log → the same trace, alongside the existing span-to-profile navigation.
  Verify trace/span IDs and time ranges in both backend data and the Grafana UI,
  including an unrelated-ID control. Re-record with factory-based span names,
  and update the demo scripts, provisioning, captions, and correlation guide.

### Performance and profiler lifetime

- [ ] Increase sustained trace export throughput. Benchmark larger batches,
  compression, and a small bounded number of concurrent requests against the
  serialized exporter. Compare local and delayed receivers, measuring loss,
  CPU, memory, and final drain time. A larger queue alone does not increase
  sustained throughput. Preserve bounded memory and cleanup behavior.
- [ ] Retire profile element descriptions safely after queued samples no longer
  reference them. The current 4,096-description limit applies over the process
  lifetime, so repeated pipeline creation with new names can exhaust attribution
  capacity. Test beyond that limit without relabeling old samples or retaining
  destroyed GStreamer objects.
- [ ] Reduce symbolization cost and temporary memory peaks. Measure cold/warm
  caches and large debug files; compare selective eviction with whole-cache
  clearing and evaluate an optional lightweight symbolization mode. Keep
  resolver work off streaming threads and out of the signal handler, preserving
  the protections against loader-lock deadlocks.
- [ ] Reduce OTel capture overhead on stock GStreamer; see the
  [measured limits](tracer/otel/README.md#performance-and-sampling-cost).
- [ ] Measure current full-metrics signal combinations alongside compatibility
  presets, including logs, profiles, and each self-monitoring level. Report
  throughput, CPU, steady/peak memory, losses, and shutdown time with matched
  workloads. Keep private application validation outside the repository.
- [ ] Complete long-running soak tests covering pipeline recreation, thread
  churn, plugin loading, collector outages/recovery, and repeated teardown on
  x86-64 and ARM64. Check settled memory, attribution capacity, queue bounds,
  and shutdown progress, alongside broader platform validation.

### Trace coverage

- [ ] Add buffer-list spans and exemplars. Individual push and pull calls already
  create spans; lists currently have metrics and attributed CPU profiles only.
- [ ] Add span links for fan-in and batched inputs, with explicit limits and
  documented behavior when elements replace buffers or strip metadata.
- [ ] Investigate per-buffer queue residence measurements separately from
  synchronous call duration, including matching and cleanup on flush/seek/drop.

GPU execution timing is out of scope. Revisit it only as a separate extension
after delivery, lifecycle, instrumentation health, and CPU profiling are solid.

Whole-bin elapsed latency across queues and parallel branches is out of scope:
input/output matching and completion are ambiguous. Existing metrics report
synchronous call costs and separate queue diagnostics.
