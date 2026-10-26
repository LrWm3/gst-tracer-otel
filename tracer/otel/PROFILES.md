# mars-profiles: profiles without OTLP telemetry

`mars-profiles` is a tracer entry point in `libgstoteltracer.so`, built with the
`profiles` Cargo feature. It uses the same embedded sampler, bounded buffers,
background symbolization, and Pyroscope uploader as correlated profiling in
`mars-otel` and `mars-instrument`. It collects CPU profiles by default, without
OTLP requests, span capture, buffer trace metadata, metric series, or a
GStreamer log bridge.

The sampler supports Linux x86-64 and arm64 (AArch64), with GStreamer 1.22 or newer.
Build and run:

```sh
RUSTFLAGS="-C force-frame-pointers=yes" cargo build --profile profiling -p gst-otel-tracer --features profiles
export GST_PLUGIN_PATH="$PWD/target/profiling"
export GST_TRACERS=mars-profiles
export OTEL_SERVICE_NAME=gstreamer-pipeline
export GST_OTEL_PROFILE_URL=http://localhost:4040
gst-launch-1.0 videotestsrc ! videoconvert ! video/x-raw,format=RGB ! fakesink sync=false
```

| Setting | Default | Meaning |
| --- | --- | --- |
| `profile-sources` tracer property | `cpu` | `cpu`, `gpu`, or `cpu,gpu`; GPU values require a Linux `gpu-profiles` build |
| `GST_OTEL_PROFILE_URL` | `http://localhost:4040` | Pyroscope base URL |
| `GST_OTEL_PROFILE_HZ` | `100` | CPU samples per second per registered thread; 1–1000 |
| `MARS_PROFILE_TIMEOUT_MS` | `10000` | Per-upload HTTP timeout, 1–60000 ms; fallback `GST_OTEL_PROFILE_TIMEOUT_MS` |
| `GST_OTEL_PROFILE_ELEMENTS` | `true` | Group native stacks by element execution context or task owner |
| `GST_OTEL_PROFILE_SIGNAL` | `SIGRTMIN()+6` | Reserved realtime signal |
| `OTEL_SERVICE_NAME` | SDK-detected service name | Pyroscope `service_name` |

The `MARS_PROFILE_*` equivalents take precedence over the environment names above.
`mars-instrument(signals="profiles")` selects the same sampler through the
[unified interface](INSTRUMENT.md), without enabling metrics, traces or logs.

Select sources through tracer properties, for example
`GST_TRACERS='mars-profiles(profile-sources="cpu")'`. With the optional
`gpu-profiles` build feature, use `profile-sources="gpu"` for GPU kernels only
or `profile-sources="cpu,gpu"` for both. GPU values are unavailable in other
builds. This construction-time property has no environment equivalent; CPU is
still the default in GPU-enabled builds. See the [GPU experiment](GPU_PROFILES.md)
for its native adapter, GObject property API, and limitations.

Selecting `mars-profiles` or an explicit `profiles` signal enables profiling
even when the URL is unset. With `mars-otel` or the original no-parameter
`mars-instrument` preset, the URL remains required to opt into profiles.
Trace, metric, and log configuration is ignored by `mars-profiles`, including
`GST_INSTRUMENT_PROFILES`. Invalid profiler settings disable the tracer with an
error; they do not enable another telemetry signal.

Select one of `mars-profiles`, `mars-otel`, or `mars-instrument`. They share one
process-wide configuration; the first constructed instance selects the signals
and installs hooks. Later instances log a warning and install no hooks. For
profiles correlated with traces, select `mars-otel` or `mars-instrument` and set
`GST_OTEL_PROFILE_URL`. Then configure the
[Grafana trace-to-profile link](CORRELATION.md); IDs alone do not configure data-source navigation.

## Finding the element that used CPU

Profiles have a synthetic root frame identifying the current element instance,
followed by its original native stack. For example:

```text
[element] heavy [identity] — /GstPipeline:pipeline0/GstIdentity:heavy
  clone3 → … → gst_pad_push → … → application_callback
```

This distinguishes instances using the same GStreamer wrapper or callback.
It works in all three tracer factories, independently of trace sampling.
There are three kinds of root:

- **`[element]`**: the innermost active chain, chain-list, or upstream pull-range
  call. Nested calls restore their caller on return. Each CPU sample contributes
  to one element only; its total excludes samples attributed to nested elements.
- **`[task owner]`**: CPU outside those calls on a GStreamer task owned by this
  element, identified by synchronous `STREAM_STATUS` ENTER/LEAVE messages. This
  includes source production and queue task work, but is ownership context,
  not proof that all that CPU was spent inside the owner's implementation.
- **`[unattributed GStreamer CPU]`**: a registered thread without reliable
  context, including scope overflow or exhausted description capacity.

These are pprof frames, not new series labels or duplicated samples. In Grafana
Explore, expand frame groups to see the names, or sort the top table by **Total**.
Synthetic frames have zero **Self** time; their **Total** is the grouped CPU.
See the [screenshots and validated example](../../reports/otel/PROFILE_ELEMENT_REVIEW.md).
Set `GST_OTEL_PROFILE_ELEMENTS=false` for native stacks without these roots.

The optional [NVTX worker bridge](GPU_PROFILES.md#optional-inference-worker-attribution)
attributes supported `nvinfer` worker ranges and registers those threads for CPU
sampling. It works with the `profiles` feature alone, independently of GPU
collection, and needs an explicitly enabled native adapter. Other private worker
pools remain outside automatic attribution.

Attribution does not recover missing native symbols. Events, queries, and
arbitrary callbacks have no separate scope hooks; they retain the surrounding
call/task context. Task-owner names are captured at ENTER; chain descriptions
refresh after name/parent changes.

## Linux arm64

Use the same feature, tracer names, configuration, and build command on a native
`aarch64-unknown-linux-gnu` system. Element/task attribution and trace correlation
work independently of native-stack completeness on both architectures.
See [validation on an integrated memory nvidia device and measurements](../../reports/otel/ARM64_PROFILES.md).
Validation uses GNU libc; other C libraries have not been tested on ARM.

For native C/C++ dependencies built with GCC or Clang, keep both ordinary and
leaf frame pointers. The [ARM Meson native file](gstreamer/native.aarch64.flags.ini)
adds `-mno-omit-leaf-frame-pointer` to the existing native flags:

```sh
meson setup build --native-file /path/to/gst-tracer-otel/tracer/otel/gstreamer/native.aarch64.flags.ini
```

The sampler reads the interrupted PC/SP/x29 and walks only the bounded copied
stack. Saved return addresses have their PAC signature stripped with XPACLRI
(a backwards-compatible HINT on CPUs without pointer authentication), then
are adjusted by four bytes to identify the calling instruction. No fixed virtual
address-width mask is used. The original interrupted PC is preserved.

An omitted frame or interruption during a function prologue/epilogue can skip
callers or shorten the stack. Live x30 is deliberately not appended: it may
describe a nested call inside the interrupted function rather than its caller.
Missing/unreadable frames still retain the interrupted PC and element context.
This does not add DWARF unwinding, JIT support, or recovery of tagged stack
pointers. `truncated_stacks` counts detected chain failures, not every omitted
frame. Stock vendor libraries can therefore produce incomplete native stacks.

An integrated memory nvidia device may ship GStreamer 1.16 or 1.20. The existing 1.22 minimum
still applies; use an application environment with newer GStreamer. The
[validation Dockerfile](bench/arm64/Dockerfile) provides Rust and GStreamer 1.22
in a container without changing the host installation. It tests CPU profiling;
it does not include NVIDIA GPU plugins or measure GPU execution.

## Sampling and shutdown

With element attribution enabled, GStreamer task ENTER registers a native
thread before its first buffer, including source production. Push, buffer-list,
chain, and pull hooks also register participating threads. Sampling then includes
CPU between calls until thread exit or shutdown, for up to 64 concurrent threads.
Threads that never reach these hooks or a recognized optional NVTX range, work
before registration, and sleeping time are not sampled. With attribution
disabled, profiles-only registration starts at the first push, buffer-list push,
or pull-range call. Samples carry
native `thread_id` labels, without trace or span IDs.

The context stack is bounded at 256 calls per thread; immutable descriptions are
bounded at 4096 live or awaiting retirement, with names derived from paths capped
at 512 characters and factories at 128 characters. Metadata and active execution
scopes retain a description lease. Once its final lease is released, the report
worker drains all sample rings and encodes their original names before reclaiming
the description. IDs are never reused, including when the same path is recreated.
Pipeline recreation can therefore exceed 4096 descriptions over process lifetime.
Overflow becomes unattributed until a report releases capacity; cached misses
retry after that reclamation. The
signal handler reads only an atomic integer; all name lookup and synthetic frame
construction happen outside signals. No GStreamer objects are retained by the
description registry.

`MARS_PROFILE_SYMBOLIZATION=full` is the default: native function names, source
locations and inline frames when debug information is available. Use
`MARS_PROFILE_SYMBOLIZATION=functions` to read ELF symbol tables without loading
DWARF or separate debug files. This reduces resolver work and temporary memory
at the cost of source locations and inline detail; stripped private functions
may appear only under their library name. Element roots and trace IDs are
unchanged. `GST_OTEL_PROFILE_SYMBOLIZATION` is the fallback name.

The worker keeps up to 4096 resolved addresses and 4 MiB of frame/string data.
Loading an unrelated library preserves existing cached symbols; replacing or
unmapping a library invalidates its addresses. Capacity pressure evicts stale
entries absent from the current report in least-recently-used order. A report
with more unique addresses than the cache can hold retains unresolved addresses
as native fallbacks. These limits exclude temporary ELF/DWARF parsing and cache
bookkeeping, so they are not a bound on process RSS.
See the [cold/warm resolver measurements](../../reports/otel/PROFILE_VIDEO_RESULTS.md#symbolization-cost-and-cache-retention--2026-10-25)
for the measured tradeoff in a build with debug information.

Install debug symbols and build relevant native libraries with frame pointers
for useful stacks. The Rust flag above only changes Rust code. The
[native build flags](gstreamer/native.flags.ini) can be passed to GStreamer's
Meson build with `--native-file`; the shared sampler's
[unwinding limits and memory bounds](README.md#experimental-correlated-cpu-profiles)
also apply here.

Profiles upload approximately once per second. Stop streaming, then call
`gstoteltracer::shutdown(timeout)` to stop timers and drain the final profile.
Shutdown uses the same bounded deadline as correlated profiling, can be retried
after a timeout, and cannot restart sampling. `gstoteltracer::flush(timeout)`
drains the current profiles while preserving sampling for other or later
pipelines. C/Python applications can invoke the tracer's `flush` or `shutdown`
[action signals](README.md#drain-and-counters), with a millisecond timeout and
per-signal results. Pipeline NULL transitions do not drain profiles automatically.
`gst-launch-1.0` relies on periodic uploads and may lose the final interval when
exiting.

Sampling buffers are bounded and intentionally remain allocated until process
exit to protect pending signals. Element descriptions are reclaimed after their
last sample drain; an idle active task can retain its owner description until
LEAVE or thread exit. Temporary symbolization allocations and
allocator retention can make process RSS much larger than the retained caches.
The [native ARM lifecycle check](../../reports/otel/ARM64_PROFILES.md#deadlock-and-memory-growth-check)
observed settled memory usage and prompt shutdown after repeated pipeline
creation and destruction; it does not rule out every production leak or hang.

`gstoteltracer::profile_stats()` exposes sampling, registration, processing,
and upload-queue counters. `element_samples` and `task_samples` count accepted
samples with those contexts; `context_capacity_rejections` counts descriptions
rejected by the registry limit. `context_descriptions` is the current retained
count and `context_retired` is the cumulative reclaimed count. The `core` health
metrics expose both when metrics are selected. Trace and metric counters remain
zero in profiles-only mode.

The separate [uploader](src/profileupload.rs) exposes `reported_samples` (queued),
`accepted_samples` (HTTP acknowledged), `failed_samples` (rejected or unconfirmed),
`accepted_profiles`, `failed_exports`, `pending_samples`, and `pending_profiles`.
After producers stop and a drain completes, reported samples equal accepted plus
failed samples and pending counts are zero. A timeout can leave delivery uncertain;
HTTP acknowledgement does not promise durable backend storage.

The queue holds ten profiles, plus one in flight. Requests use gzip and the
existing Pyroscope push endpoint. Responses must be HTTP 200 with a valid bounded
protobuf body. Failed uploads are not retried automatically, avoiding an extra
retry queue and duplicate requests after ambiguous timeouts. Subsequent profiles
continue normally after an error, including after a failed explicit flush.
A forwarding proxy can provide authentication or its own retry policy.

Flush/shutdown return `export-failed` for unsuccessful uploads since the previous
profile drain. Cumulative counters retain older failures. The C/Python drain
structure's `profile-delivery-confirmed` is true only when profiles were uploaded,
all queued samples were acknowledged, and no processing/upload loss has occurred;
it stays false after earlier losses, for disabled profiling, and after a timeout.
Capture losses such as full sample rings remain separate counters.

## Measured overhead

The current [1080p signal matrix](../../reports/otel/INSTRUMENT_VIDEO_RESULTS.md#current-full-metrics-signal-matrix--2026-10-25)
uses 100 Hz profiles with element attribution enabled. It measures profiles-only,
full instrumentation, compatibility presets, and each monitoring level, including
CPU, steady/peak memory, delivery checks, and shutdown. Profiles-only added about
2% CPU cycles and 42 MiB peak RSS on that workload; the report retains the run
variation. Different pipelines, sample rates, native libraries, and debug symbols
can change these costs.

The [profiling report](../../reports/otel/PROFILE_VIDEO_RESULTS.md) also retains
older comparisons, symbolization measurements, and native lifecycle soaks.
Its historical pre-attribution results are separate from the current matrix.
The [element review](../../reports/otel/PROFILE_ELEMENT_REVIEW.md) includes a
matched attribution-on/off check.

With attribution enabled, the [check on an integrated memory nvidia device](../../reports/otel/ARM64_PROFILES.md)
measured approximately 1.3% extra process CPU time for profiles-only and 3.1% for
OTel with profiles and logs. Its builds include debug information, and memory
peaks varied substantially; use the report's observed upper values when planning.

## Migrating from the standalone plugin

The `gst-pyroscope-tracer` package and `libgstpyroscopetracer.so` are removed.
Remove any old installed copy of that library from your GStreamer plugin path
before loading the new `mars-profiles` factory from `libgstoteltracer.so`.

Replace the old tracer parameters with the shared profiler configuration:

| Old parameter | Replacement |
| --- | --- |
| `server-url` | `GST_OTEL_PROFILE_URL` |
| `sample-rate` | `GST_OTEL_PROFILE_HZ` |
| `tracer-name` | `OTEL_SERVICE_NAME` |
| `stop-agent-on-dispose` | Explicit `gstoteltracer::shutdown(timeout)` |
| `tags` | No equivalent arbitrary series tags in the embedded backend |

The old `mars-profiles(...)` parameter syntax is rejected with an error rather
than silently ignored. The former process-wide pprof backend is replaced by
the participating-thread sampler described above.

## Experimental CUDA kernel profiles

The optional `gpu-profiles` extension builds element → kernel profiles with
span/trace correlation, independently of CPU sampling. See
[the branch experiment and its limits](GPU_PROFILES.md).
