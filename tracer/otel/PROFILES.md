# mars-profiles: CPU profiles without OTLP telemetry

`mars-profiles` is a tracer entry point in `libgstoteltracer.so`, built with the
`profiles` Cargo feature. It uses the same embedded sampler, bounded buffers,
background symbolization, and Pyroscope uploader as correlated profiling in
`mars-otel` and `mars-instrument`. It emits CPU profiles only: no OTLP requests,
span capture, buffer trace metadata, metric series, or GStreamer log bridge.

The current sampler supports Linux x86-64 and requires GStreamer 1.22 or newer.
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
| `GST_OTEL_PROFILE_URL` | `http://localhost:4040` | Pyroscope base URL |
| `GST_OTEL_PROFILE_HZ` | `100` | CPU samples per second per registered thread; 1–1000 |
| `GST_OTEL_PROFILE_ELEMENTS` | `true` | Group native stacks by element execution context or task owner |
| `GST_OTEL_PROFILE_SIGNAL` | `SIGRTMIN()+6` | Reserved realtime signal |
| `OTEL_SERVICE_NAME` | SDK-detected service name | Pyroscope `service_name` |

Selecting `mars-profiles` enables profiling even when the URL is unset. In
`mars-otel` and `mars-instrument`, the URL remains required to opt into profiles.
Trace, metric, and log configuration is ignored by `mars-profiles`, including
`GST_INSTRUMENT_PROFILES`. Invalid profiler settings disable the tracer with an
error; they do not enable another telemetry signal.

Select one of `mars-profiles`, `mars-otel`, or `mars-instrument`. They share one
process-wide configuration; the first constructed instance selects the signals
and installs hooks. Later instances log a warning and install no hooks. For
profiles correlated with traces, select `mars-otel` or `mars-instrument` and set
`GST_OTEL_PROFILE_URL`.

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

Attribution does not recover missing native symbols or follow element-owned
private worker pools. Events, queries, and arbitrary callbacks have no separate
scope hooks; they retain the surrounding call/task context. Task-owner names
are captured at ENTER; chain descriptions refresh after name/parent changes.

## Sampling and shutdown

With element attribution enabled, GStreamer task ENTER registers a native
thread before its first buffer, including source production. Push, buffer-list,
chain, and pull hooks also register participating threads. Sampling then includes
CPU between calls until thread exit or shutdown, for up to 64 concurrent threads.
Threads that never reach these hooks, work before registration, and sleeping
time are not sampled. With attribution disabled, profiles-only registration
starts at the first push, buffer-list push, or pull-range call. Samples carry
native `thread_id` labels, without trace or span IDs.

The context stack is bounded at 256 calls per thread; immutable descriptions are
bounded at 4096 per process, with names derived from paths capped at 512 characters
and factories at 128 characters. Description IDs are never recycled, so already
queued samples keep their original names. Overflow becomes unattributed. The
signal handler reads only an atomic integer; all name lookup and synthetic frame
construction happen outside signals. No GStreamer objects are retained by the
description registry.

Install debug symbols and build relevant native libraries with frame pointers
for useful stacks. The Rust flag above only changes Rust code. The
[native build flags](gstreamer/native.flags.ini) can be passed to GStreamer's
Meson build with `--native-file`; the shared sampler's
[unwinding limits and memory bounds](README.md#experimental-correlated-cpu-profiles)
also apply here.

Profiles upload approximately once per second. Stop streaming, then call
`gstoteltracer::shutdown(timeout)` to stop timers and drain the final profile.
Shutdown uses the same bounded deadline as correlated profiling, can be retried
after a timeout, and cannot restart sampling. Pipeline NULL transitions and
`flush()` do not drain profiles. `gst-launch-1.0` relies on periodic uploads and
may lose the final interval when exiting.

`gstoteltracer::profile_stats()` exposes sampling, registration, processing,
and upload-queue counters. `element_samples` and `task_samples` count accepted
samples with those contexts; `context_capacity_rejections` counts descriptions
rejected by the registry limit. Trace and metric counters remain zero. The SDK does
not expose delivery acknowledgements; a drained upload queue does not prove
successful server ingestion.

## Measured overhead

The [1080p video comparison](../../reports/otel/PROFILE_VIDEO_RESULTS.md)
measures profiles-only against an untraced baseline and `mars-otel` with profiles
and logs. It includes CPU work, peak memory, delivery checks, and the observed
timing variability. Results use 100 Hz sampling and the stock native libraries;
different workloads, sample rates, and debug symbols can change the cost.
Those measurements predate element attribution. The
[element review](../../reports/otel/PROFILE_ELEMENT_REVIEW.md) includes a matched
attribution-on/off check.

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
