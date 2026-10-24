# Grafana trace/profile correlation validation

For the 2026-10-24 full metric → trace → profile check, see the
[explicit-signals review](SIGNALS_REVIEW.md). The record below describes the
initial trace/profile-only check.

Validated on 2026-10-23 with Grafana 13.1.1, Tempo 2.10.0 and Pyroscope 2.4.0.
The tracer runtime is `a4bb8fd`; this change adds documentation, provisioning and
a test workload without changing runtime behavior. The
[setup guide](../../tracer/otel/CORRELATION.md) contains the reproduction commands
and the remaining manual browser check.

The provisioned configuration and backend correlation queries passed against
real services. **Browser click-through was not verified:** the browser tool
reported no connected browser, so no button interaction or screenshot is claimed.

## Workload and method

The [example](../../tracer/otel/examples/profile_correlation.rs) sends four buffers
through two `identity` elements separated by a queue. Their handoff callbacks
consume 100 ms (`light`) and 250 ms (`heavy`) of thread CPU per buffer. Profiling
runs at 100 Hz with element attribution and tracing enabled. Explicit shutdown
drains both exporters before validation. This artificial workload creates spans
long enough to sample reliably; it does not measure instrumentation overhead.

The [checker](../../tracer/otel/bench/correlation/check.py) forwards the actual
OTLP and profile uploads to Tempo and Pyroscope while retaining their payloads
for decoding. Each run uses a unique service name. It then queries those
services through Grafana's data-source proxies and compares the returned CPU
weights with the decoded exports.

The documented release build and checker commands ran on Linux x86-64, using
Rust 1.89.0, `-C force-frame-pointers=yes` and GStreamer 1.24.2. Docker Compose
used the checked-in configuration with one local network override: this host's
default address pools were exhausted, so the demo used an unused
`10.253.71.0/24` subnet. No other containers or networks were removed or changed.
This host-specific override is not required by the correlation setup.

## Results

All 16 started spans were exported and decoded. Two profiles contained 138 CPU
samples, representing 1.38 CPU seconds. Of these, 137 samples matched eight
exported spans; one sample occurred outside a correlated push. There were no
trace or profile queue drops, export failures, registration failures, timer
overruns, failed stack reads or truncated stacks. No profiler threads remained
registered after shutdown.

The selected `heavy` span was `6693ea3f590c1762`, in trace
`8cb19ea01fbc44c49cf49b531039a28d`. Tempo retained both its
`pyroscope.profile.id` marker and the matching service resource. Queries through
Grafana's Pyroscope proxy returned:

| Query | Decoded upload CPU | Backend CPU | Result |
| --- | ---: | ---: | --- |
| Entire service/run | 1,380 ms | 1,380 ms | Exact match |
| Selected span | 250 ms | 250 ms | Exact match |
| Selected trace | 350 ms | 350 ms | Exact match |
| Unknown span (`ffffffffffffffff`) | 0 ms | 0 ms | No samples |

The selected span's flame graph contained
`[element] heavy [identity] — /GstPipeline:pipeline0/GstIdentity:heavy`, with the
native stack beneath it, including `profile_correlation::simulated_cpu_work`.
The selected trace's additional 100 ms belonged to its `light` span. Exact
agreement checks filtering and ingestion; sample counts may vary between runs.

Grafana's configuration API also confirmed the Pyroscope data-source UID,
`service.name` → `service_name` mapping, and
`process_cpu:cpu:nanoseconds:cpu:nanoseconds` profile type. Together these checks
establish that the stored data and configured backends support a span-specific
profile query. They do not establish that a particular browser renders or
activates the expected link correctly.

## Evidence and validation limits

- [Recorded API responses, decoded totals and source/binary hashes](correlation/2026-10-23.json)
- [Workload output and final exporter counters](correlation/2026-10-23.txt)
- [Provisioning YAML](../../tracer/otel/bench/correlation/datasources.yaml)
- [Local service definitions](../../tracer/otel/bench/correlation/compose.yaml)

The JSON records the parent commit because the new harness was uncommitted at
execution time; its source hashes identify the tested files. The saved Explore
URL requires the original local data to remain available. Rerun the checker to
generate a fresh trace and URL after retention or container cleanup.

This demo covers trace/profile correlation. It does not deploy metrics or logs
backends, validate their navigation links, or test Grafana Cloud. The guide
separately describes the configuration those links require.

Passed `cargo fmt --all`, workspace/all-features/all-targets Clippy with warnings
denied (Rust 1.99.1), `cargo test`, the profiles-feature test suite, and nightly
`cargo udeps` for both tracer packages. `cargo audit` exited zero with the
existing allowed Tokio `RUSTSEC-2026-0386` warning; dependencies were unchanged.
The Python files parsed successfully and Docker Compose validated the service
configuration.
