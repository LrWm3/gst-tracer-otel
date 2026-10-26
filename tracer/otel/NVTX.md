# Optional NVTX worker attribution

NVTX ranges let Mars identify the element that owns work on a private inference
thread. This is an attribution bridge, independent of the profile source:

| Build feature | Profile sources | NVTX worker attribution |
| --- | --- | --- |
| `profiles` on Linux | CPU | Supported; no CUPTI or CUDA headers needed by the adapter |
| `gpu-profiles` on Linux | CPU, GPU, or both | Supported with the same adapter |
| Neither | No profiles | Property unavailable |

The bridge currently recognizes `nvinfer` domains and domain push/pop ranges.
It does not collect GPU kernel activities. With CPU-only profiling, it registers
a worker for CPU sampling at its first recognized range, then attributes those
samples to the owning element. GPU collection remains off.

## Enable it with a tracer property

Build the CPU profiler and the small native NVTX adapter:

```sh
cargo build --release -p gst-otel-tracer --features profiles
sh tracer/otel/native/build-nvtx-adapter.sh target/libmars-nvtx.so
export GST_PLUGIN_PATH="$PWD/target/release${GST_PLUGIN_PATH:+:$GST_PLUGIN_PATH}"
export NVTX_INJECTION64_PATH="$PWD/target/libmars-nvtx.so"
export GST_TRACERS="mars-profiles(profile-sources=cpu,profile-nvtx-adapter=\"$NVTX_INJECTION64_PATH\")"
export MARS_PROFILE_URL=http://localhost:4040
export OTEL_SERVICE_NAME=my-video-pipeline
# Start the application's nvinfer pipeline here.
```

`profile-nvtx-adapter` is an optional, construction-time string property on
`mars-profiles`, `mars-otel`, and `mars-instrument`. Its default is unset (off).
Omit it to disable NVTX attribution while keeping ordinary GStreamer element
attribution. Selecting `profile-sources="cpu"` keeps an explicitly configured
NVTX bridge active with CPU sampling alone.

For correlated telemetry, use the same property in
`mars-instrument(signals="traces,profiles",profile-sources="cpu",profile-nvtx-adapter="/absolute/path/libmars-nvtx.so")`.
The property does not enable the `profiles` signal or GPU collection by itself.
It is ignored when the selected signals omit profiles. Configuration is fixed
at construction, so restart the process to change it.

Applications can set the property through the GObject API too:

```rust
let tracer = glib::Object::builder::<gstoteltracer::ProfilesTracer>()
    .property("profile-sources", gstoteltracer::ProfileSources::Cpu)
    .property("profile-nvtx-adapter", "/absolute/path/libmars-nvtx.so")
    .build();
```

A null or empty GObject string disables the bridge. When both `params` and the
direct property specify an adapter, `params` takes precedence. Explicitly typed
empty strings in `params` use `profile-nvtx-adapter=(string)""`; older GStreamer
parsers can otherwise interpret an empty quoted value as integer zero.
The earlier `MARS_PROFILE_NVTX_ADAPTER` environment selector is no longer read.

## Why one NVIDIA environment setting remains

`NVTX_INJECTION64_PATH` is NVIDIA's library-loading mechanism. Set it **before
starting the application**, pointing to the same file as `profile-nvtx-adapter`.
NVTX initializes its callback table once; changing the environment after a
library has used NVTX cannot reliably install a new injection. See the
[NVIDIA initialization implementation](https://github.com/NVIDIA/NVTX/blob/v3.2.1/c/include/nvtx3/nvtxDetail/nvtxInit.h#L403-L429).

Mars checks that both paths resolve to the same library and reports a mismatch
through `GST_DEBUG=mars-otel:3`. It does not change the process environment.
The NVIDIA setting alone does not opt Mars into attribution. The injection slot
cannot be shared with another NVTX profiler such as Nsight.

## Attribution and limits

The bridge binds an NVTX domain created during `nvinfer`'s READY → PAUSED
transition to that exact element. On the worker, domain push/pop ranges install
and restore the shared profiling context. It does not infer ownership from
thread names or globally match `unique-id` values: different elements and
pipelines may reuse them. CPU sampling registers a participating worker at its
first recognized range; GPU-only mode creates no CPU timers. CPU work outside
the range remains unattributed unless another GStreamer scope owns it.

This provides `element → CPU stack` and `element → GPU kernel` attribution.
**It does not establish per-batch trace links.** A worker range has no submitting
span unless that same element already has an active synchronous call on the
thread. Unrelated spans are suppressed, and asynchronous domain start/end
ranges are not treated as execution scopes. Exact worker trace links still
need a context handoff through the job queue.

Support depends on the plugin emitting `GstNvInfer: UID=...` domains and domain
push/pop ranges at these boundaries. Domains initialized before Mars, domains
created on an unknown thread, and other plugins' private workers are not
automatically attributed. No range messages, batch numbers or kernel names are
used to guess a missing relationship.

The registry caps at 4,096 domains and holds weak element descriptions. Domain
destruction releases entries; expired owners are reclaimed on the next domain
creation. Thread scopes share the existing 256-entry limit. Ambiguous nesting
and overflow suppress attribution. `nvtx_profile_stats()` exposes bound/live
domains, entered ranges and capacity rejections. Signal handlers never read the
domain registry or invoke NVTX.

Implementation is isolated in [nvtxcontext.rs](src/nvtxcontext.rs) and
[nvtx_adapter.c](native/nvtx_adapter.c). Tests exercise the native injection ABI
with two elements sharing an inference ID, overlapping ranges, reused workers,
repeated element lifetimes, and real CPU samples. They also cover the property
API, parameter precedence, disabling, and missing/mismatched injection paths.
These tests require no GPU:

```sh
cargo test -p gst-otel-tracer --features profiles --test nvtx_profiles
cargo test -p gst-otel-tracer --features gpu-profiles --test nvtx_profiles
```

The second command adds mocked GPU activities to check that attribution works
for CPU-only, GPU-only, and combined collection.
