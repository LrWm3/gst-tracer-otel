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
cargo audit
cargo +nightly udeps -p gst-otel-tracer -p gst-prometheus-tracer --all-targets --all-features
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

- Reduce OTel capture overhead on stock GStreamer; see the
  [measured limits](tracer/otel/README.md#performance-and-sampling-cost).
- Extend combined capture to pull mode and buffer lists, and bring over the
  remaining Prometheus diagnostics.
- Complete production soak testing and broader platform validation.

Whole-bin elapsed latency across queues and parallel branches is out of scope:
input/output matching and completion are ambiguous. Existing metrics report
synchronous call costs and separate queue diagnostics.
