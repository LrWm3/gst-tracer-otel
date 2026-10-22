# Trace and profile integration investigation

Investigated on 2026-10-20/21 against `1b9e13e`. The investigation below describes
that baseline. An opt-in embedded prototype now implements the custom-backend
alternative in [profiles.rs](tracer/otel/src/profiles.rs), with
[configuration and limits](tracer/otel/README.md#experimental-correlated-cpu-profiles).
Tests capture real CPU-timer samples and decode the resulting Pyroscope and OTLP
requests to verify nested-push and queue-thread attribution. A local Pyroscope
2.4.0 server subsequently accepted the uploads and returned exactly matching
aggregate, span-ID and trace-ID CPU weights; an unknown span returned no samples.
Grafana UI validation, broader native unwinding and production lifecycle support
remain open. The external-profiler ABI described
below is not implemented by this prototype.

A shared-library smoke test also loaded the feature-enabled plugin through
GStreamer and ran 240 video buffers through conversion. It exported all 960
push spans with no trace losses and received four profile reports. All 71
correlated stack entries matched an exported trace/span and its profile-link
attribute. This smoke test validates local payloads and normal plugin loading;
the subsequent server check above validates ingestion and query selection.

The recommended integration is an optional publisher of the currently executing
push's trace/span IDs, read by the profiler **when it captures a sample**. Keep
the compact tracing path and export workers. Do not restore the removed span
processor or call Pyroscope thread-tag APIs from push hooks.

Prefer an external profiler consuming OpenTelemetry thread context as the target
architecture on Linux. First prove a specific publisher/reader/backend version
combination in an isolated experiment: the upstream native thread-context reader
is still unmerged at investigation time. If correlation must ship independently
of that reader, it requires an in-process sampler change, not an adapter around
the existing tag API. Service/time navigation can be delivered separately with
the current independent collectors.

## Deadlock audit and worker changes

The follow-up implementation keeps the sampler and profile worker in
`tracer/otel/src/profiles.rs`. It allocates rings only for used slots, retains
a bounded address-to-frame cache, and interns directly into pprof before
enqueueing. The span-link encoder also avoids per-span temporary strings and
protobuf allocations. Measurements are in the tracer README.

Source inspection found three relevant hazards. These are not a diagnosis of
the previously observed production hang; no stack dump from that hang is
available.

1. The old `pprof2` 0.13.1 signal handler uses `PROFILER.try_write()`, which avoids
   blocking on that particular lock, but its default unwinder still calls
   `backtrace::trace_unsynchronized` from the signal handler. "Unsynchronized"
   removes the Rust backtrace mutex; it does not make the underlying native
   unwinder async-signal-safe. A signal interrupting native loader/unwinder
   activity is a plausible deadlock path. The embedded handler instead uses
   atomics, syscalls and a bounded copied-stack frame-pointer walk.
2. `backtrace` 0.3.75 symbolization holds its global reentrant mutex and can call
   `dl_iterate_phdr`, which takes glibc's loader lock. Another thread holding
   that loader lock and calling backtrace can form a lock-order cycle. The
   prototype's original background resolver also used this API. It now reads
   `/proc/self/maps` and ELF/DWARF files through scoped `object`/`addr2line`
   readers, without either lock. A regression test resolves a known function,
   source file and line while another thread holds the loader lock; the callback
   releases the lock on timeout so failures cannot strand the test runner.
3. Both inspected SDK Agents (0.5.8 and 2.1.2) set their `running` flag false and
   notify `stop()` only in the normal `TimerSignal::Terminate` branch. Report,
   session creation or channel-send errors can return earlier. If `stop()` has
   already queued its messages, it can wait forever on that condition variable.
   The embedded integration now owns scheduling and shutdown and uses the SDK's
   `SessionManager` solely for delivery. Worker joins observe early exit/panic;
   nonblocking enqueue prevents a full upload queue from stalling processing.
   Caller deadlines also cover registration/handler quiescence and upload drain.

The custom backend supplies `RawPprof`; the pinned SDK still builds the push
request, applies service labels, compresses and sends it. At most ten queued
profiles, each at most 8 MiB, can wait behind the active request. Queue saturation
and encoding-size drops have sample counters. There is no accumulating timer
message queue. A slow receiver can still delay background shutdown while queued
SDK requests finish (each has the SDK's ten-second HTTP timeout). Non-success
HTTP responses remain SDK log events, not delivery acknowledgements available
to this integration.

Tests also exercise a full/disconnected upload channel, 1000 Hz sampling during
allocation and mutex/loader activity, more thread retirements than available
slots, and a stalled registration lock. Child processes have watchdog deadlines.
The standalone `mars-profiles` tracer (`gst-pyroscope-tracer`) still uses its original dependencies and
does **not** receive these embedded-worker changes. Do not treat this work as a
fix for that separate plugin's runtime.

Validation after the follow-up: formatting, all-feature workspace Clippy,
default workspace tests, OTel tests with and without `profiles`, `cargo audit`,
and nightly unused-dependency checks (including the optional profile feature)
passed. The feature run passed 30 unit and 12 integration tests. The audit
initially rejected the old symbolizer dependency's `ruzstd` 0.7.3; the final
lockfile uses `addr2line` 0.27.1 / `object` 0.40.0 / fixed `ruzstd` 0.9.1, and
performance/server checks were repeated with those final dependencies.

## Baseline investigation

**What the repository actually supports**

- [Capture](tracer/otel/src/capture.rs) owns a per-thread nested push stack and
  allocates sampled span IDs before downstream work. This is the integration
  point. Records are completed and sent to the worker on push return.
- [Propagation](tracer/otel/src/propagation.rs) carries trace context on buffers
  across queues and copies. A destination thread must publish its own active
  context when it begins a push; buffer metadata alone is invisible to a CPU
  sampler.
- [The public context accessor](tracer/otel/src/lib.rs) exposes the current push
  to ordinary application instrumentation. It uses Rust TLS and a `RefCell`
  internally, constructs a `SpanContext`, and can clone trace state. It is not
  an asynchronous signal handler interface or an external-profiler ABI.
- [Trace encoding](tracer/otel/src/wire.rs) is direct OTLP protobuf. There are no
  live SDK spans and no `SpanProcessor` callbacks to extend. Both this encoder
  and the reference `SpanData` conversion in [export.rs](tracer/otel/src/export.rs)
  would need any new profile-link attribute, with equivalence tests.
- [The standalone profiler](tracer/pyroscope/src/profiles.rs) starts a
  process-wide CPU profiler on the first successful addition to a pipeline.
  It currently has no trace-context connection. Its `pipeline` tag comes from
  that first pipeline even though it profiles the process, including other
  pipelines and non-streaming threads.

**Why the previous approach cannot be repaired by moving work to a worker**

The lockfile resolves `pyroscope` 0.5.8, `pyroscope_pprofrs` 0.2.10 and `pprof2`
0.13.1. Inspection of those downloaded crate sources confirms:

1. `Pprof::add_rule` and `remove_rule` call `dump_report` while the profiler is
   active. That resolves stacks, applies the current rule set and resets the
   profiler. It runs on the caller's thread.
2. Rules are applied to a report after sampling. Deferring a tag change changes
   its attribution interval. Removing the report boundary would label earlier
   samples with whichever span happens to be current at report time.
3. `pprof2::UnresolvedFrames` contains `sample_timestamp`, but its equality/hash
   key uses thread ID and stack addresses, excluding that timestamp. Its
   collector merges repeated samples before report processing. A later join
   against span time intervals therefore cannot recover individual sample times.
4. The old processor also used different root predicates at start and end:
   start tested the parent's remote flag, end tested the span's own remote flag.
   A local span under a remote parent can acquire a label without removing it.
   Root-only labeling also did not activate context on downstream queue threads.

The [previous performance review](OTEL_PERFORMANCE_REVIEW.md) records the measured
costs. The [removed processor](https://github.com/LrWm3/gst-tracer-otel/blob/c5c45815bc24b256b97bb7c24b0ea21e278da1ef/tracer/otel/src/pyroscopespanprocessor.rs)
is useful historical evidence, not an implementation to revive.

I also inspected the published `pyroscope` **2.1.2** source, release commit
`7423c074b5934bd279e91e43a5e321c7dfcdcbb5`. Its integrated backend replaces the
old reset with `build_and_clear(true)`, but `add_tag` and `remove_tag` still call
`dump_report` synchronously. Its unresolved sample key still omits timestamps.
An upgrade may improve independent profiling; it does not supply cheap span
correlation. See the pinned [backend](https://github.com/grafana/pyroscope-rs/blob/7423c074b5934bd279e91e43a5e321c7dfcdcbb5/src/backend/pprof.rs)
and [sample key](https://github.com/grafana/pyroscope-rs/blob/7423c074b5934bd279e91e43a5e321c7dfcdcbb5/src/backend/pprofrs/frames.rs).

**Backend choices**

| Approach | What it provides | Assessment |
| --- | --- | --- |
| Shared service identity plus Grafana time-range navigation | Process/service profile near a span | Useful immediately; includes concurrent unrelated work |
| Existing Pyroscope thread tags, including 2.1.2 | Report-boundary labeling | Reject for high-frequency push hooks |
| OpenTelemetry thread-context publisher plus external eBPF reader | Sample-time context without an application-owned sampler | Preferred Linux direction; exact reader/version compatibility remains a gate |
| Modified in-process sampler plus pprof sample labels | Sample-time context with an embedded profiler | Feasible fallback; requires sampler, aggregation, ABI and lifecycle work |

The external option has two separate readiness questions. Pyroscope supports
receiving profiles from the [OpenTelemetry eBPF profiler](https://grafana.com/docs/pyroscope/latest/configure-client/opentelemetry/ebpf-profiler/),
but that alone does not make this plugin's context visible. The native TLS reader
[PR #1869](https://github.com/open-telemetry/opentelemetry-ebpf-profiler/pull/1869)
was **open, not merged**, verified through the GitHub API on the investigation
dates. Its tests explicitly cover libraries loaded with `dlopen`, which matters
for GStreamer plugins. Check that PR and the exact profiler release again before
implementation; do not assume a released Alloy image contains it.

Alloy's documented [OBI/Beyla correlation](https://grafana.com/docs/alloy/latest/reference/components/pyroscope/pyroscope.ebpf/#correlate-request-profiles-with-beyla-traces-and-spans)
uses shared eBPF maps for compatible instrumentation. Enabling that option does
not publish `gst-otel-tracer`'s private Rust TLS. Deployment also needs a compatible
Linux kernel, host process visibility and the profiler's required privileges;
this is a different deployment model from loading an unprivileged tracer plugin.

The [thread-context proposal](https://github.com/open-telemetry/opentelemetry-specification/blob/bd83138a95b6ca31384f0cb2490b2069bdec26f5/oteps/profiles/4947-thread-ctx.md)
defines an exported native TLS symbol, `otel_thread_ctx_v1`, plus process-context
discovery/schema metadata. A Rust `thread_local!` declaration alone is
insufficient. [libdd-otel-thread-ctx](https://github.com/DataDog/libdatadog/tree/679f33240a213e97c47639c36ef9a827d35bb87e/libdd-otel-thread-ctx)
is a concrete publisher candidate; the inspected implementation offers reusable
records and Linux x86-64/aarch64 support. Evaluate its released API, ownership,
thread-exit cleanup, dynamic symbol visibility, process-context companion and
reader schema together before adopting it. Merely linking the publisher is not
an end-to-end integration.

**Attribution contract to implement first**

Use the innermost *recording* push span as the first supported mode. Its trace
and span IDs already exist in `capture::Token`, and this avoids introducing a
second propagated identity. The sequence on a thread should be:

```text
outside -> push A -> push B -> return B -> return A
context:   A         B         A          previous outside context
```

A sample gets one active span ID. Consequently A's direct profile excludes
samples labeled B, even though A's exported duration includes B. An inclusive
profile requires a query over descendant span IDs; do not duplicate sample
weights or present direct samples as inclusive element CPU time. A push is also
named for a pad call and includes downstream processing, not just the source
element's own work.

At a queue boundary the consumer publishes the new push span derived from the
propagated token. Queue residence time, processing before a thread's first push,
source generation before its push, other worker threads, and unsupported
buffer-list/pull operations are outside this attribution window. Existing
[propagation limitations](tracer/otel/README.md#correlation-boundaries) still apply.
Profiling does not repair missing metadata or generic fan-in parentage.

A later whole-buffer-journey mode would need an explicit correlation-root ID in
the propagated token and metadata. Preserve it across queues after the original
push has ended, distinguish it from the immediate parent, and define query time
ranges for asynchronous descendants. Simply labeling root spans on their
original thread does not implement that mode. A thread's outermost push is a
third possible scope; it is not necessarily the trace root.

At 100 Hz, a single fully busy thread executing for 1 ms has roughly 0.1 expected
CPU samples under an idealized sampling model. Many short pushes will legitimately
have none. The wall duration of a blocked push is not its sampled CPU time.
Aggregate profiles remain useful even when individual buffer journeys are too
short to yield meaningful flame graphs.

**Capture and publication changes**

Add an optional publisher module owned by the OTel capture side. Suggested
configuration names such as `GST_OTEL_PROFILE_CORRELATION=off|thread-context`
are proposals, not existing options. Default to off; publishing must never start
an agent implicitly. Unsupported platforms should produce a clear initialization
diagnostic while ordinary tracing remains available.

For each thread, initialize reusable publication storage outside the steady
state hook path. After `begin_local` has committed a sampled recording frame,
publish the fixed-size IDs and flags immediately before returning to downstream
execution. On the matching `end`, restore the enclosing frame or previously
published application context before submitting the completed record. Keep the
restore independent of export success and use short TLS borrows as capture does
today. Do not hex-format IDs, build attributes, resolve symbols, take an agent
lock, or create SDK spans in these operations.

Suppress Gst correlation during unsampled, non-recording or overflowed pushes;
retain enough state to restore the enclosing context on return. In particular,
the existing top frame remains visible during `MAX_DEPTH` overflow, so blindly
copying `current_span_context()` would falsely attribute untracked work to it.
On a mismatched post hook, invalidate publication and recover only at a known
balanced boundary. Preserve and restore any external publisher's context; a
rejected Gst trace must not accidentally inherit a stale sampled Gst span.

The native publication protocol must handle interruption during an update:
invalidate, write fixed fields, then publish validity with the specified compiler
ordering. A sampler interrupting that transition leaves the sample uncorrelated.
It must not spin waiting for a writer that it has interrupted. The OTel protocol
assumes the target thread is stopped/interrupted while read; an unrelated worker
polling mutable TLS is not a conforming reader. Thread exit, shutdown with open
pushes, and plugin lifetime must not leave readable freed memory.

Do not connect the two `cdylib` plugins by statically linking a common Rust crate
and assuming its globals/TLS are unique process-wide. Each shared library may
contain its own instance. An embedded fallback needs a single owner with an
explicit, versioned C ABI or a separately shared native provider, plus an
initialization/lifetime contract. Test the actual `.so` loading order and threads
created by GStreamer, not only an application statically linked to the `rlib`.
For an external publisher, inspect `.dynsym` and validate native TLS discovery
after `dlopen` and in the presence of another SDK publisher.

**What the embedded fallback would actually require**

The sampler must copy trace/span IDs at stack capture time from a signal-safe
native slot, before aggregating. Carry those IDs through unresolved frames,
equality/hash, resolved frames, stack metadata and pprof encoding. Equal stacks
sampled in A and B must remain separate; repeated stacks within A may aggregate.
Avoid raw references to `Token`, `Arc`, buffers or application strings in a
sample. A new backend implemented only above the current aggregated report API
is too late.

Use bounded sample storage, bounded aggregation and a bounded export queue;
per-span labels increase the number of distinct sample keys. Count samples lost
to capacity, unavailable context, unwinding failure and export failure. Keep
symbolization, protobuf work and network requests on workers. Verify that the
sampler's actual signal/CPU-clock behavior covers multiple busy streaming threads
fairly; a configured rate is not proof of one sample per period per thread.
Avoid dual ownership of the process's profiling signal when both tracers load.

This is a suitable upstream contribution to the current Pyroscope sampler or a
deliberately maintained backend. Reintroducing the old `pprof2` dependency stack
through a private fork would also inherit its maintenance burden. Validate native
stack unwinding separately from symbol resolution: the current backend enables
frame-pointer unwinding, so Rust build settings alone do not ensure usable stacks
through C GStreamer libraries. Debug symbols improve names/lines but do not
restore frame pointers omitted at compilation.

**Export and Grafana contract**

Pyroscope span profiles use `span_id` and optionally `trace_id` on individual
samples. Export `pyroscope.profile.id` on eligible trace spans using the same
span ID, conventionally 16 lowercase hex digits. This marker means correlation
is enabled for that span, not that a sample was captured or delivered. Keep IDs
out of service-level series tags. These conventions are documented in
[Grafana's span-profile guide](https://grafana.com/docs/pyroscope/latest/configure-client/trace-span-profiles/).

Store an eligibility bit on the compact record so worker encoding reflects what
was enabled when the push ran. Account for the attribute in both encoding paths,
attribute limits and dropped-attribute counts. Do not emit truncated IDs as
usable links when string limits are smaller than the required ID length; define
and test omission behavior. A trace lost to queue pressure can still have profile
samples, and a retained trace can have zero profile samples. These are independent
loss domains and must remain observable.

Align OTel `service.name` with the profile `service_name`, and add a matching
instance/container selector where needed. The existing profiler's package-valued
`service` tag and first-pipeline tag do not establish that contract. Provision
Tempo's `tracesToProfiles` to point to Pyroscope, map `service.name` to
`service_name`, and select the profile type actually emitted by the chosen
backend. See [Grafana provisioning](https://grafana.com/docs/grafana/latest/datasources/tempo/configure-tempo-data-source/provision/).
Service/time navigation can use this configuration before exact sample
correlation exists, but should be described as a process profile near the span.

OTLP Profiles is another export format, not a sampling solution. The pinned
`opentelemetry-proto` 0.33.1 contains feature-gated `v1development` profile
messages; this workspace does not enable them. The [Profiles specification](https://opentelemetry.io/docs/specs/otel/profiles/)
is Alpha and includes sample links containing trace/span IDs. An external
profiler can own that protocol. Do not add a second custom OTLP encoder to this
plugin merely to correlate samples; pprof labels also meet the Pyroscope use case.

**Implementation sequence and acceptance**

1. Normalize service/instance identity and add a reproducible Tempo/Pyroscope/
   Grafana fixture. Fix standalone profiler configuration and lifecycle while
   touching that code: validate the signed sample rate before casting, handle
   startup/shutdown errors without hook panics, enforce one sampler owner and
   document explicit final flushing. Tracer disposal is not pipeline-stop scope.
2. Build a minimal Linux publisher/reader experiment with pinned versions. Two
   threads alternate recognizable CPU workloads under different IDs, nesting
   and restoring contexts. Require decoded samples and a successful backend
   query; an exported TLS symbol or a Grafana link alone is insufficient. If the
   required reader remains unavailable, keep this experimental and choose the
   sampler-change fallback only as a separately scoped effort.
3. Integrate the proven publisher into `capture::begin_local`/`end`, preserve
   sampling and stack recovery, and add the worker-side link attribute. Keep the
   disabled path cheap and independent of the standalone Pyroscope plugin.
4. Add deterministic publication/encoding tests plus real pipeline/backend tests.
   Cover nesting/restoration, remote parents, queue and tee branches, copied
   buffers, reused buffers, missing context, 0%/partial/100% trace sampling,
   overflow, mismatched hooks, shutdown during an open push, thread churn,
   multiple pipelines, duplicate plugin loading and an absent profile backend.
   Force interruptions at publication boundaries in the publisher harness. Test
   known identical stacks under different span IDs to detect premature merging.
5. Benchmark tracing alone, profiling alone, both without correlation and both
   with correlation. Reuse the existing 1/4/16-stream and tee/queue workloads.
   Report streaming-thread CPU, total process CPU, external collector CPU/RSS,
   latency, allocations, trace delivery/loss and profile sample counts/loss.
   Include partial sampling and slow/absent receivers. Accept only measured
   bounded behavior; fewer delivered traces or samples is not an optimization.

**Evidence and validation from this investigation**

A standalone experiment linked the workspace's compiled `pprof2` 0.13.1. After
capturing an unresolved stack, it cloned the key and advanced only its timestamp
by 60 seconds. The keys compared equal, and inserting both into a `HashMap`
produced one entry with count two. This directly confirms that report timestamps
cannot support a lossless later join. It is not a performance benchmark or an
end-to-end correlation test.

All required repository checks passed:

- `cargo fmt --all`
- `cargo clippy --workspace --all-features -- -D warnings`
- `cargo test`: default Prometheus member, 17 unit and 16 integration tests
- `cargo audit`: exit 0 with the repository's existing audit configuration
- `cargo +nightly udeps -p gst-otel-tracer -p gst-prometheus-tracer -p gst-pyroscope-tracer --all-targets`

Additionally, `cargo test -p gst-otel-tracer` passed 23 unit and 8 integration
tests; three manual tests remained ignored. `git diff --check` passed. These
checks establish the current baseline, not the proposed integration's correctness.
No backend was deployed, no sampler patch was installed, and no correlation
performance claim is made by this investigation.
