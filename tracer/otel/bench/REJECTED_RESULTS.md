# Rejected-push overhead and GStreamer dispatch

Plugin baseline: `8e0f524`. Branch: `impl/otel-compact-capture`.

The retained plugin change lowers single-stream 0%-sampled source CPU from
**44.95 to 42.87 ms (4.6%)**. A separate GStreamer prototype lowers the same
metric by **25.6% at one stream** and **20.6% at four streams** in matched private
core builds. The larger improvement requires rebuilding GStreamer; this plugin
continues to work with the installed core library.

## Retained plugin change

Use a constant initializer for capture TLS and resolve it once per begin hook.
Reuse that reference for the initial stack check, sampled thread information,
and final frame insertion. Each `RefCell` borrow still ends before GStreamer
calls, preserving reentrant callbacks and optional native log correlation.

No sampling decision, token lifetime, frame limit, counter, metadata propagation,
queue, export format, or shutdown behavior changes. Rejected pushes still avoid
recording/export work and carry the information needed across queues and copies.

## Plugin-only comparison

Installed GStreamer 1.24.2 on both sides. Median milliseconds, seven repetitions,
10000 buffers and five pushes each. Source CPU includes ordinary pipeline work;
process CPU includes completed export drain.

| Streams | Mode | Source CPU before | Source CPU after | Process CPU before | Process CPU after |
| --- | --- | ---: | ---: | ---: | ---: |
| 1 | Disabled | 20.89 | 20.92 | 21.76 | 21.82 |
| 1 | 0% | 44.95 | 42.87 | 45.97 | 44.00 |
| 1 | 10% | 50.71 | 48.08 | 62.20 | 59.44 |
| 1 | 100% | 58.87 | 56.09 | 98.76 | 94.88 |
| 4 | Disabled | 30.80 | 30.39 | 32.62 | 32.25 |
| 4 | 0% | 52.20 | 50.53 | 54.50 | 52.85 |
| 4 | 10% | 59.75 | 58.08 | 67.97 | 66.29 |
| 4 | 100% | 70.59 | 69.82 | 112.41 | 108.92 |
| 16 | Disabled | 57.88 | 56.98 | 63.42 | 62.21 |
| 16 | 0% | 66.53 | 65.51 | 73.71 | 72.34 |
| 16 | 10% | 79.99 | 74.48 | 92.28 | 86.24 |
| 16 | 100% | 86.57 | 85.83 | 132.47 | 131.01 |

The 0% source CPU reductions are 4.6%, 3.2%, and 1.5%. The small 16-stream
difference is comparable to movement in its disabled control. An earlier
five-repetition series also favored this change at 0%; fully sampled gains
varied between series. Do not treat small differences as precise guarantees.

## Optional GStreamer prototype

In the original 1.24.2 dispatcher, enabling any tracer makes each executed
tracepoint take a timestamp and perform hook-table lookups, including tracepoints
without listeners. See the
[upstream dispatcher](https://raw.githubusercontent.com/GStreamer/gstreamer/1.24.2/subprojects/gstreamer/gst/gsttracerutils.h).

[The prototype patch](gstreamer-1.24.2-hook-interest.patch) records registered
hook IDs in a 64-bit interest mask. A tracepoint checks its bit before obtaining
a timestamp or looking up callbacks. A wildcard registration sets all bits;
specific and wildcard callbacks retain the existing dispatch loops. Teardown
clears the mask. Registration retains the existing requirement to happen before
streaming; the mask does not introduce support for concurrent hook registration.
A compile-time assertion ensures all hook IDs fit the mask.

This is an optional core patch against the official 1.24.2 release tarball,
not a plugin dependency override. Both comparison cores were built from that
tarball with the same compiler/options and use the **same updated tracer plugin**.
The benchmark selects each private library with `LD_LIBRARY_PATH`. The standalone
callback executable's dynamic linkage was checked against both private libraries.

Median milliseconds, five repetitions:

| Streams | Mode | Source CPU before | Source CPU after | Process CPU before | Process CPU after |
| --- | --- | ---: | ---: | ---: | ---: |
| 1 | Disabled | 20.17 | 20.44 | 20.93 | 21.26 |
| 1 | Empty hooks | 35.02 | 23.70 | 36.03 | 24.42 |
| 1 | 0% | 43.24 | 32.17 | 44.20 | 33.08 |
| 1 | 10% | 48.87 | 36.36 | 59.62 | 46.26 |
| 1 | 100% | 56.63 | 44.60 | 94.93 | 81.79 |
| 4 | Disabled | 30.71 | 30.55 | 32.48 | 32.28 |
| 4 | Empty hooks | 42.31 | 33.44 | 44.38 | 35.29 |
| 4 | 0% | 51.29 | 40.75 | 53.79 | 42.99 |
| 4 | 10% | 58.63 | 50.22 | 67.04 | 57.51 |
| 4 | 100% | 70.22 | 61.50 | 108.61 | 100.48 |
| 16 | Disabled | 56.72 | 50.48 | 62.07 | 56.53 |
| 16 | Empty hooks | 57.49 | 54.20 | 63.38 | 59.46 |
| 16 | 0% | 66.55 | 61.85 | 73.55 | 67.74 |
| 16 | 10% | 79.62 | 76.20 | 91.78 | 87.09 |
| 16 | 100% | 86.01 | 82.10 | 131.26 | 124.59 |

At one stream, the empty-hook increment over disabled falls from 14.85 to
3.26 ms. This strongly supports avoiding work at unobserved tracepoints as a
useful optimization. At sixteen streams the disabled controls differ materially,
so that series does not isolate a reliable tracing-specific gain. These core
results must not be combined arithmetically with the plugin-only measurements;
the system and private builds have different build settings and measurement runs.

## Shared buffers and collector failures

Plugin-only tee/queue comparison, median milliseconds over five repetitions:

| Streams | Mode | Source CPU before | Source CPU after | Process CPU before | Process CPU after |
| --- | --- | ---: | ---: | ---: | ---: |
| 1 | 0% | 49.08 | 46.51 | 121.42 | 115.37 |
| 1 | 100% | 61.33 | 62.35 | 190.45 | 190.66 |
| 4 | 0% | 48.29 | 45.93 | 102.42 | 97.87 |
| 4 | 100% | 61.13 | 60.52 | 165.49 | 162.20 |

The single-stream sampled tee result is effectively flat in process CPU and
slightly slower in source CPU. This plugin change primarily benefits rejection.
Source CPU excludes queue threads in these cases.

The optional core patch also preserves delivery through tee/queue branches.
Across three repetitions per cell, its 0% process CPU medians fall from
116.42 to 95.09 ms at one stream and 100.27 to 81.45 ms at four streams;
fully sampled process CPU falls from 189.99 to 158.44 and 157.91 to 143.48 ms.
These smaller series are supporting observations, not production workload claims.

Every healthy fully sampled run delivers 50000 spans. Healthy runs have zero
failed exports, queue/closed drops, pending records, propagation misses, depth
drops, and hook mismatches. Rejected runs deliver zero and count 50000 rejected
pushes. Metadata-churn runs also preserve full delivery and zero losses.

The 24 failure runs cover slow/absent receivers with both system and patched
GStreamer, using queue capacity 128 and batches of 64. Slow runs deliver 192–256
spans, accounting for the rest as queue drops. Absent receivers deliver zero
and account for all 50000 completed spans as queue drops or failed exports.
All finish with zero pending records and exact completed-span accounting.

## Rejected alternative

[The compact-frame prototype](rejected-frames.patch), applicable to `8e0f524`,
stores an unchanged rejected context once for a nested run. Its isolated warm
capture benchmark improved, but the 90-run pipeline comparison did not show a
consistent gain: single-stream rejected CPU changed from 43.96 to 44.34 ms, and
sampled CPU increased across all three stream counts. That representation change
was removed. The retained TLS change keeps the existing frame/token ownership.

## Validation and limits

- Formatting, workspace all-feature clippy, default tests, OTel tests, and release
  build pass. OTel runs 23 unit tests and 8 collector tests; manual benchmarks
  and the subprocess entry point remain intentionally ignored.
- The same 31 OTel tests pass with the private patched core, including sampling,
  external parent decisions, metadata copies, queue/tee propagation, native log
  correlation, both HTTP adapters, and failure accounting.
- [The standalone callback check](gst-dispatch-check.c) passes against both core
  builds, compiled with `-Wall -Wextra -Werror`. It checks specific listeners,
  multiple listeners, wildcard delivery on otherwise unobserved events, and
  nonzero timestamps.
- Core `gst_gsttracerrecord` and `gst_gstbuffer` suites pass on both private builds.
  Both builds fail the same three threaded cases: `test_link_unlink_threaded`,
  `test_ref_threaded`, and `test_unref_threaded`, with SIGSEGV. Private core tests
  also emit plugin-scanner fallback warnings. The full core test suite was not
  run. These findings limit validation of the optional patch; it is a prototype
  for review, not a claim of a fully qualified GStreamer release.
- `cargo audit` retains 22 vulnerabilities and 15 warnings. Required nightly
  workspace `udeps` still fails in `rustix 0.37.28`; its libc workaround reports
  the existing independent Pyroscope `console`/`pyroscope_pyspy` findings.
  OTel-only nightly `udeps --all-targets` passes.
- Both saved patches pass `git apply --check` against their documented baselines.

Intel i5-1345U, Rust 1.89, GStreamer 1.24.2, release runs pinned to CPUs 4–11.
Variants rotate order; builds and tests did not overlap the measured pipelines.
The shared workstation still permits frequency, cache, and scheduler variation.
Healthy queues hold 65536 records, batches contain 512 spans, logging is disabled,
and the local receiver uses HTTP/1.0. Wall time excludes drain; process CPU
includes it. Exporter ticks are coarse and exclude the blocking HTTP helper.

[Raw measurements](REJECTED_RESULTS.csv) retain all 598 pipeline runs: 90 frame
prototype, 90 TLS pilot, 150 core comparison, 168 final plugin comparison,
40 plugin tee, 12 churn, 12 plugin failure, 24 core tee, and 12 core failure runs.

## Reproduction

Save a release plugin from `8e0f524`, build this branch, and run:

```sh
taskset -c 4-11 python3 tracer/otel/bench/run.py \
  --baseline /path/to/8e0f524/plugin --baseline-sampling --baseline-disabled \
  --streams 1 4 16 --modes none unsampled partial sampled \
  --repeats 7 --drain-ms 0 --output rejected-final.json
```

For the optional core comparison, unpack the official
[GStreamer 1.24.2 tarball](https://gstreamer.freedesktop.org/src/gstreamer/gstreamer-1.24.2.tar.xz)
into two private source directories and apply the included patch to one. Both
were configured with Meson 1.3.2 and Ninja 1.11.1, GCC, and these identical options:

```sh
meson setup /path/to/build /path/to/source --prefix=/usr \
  --libdir=lib/x86_64-linux-gnu --buildtype=release --auto-features=disabled \
  --wrap-mode=nodownload -Dcheck=enabled -Dtests=enabled
ninja -C /path/to/build -j 4
```

The downloaded archive's SHA-256 is
`9cafdd23bd180f1681c56cd3a6879a8497ccf24da6f422a6b6f356fa074a8481`.

For this uninstalled layout, each `build/gst/gstreamer-1.0` is a symlink to the
system plugin directory from `pkg-config --variable=pluginsdir gstreamer-1.0`.
This handles the core library's relocated plugin lookup. Build tools were unpacked
under `target/perf-review/gst-dispatch`; no system core installation is needed.
Compile `gst-dispatch-check.c` using the system GStreamer `pkg-config` flags, then
run it with `LD_LIBRARY_PATH` pointing to each private `build/gst` directory.

```sh
taskset -c 4-11 python3 tracer/otel/bench/run.py \
  --baseline target/release --baseline-gst-library-path /path/to/before/gst \
  --gst-library-path /path/to/after/gst \
  --baseline-sampling --baseline-disabled --baseline-hooks \
  --streams 1 4 16 --modes none hooks unsampled partial sampled \
  --repeats 5 --drain-ms 0 --output rejected-gst-dispatch.json
```

Here `--baseline target/release` deliberately selects the same updated plugin
for both variants. Use `--tee-queues`, `--churn-every 10`, and `--modes slow absent`
for the supporting cases.
