# mars-profiles: CPU profiles without OTLP telemetry

`mars-profiles` is a tracer entry point in `libgstoteltracer.so`, built with the
`profiles` Cargo feature. It uses the same embedded sampler, bounded buffers,
background symbolization, and Pyroscope uploader as correlated profiling in
`mars-otel` and `mars-instrument`. It emits CPU profiles only: no OTLP requests,
span capture, buffer trace metadata, metric series, or GStreamer log bridge.

The current sampler supports Linux x86-64. Build and run:

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

## Sampling and shutdown

The profiles-only hooks register a native thread at its first buffer push,
buffer-list push, or pull-range call. Sampling then includes CPU work on that
thread between those calls, until thread exit or shutdown. Up to 64 concurrent
threads can register. Threads that never perform these operations, work before
registration, and sleeping time are not sampled. Queue workers register when
they perform an operation themselves. Samples carry native `thread_id` labels,
without trace or span IDs.

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
and upload-queue counters. Trace and metric counters remain zero. The SDK does
not expose delivery acknowledgements; a drained upload queue does not prove
successful server ingestion.

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
