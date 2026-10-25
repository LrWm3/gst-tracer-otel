# Linux arm64 profiling validation — 2026-10-23

The embedded profiler now supports Linux AArch64 using the same timers, bounded
sample rings, element/task attribution, background symbolizer, and Pyroscope
uploader as x86-64. Architecture-specific code is isolated in
[profilearch.rs](../../tracer/otel/src/profilearch.rs).

Validation used an integrated memory nvidia device. The host's GStreamer 1.16.3
is below the existing 1.22 minimum. Tests and benchmarks ran natively in a Debian 12 container with
GStreamer 1.22.0 and Rust 1.89.0; the host installation was not changed. No GPU
plugins, inference workloads, or production services were used.

## What changed

- Read the interrupted PC, SP, and x29 from the ARM signal context.
- Walk the existing bounded stack copy using ARM frame records, with the same
  bounds, alignment, cycle, and depth checks as x86-64.
- Strip instruction-pointer authentication signatures using XPACLRI/HINT #7,
  then subtract four bytes from saved return addresses to identify their call
  instructions. Interrupted PCs receive no adjustment. x86-64 keeps its one-byte
  return-address bias.
- Keep native symbolization and pprof construction outside the signal handler.
  No new signal-path locks, allocations, or loader calls were introduced.
- Enable the real profiling tests for ARM and add a native ARM CI job. Add
  ordinary/leaf frame-pointer build flags for native ARM dependencies.

The walker deliberately does not guess callers from live x30. Leaf functions
without frame records and interruptions during prologues/epilogues can still
skip callers. Unreadable or invalid chains retain the interrupted instruction
and element context. Tagged stack pointers, DWARF unwinding, and JIT unwinding
are not added. The [user guide](../../tracer/otel/PROFILES.md#linux-arm64) explains
these limits and build options.

## Native and emulated checks

On an integrated memory nvidia device, the OTel profiles-feature suite passed:

| Suite | Passed |
| --- | ---: |
| Unit tests, including signal/lifecycle stress and concurrent ring transfers | 43 |
| Metrics/instrumentation integration tests | 7 |
| Real CPU-timer/profile integration tests | 9 |
| Trace integration tests | 8 |
| Explicit native caller-chain test with frame pointers | 1 |

The tests cover same-factory element instances, source task ownership, nested
and queued trace correlation, unsampled pushes, buffer lists, pull calls,
shutdown deadlines, loader-lock contention, and 1000 Hz sampling during
allocation/mutex/loader activity. The new caller-chain test requires real samples
containing two known Rust callers in leaf-first order under the correct element
root. It is explicitly run with `-C force-frame-pointers=yes`, since builds that
omit those frames cannot satisfy that check.

Native architecture tests verified the backwards-compatible HINT behavior.
A separate QEMU 7.2.22 `-cpu max` run
exercised **actual changed signatures with both instruction keys A and B** and
verified that stripping recovers each original callsite. This is emulated PAC
instruction coverage, not native PAC-hardware or profiler-timer validation.

The native shared library also loaded through `gst-inspect-1.0 mars-profiles`.
Disassembly confirmed XPACLRI followed by the four-byte callsite adjustment in
the frame walker. The container emitted an external plugin-scanner warning for
both the stock `fakesrc` baseline and this plugin; both factory inspections
succeeded with fresh registries.

The x86 profiles suite and explicit caller-chain test also passed, as did the
non-profiling OTel suite and default workspace tests. Formatting, all-feature
Clippy with warnings denied, and nightly dependency-use checks passed.
`cargo audit` exited zero with the existing allowed Tokio
`RUSTSEC-2026-0386` warning. Dependencies were not changed.

[Validation details and source hashes](arm64/2026-10-23-validation.json) record
the environment and test results. The base revision is `970c35c`; the measured
binaries include the uncommitted implementation from this branch, not stock main
at that revision.

## Pyroscope and element-attribution demo

The existing 1200-frame, 640×360
[controlled demo](../../tracer/otel/examples/profile_elements.rs) ran with
attribution disabled and enabled. It deliberately spends approximately 2 ms and
6 ms of thread CPU per frame in two `identity` instances on different threads.
Uploads were forwarded to a real Pyroscope 2.4.0 server, and its merged query
matched the decoded payload totals exactly.

| Enabled scope | CPU seconds | Samples |
| --- | ---: | ---: |
| Element `heavy` | 7.23 | 723 |
| Element `light` | 2.53 | 253 |
| Task owner `source` | 2.88 | 288 |
| Element `convert` | 1.51 | 151 |
| Element `output` | 0.01 | 1 |
| Element `capsfilter1` | 0.01 | 1 |
| Unattributed | 0.02 | 2 |
| **Total** | **14.19** | **1419** |

The heavy/light ratio was **2.86**, within the test's sampling tolerance around
the intended 3.0. There were no ring, processing, or upload drops, timer overruns,
failed stack reads, truncated chains, or registration failures. All timers were
retired after shutdown. The disabled run also had 1419 samples; its native root
had no element name. These two short demo runs are not an overhead comparison.
See [raw attribution results](arm64/2026-10-23-attribution.json).

## Video CPU and memory check

A separate run used 600 frames of 1920×1080 RGBA noise, conversion to I420, and
an unsynchronized fake sink. Each mode had one discarded warm-up and three
measured repetitions, with rotating order. This used the documented `profiling`
build **with debug information**, frame pointers, 100 Hz sampling, and the stock
container GStreamer libraries. The child ran on CPUs 0 and 2, the receiver on
CPU 3, without a container CPU quota. No compilation or other validation ran
during the measurements. CPU frequency was not pinned (`schedutil`).

| Mode | Median process CPU | Change vs no tracer | Process CPU range | Peak RSS range |
| --- | ---: | ---: | ---: | ---: |
| No tracer | 20.573 s | — | 20.547–20.660 s | 22.85–22.87 MiB |
| `mars-profiles` | 20.833 s | +1.3% | 20.826–20.874 s | 25.88–114.13 MiB |
| `mars-otel` with profiles and logs | 21.213 s | +3.1% | 21.149–21.217 s | 117.93–118.56 MiB |

These are process CPU-time measurements, not hardware-cycle measurements or
FPS-loss estimates. They include final report processing and shutdown. The
logging mode emits one INFO message per frame, uses 100% trace sampling, and
keeps the ordinary GStreamer log handler enabled. Queue capacity is 8192 and
the trace export interval 200 ms. The local receiver acknowledges actual
uploads and validates them; remote storage is outside these timings.

Profile-only memory varied substantially: two measured runs peaked near 26 MiB,
and one at 114 MiB. Use the observed upper value when planning for this build,
not its low median. The profiler's bounds exclude temporary debug-file parsing;
this build retains debug information, unlike the `release` binary used for the
earlier x86 video tables. The result is not a direct ARM-versus-x86 comparison.

All **12,458 CPU samples**, **7200 spans**, and **1800 logs** from measured runs
were received and decoded, with profile/trace correlation validated. There were
no sample drops, timer overruns, failed stack reads, or registration failures.
Seven chains were truncated; the original samples and interrupted PCs were
retained. Raw [CSV](arm64/2026-10-23-video.csv) and
[settings/environment JSON](arm64/2026-10-23-video.json) include those counters,
warm-ups, binary hash, and timings. These short synthetic runs do not establish
overhead for production video decoding, inference, or GPU workloads.

## Deadlock and memory-growth check

These are historical results from before description reclamation. The newer
[30-minute native lifecycle soaks](PROFILE_VIDEO_RESULTS.md#thirty-minute-lifecycle-soaks--2026-10-25)
exercise the current implementation beyond 4096 cumulative descriptions, with
collector outages, native thread churn and shared-library loading. Descriptions
are now reclaimed after queued samples drain; the limit applies to live and
pending descriptions.

A follow-up on an integrated memory nvidia device tested runtime code from
`128b002`. Source hashes matched the checkout. No deadlock or sustained memory growth was observed in these
finite runs; this is not proof that every production workload is free of leaks
or hangs.

Each profiled process ran two approximately 90-second phases of repeated
30-frame, 1080p video pipelines. Between those phases it created and destroyed
**1500 uniquely named tiny pipelines**, exercising the 4096-description limit.
Every pipeline's weak reference had to expire within one second of teardown.
An untraced control used two 30-second video phases and the same 1500-pipeline
churn. All processes completed. The two profiled runs processed **35,841 CPU
samples**, with no ring, processing, or upload-queue drops and no registration
failures or failed stack reads.

| Mode | Video pipeline cycles, in addition to 1500 tiny pipelines | Late-run median RSS | Peak RSS | RSS after shutdown | Shutdown |
| --- | ---: | ---: | ---: | ---: | ---: |
| No tracer | 58 | 37.7 MiB | 38.0 MiB | 37.7 MiB | — |
| `mars-profiles` | 172 | 84.7 MiB | 139.6 MiB | 45.8 MiB | 10.5 ms |
| `mars-otel` with profiles and logs | 171 | 71.9 MiB | 125.5 MiB | 33.5 MiB | 13.4 ms |

These are **whole-process RSS observations from one run per mode**, not an
overhead comparison or a ranking of the modes' memory requirements. The
profiled runs' late-run medians cover the last 60 seconds of video processing;
the shorter control covers its final 30-second phase. Profiles-only RSS ranged
from 84.5 to 84.7 MiB over that final minute; OTel stayed at 71.9 MiB at the
pipeline-boundary observations. Threads and file descriptors returned to the
control's idle counts: two threads and three descriptors. Sampling timers were
all stopped.

After shutdown and ten idle seconds, a diagnostic `malloc_trim(0)` reduced RSS
further to 36.4 MiB for profiles-only and 21.5 MiB for OTel. The application does
not call this in production. It demonstrates that allocator retention accounts
for part of the measured footprint. Sampling buffers and element descriptions
deliberately remain allocated for process lifetime, subject to their bounds;
temporary debug parsing and report construction are outside the symbol-cache
budget.

The deliberate description churn caused 1264 and 1259 context-capacity
rejections, respectively; new descriptions become unattributed once the registry
is full. There were three and eight timer overruns, respectively, and 27
truncated chains in the OTel run. Truncated samples retained the interrupted PC.
These counters are recorded alongside the zero queue-drop counts rather than
treating every attempted CPU tick as a delivered sample.

Separately, **60 native stress checks passed**: ten repetitions each of
1000 Hz sampling during allocation/mutex/loader activity and thread retirement,
symbolization while the loader lock is held, full/disconnected upload queues,
concurrent ring transfers, and both tracers' slow-receiver shutdown tests.
Those checks ran concurrently on CPU 1 in a separate container with a one-CPU
quota. The soak container had a two-CPU quota, with video children pinned to
CPUs 0 and 2. These runs assess progress and memory behavior; they are not CPU
performance measurements.

The soak used the same native container environment and `profiling` build with
frame pointers, 100 Hz sampling, default trace export settings, and one INFO log
per video frame in OTel mode. Its local HTTP receiver discarded request bodies
after acknowledgement, keeping receiver memory independent of run duration.
Unlike the earlier video benchmark, this check did not decode every upload or
verify remote Pyroscope ingestion.

[Summary and counters](arm64/2026-10-23-lifetime/summary.json), per-process RSS
series ([control](arm64/2026-10-23-lifetime/none.csv),
[profiles](arm64/2026-10-23-lifetime/profiles.csv),
[OTel](arm64/2026-10-23-lifetime/otel.csv)), and
[stress-test results](arm64/2026-10-23-lifetime/stress.json) preserve the evidence.
The exact one-off [driver](arm64/2026-10-23-lifetime/profile_soak.rs),
[receiver/monitor](arm64/2026-10-23-lifetime/run.py), and
[stress runner](arm64/2026-10-23-lifetime/stress.py) are archived with their hashes
in the summary. The driver was temporarily copied to
`tracer/otel/examples/profile_soak.rs` and built with
`cargo build -p gst-otel-tracer --profile profiling --features profiles --example profile_soak`;
the scripts ran inside the `/work`-mounted ARM container after the native test
binaries had been built. They assume CPUs 0–2 and the paths shown in the scripts.

## Reproduce

On a native ARM machine with Docker, from the repository root:

```sh
docker build -t mars-arm64-review -f tracer/otel/bench/arm64/Dockerfile .
docker run --rm --network host \
  -v "$PWD:/work" -v mars-arm64-review-cargo:/usr/local/cargo/registry \
  -e GIT_CONFIG_COUNT=1 -e GIT_CONFIG_KEY_0=safe.directory -e GIT_CONFIG_VALUE_0=/work \
  mars-arm64-review sh -ec '
    cargo test --locked -p gst-otel-tracer --features profiles
    cargo test --locked -p gst-otel-tracer --features profiles --test profiles \
      native_frame_chain_retains_known_callers_under_an_element_root -- --ignored --nocapture
    python3 tracer/otel/bench/arm64/check_pac.py
    cargo build --locked --profile profiling -p gst-otel-tracer --features profiles --lib --examples
  '
```

The container sets frame pointers and limits Cargo to two build jobs. Its network
mode lets a host-loopback Pyroscope endpoint be used by the demo. The validation
ran with Docker's default seccomp profile and without extra capabilities.

For the attribution demo, run inside that environment with a reachable Pyroscope:

```sh
python3 tracer/otel/bench/element_profiles.py \
  --binary target/profiling/examples/profile_elements \
  --pyroscope http://127.0.0.1:44040 \
  --output target/arm64-review/attribution.json

taskset -c 3 python3 tracer/otel/bench/profile_video.py \
  --binary target/profiling/examples/profile_video --build-profile profiling \
  --cpus 0,2 --frames 600 --repeats 3 \
  --modes none profiles otel-profiles-logs \
  --export-interval-ms 200 --queue-capacity 8192 \
  --output target/arm64-review/video.csv
```

The [ARM CI workflow](../../.github/workflows/profiles-arm64.yaml) runs real timer,
attribution, and caller-chain tests on a native `ubuntu-24.04-arm` runner, then
tests PAC instructions separately under QEMU. It does not use emulation for the
native sampler tests or performance measurements.
