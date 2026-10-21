# Tracer overhead on an integrated memory nvidia device — 2026-09-05

The reviewed tracer showed **no material processor-throughput regression** in
the short matched checks. The final build delivered 125.41 FPS versus 125.78 FPS
with tracing off (−0.29%); an earlier reviewed build averaged 125.91 FPS versus
125.78 FPS off. These small differences do not establish a speedup or a
repeatable slowdown. The synthetic buffer-only loop does show additional
instrumentation cost. Final-build RTSP throughput was 120.08 FPS.

## Setup

Measurements used an integrated memory nvidia device.
The installed processor and tracer were not replaced. Existing monitoring
services remained running. All measured runs use the same locally supplied benchmark container image.

Tracer variants:

- **Off:** no active tracer.
- **Original:** `0f7504c`, before the new diagnostics.
- **Pre-review:** `19a07e4`, new diagnostics before ownership/timing fixes.
- **Reviewed, initial capacity build:** review fixes plus construction guard,
  `53645e4`.
- **Reviewed, final build:** also preserves floating RTSP pad ownership,
  `b48f429`.

For the processor tests, the pre-review version also receives the same tiny
queue-type construction guard. Its other behavior is unchanged. Unguarded
pre-review and reviewed builds crash during DeepStream graph construction, so
their failed launches are not throughput measurements.

Common configuration: every element selected (`*`), histogram sample rate 16,
PTS age disabled, queue sampling every 100 ms where supported. Traced runs have
an HTTP scrape every second. The original tracer does not implement CPU/blocked
histograms or queue metrics; this is an overall feature-cost comparison, not a
claim that it exports the same information. The pre-review/reviewed comparison
isolates the review changes more closely.

Each variant/workload has its own GStreamer registry, populated inside the
NVIDIA-enabled runtime before measuring the processor. Plugin inspection verifies
the loaded filename. Binaries have independent recorded SHA-256 identities.

## Synthetic result

One million buffers through `fakesrc ! identity ! fakesink`, two synchronous
push edges. One discarded warm-up per variant, followed by five measured runs
with rotated ordering. Child CPU seconds are measured directly with `getrusage`.
These measurements precede the construction guard, which does not change the
per-buffer path; they are not a fresh synthetic qualification of `53645e4`.

| Variant | Median elapsed seconds | Median child CPU seconds |
| --- | ---: | ---: |
| Off | 1.272 | 1.233 |
| Original | 2.723 | 2.680 |
| Pre-review | 3.026 | 2.972 |
| Reviewed, before construction guard | 3.226 | 3.194 |

Review fixes add about 0.200 seconds elapsed and 0.222 CPU seconds over two
million push edges: approximately 0.100 microseconds elapsed / 0.111 CPU
microseconds per edge. This buffer-only loop is intentionally sensitive to
instrumentation overhead. Its 6.6% elapsed increase over pre-review must not be
interpreted as a 6.6% loss in video-processing throughput.

## Processor capacity

The repeated-file full Rust path uses the established 1080p configuration:
55 decoder extras, 80 owned block-linear copy surfaces, mux pool 4, B16, three
asynchronous inference slots, GPU preprocessing, persistent RGBA hash/IQ,
tracker and non-publishing terminal output. Each run lasts 60 seconds in PLAYING;
the steady window begins ten seconds after the first terminal buffer. Two runs
per variant rotate order. FPS and tails come from the existing terminal probe,
not from the tracer being evaluated.
The table below uses `53645e4`; the subsequent floating-pad correction is
covered by the final-build checks below, not silently treated as the same binary.

Results are recorded under `/home/user/tracer-perf-46efcd7/capacity-guard-*`.

| Variant | Warmed FPS, run 1 / run 2 | Mean FPS | Mean child CPU seconds, including startup |
| --- | ---: | ---: | ---: |
| Off | 125.901 / 125.661 | 125.781 | 91.092 |
| Original | 125.564 / 125.427 | 125.496 | 92.173 |
| Pre-review + construction guard | 125.372 / 125.686 | 125.529 | 92.041 |
| Reviewed | 125.994 / 125.825 | 125.910 | 91.755 |

Reviewed versus off is +0.10%; reviewed versus pre-review is +0.30%. With only
two runs per variant these are effectively tied, not statistically established
improvements. Every run exceeds the 121 FPS gate. Process CPU measurements
include initialization and teardown, so they are not steady-state CPU load or
a precise attribution of sub-percent overhead.

Independent terminal-probe B16 interval tails:

| Variant | p99 milliseconds, run 1 / run 2 |
| --- | ---: |
| Off | 195.319 / 171.134 |
| Original | 173.879 / 151.155 |
| Pre-review + construction guard | 144.593 / 186.046 |
| Reviewed | 174.328 / 190.273 |

Tails vary substantially between runs; there is no consistent reviewed-versus-off
worsening here. These short samples cannot qualify p99.9 behavior or rare stalls.
Traced runs successfully served 63–65 scrapes each. Maximum scrape payload fell
from about 1.31 MB pre-review to 1.00 MB reviewed, mainly because disabled PTS
histogram series are no longer allocated. These are uncompressed local payloads.

## Final-build capacity check

After fixing floating-pad ownership, the final `b48f429` binary and tracer-off
each ran the same 60-second capacity workload once, with the same ten-second
warm-up exclusion. Both exited cleanly. Files: `capacity-final-*`.

| Variant | Warmed FPS | B16 interval p99, ms | Child CPU seconds, including startup |
| --- | ---: | ---: | ---: |
| Off | 125.779 | 184.514 | 91.227 |
| Reviewed, final | 125.409 | 149.603 | 93.111 |

The final build is 0.369 FPS / 0.29% below its off comparator, within the range
of the earlier matched run-to-run results. This one pair cannot establish a
sub-percent overhead estimate. Both exceed 121 FPS; the final traced run's
minimum rolling five- and ten-second output was 124.8 FPS. Its lower p99 is also
not evidence of a tail-latency improvement from this single pair. CPU seconds
include startup/teardown and must not be interpreted as steady CPU utilization.
The endpoint served 65 successful scrapes during the final capacity run.

## Two-stream check

The separate RTSP harness uses two locally paced stream-copy publishers and
source-local H.264 metadata joins, without source decode/re-encode or cloud
publishing. This is a camera-paced check, not a headroom measurement. Its
parameters and service cleanup are in `run_device_rtsp_matched.sh`.

One 90-second run per variant, ten-second warm-up, B16, mux pool 8,
55 decoder extras, 80 block-linear copy surfaces, 4-second join deadline.
The two publishers use the same sample, independently paced; this does not
exercise different scene content per source. Final reviewed binary: `b48f429`.

| Variant | Warmed aggregate FPS | B16 interval p99, ms | Process exit | Successful scrapes |
| --- | ---: | ---: | --- | ---: |
| Off | 120.123 | 138.299 | SIGSEGV after result output | 0 |
| Pre-review + construction guard | 120.209 | 137.963 | Clean | 93 |
| Reviewed, final | 120.082 | 137.688 | Clean | 92 |

The off run produced a complete runtime-limited JSON result, then exited with
SIGSEGV. Its in-run counters are shown for transparency, **not as a clean
qualification pass**. The exact failure cause has not been traced; tracing was
disabled, so it cannot be assigned to active tracer hooks. Both traced runs
completed cleanly. The revised build sustained the two-stream arrival rate,
with no observed p99 deterioration in this short check. This is not a long soak.

Every metadata frame received by the join matched: 5,296 per source pre-review,
5,288 per source reviewed. Wrong-source, duplicate, late and pending metadata
counters were zero. This does **not** mean every encoded frame was annotated:
passthrough counts were 25 per source pre-review, 26 reviewed, and 32 off.
Those passthroughs were not separately diagnosed by this overhead experiment.
The revised metric endpoint served 92 scrapes, with maximum payload 681,684
bytes versus 890,718 pre-review (about 23% smaller).

## Failure investigation

An initial attempt reused synthetic registries built without NVIDIA runtime
mounts; those registries had blacklisted DeepStream plugins. These attempts were
discarded and registries are now workload-specific.

Fresh registries exposed a separate tracer crash. GDB stopped in
`g_type_check_instance_is_fundamentally_a` → `g_object_ref_sink` → tracer
`element_new`, called during `gst_element_constructed`. Queue identification
called `element.factory()` too early. Checking the already-valid GType avoids
that factory dereference. A queue-type regression test was added.

The initial reviewed RTSP build then crashed in `remove_event_by_type`, while
`rtspsrc` was pushing a sticky event. The review's raw-pad ownership cleanup had
used `from_glib_none`, which sinks a floating GObject rather than always taking
an additional reference. For an RTSP pad not yet parented, dropping that wrapper
could destroy the C caller's pad. The final build borrows incoming pads and uses
a normal clone only for owned traversal. A regression verifies reference count
and floating state remain unchanged. Eighteen local tracer tests and workspace
Clippy pass. Failed RTSP launches are excluded from performance results.

Shared Cargo target directories also produced identical copied binaries during
staging. SHA verification caught this before the guarded measurements. The
reviewed build was rebuilt in an independent target directory.

## Reproduction and artifacts

- `bench_matched.py`: rotated variants, exact commands, plugin hashes, subprocess
  elapsed/CPU time, scrape counts and logs.
- `run_device_matched.sh`: full-path capacity test.
- `run_device_rtsp_matched.sh`: temporary stream-copy relay and two-stream test.
- Raw results/logs on an integrated memory nvidia device: `/home/user/tracer-perf-46efcd7/`.
- Native crash traces: `startup-gdb.txt` and `rtsp-gdb.txt` in that directory.

Final reviewed tracer SHA-256:
`4f1a6ee285c97d8292f63733a4c60112262be20b2037da3e2c3d3ec2c2a6f1fa`.

All temporary benchmark/RTSP/debug containers are stopped. The installed
processor and tracer remain unchanged; existing monitoring remains running.

Processor plugin SHA-256:
`2490abe2803a184ece603dde775d89daf7b1536bdca818b247b697fe5ec769ea`.
TensorRT FP16 engine (`model_b16_gpu0_fp16.engine`) SHA-256:
`6069c4c4afdfa95424e4c8cd5e45d9b6afb6373364fe19823830542535fd972b`.
Repeated input (`test_live_stream.mp4`) SHA-256:
`e3dee02187aa661e9b171a38dfe4c5b66299bb846730310a693e36d8a2b46246`.

## Local follow-up: topology labels and buffer-path cost — 2026-10-20

This follow-up compares the rebased baseline `e2b5fed` with the working-tree
fixes. It preserves the queued ownership/diagnostic changes and the merged
histogram buckets. Both variants export the same metrics with the same settings.
The candidate resolves labels lazily, invalidates them on topology changes,
uses a bounded 64-slot thread cache, and moves cache ownership out of completed
spans without the extra reference-count operation or repeated stack scans.

Host: Intel Core i5-1345U, x86_64, GStreamer 1.24.2, Rust 1.89.0, release builds.
Each child process is pinned to logical CPU 2 (a performance core). Each variant
has its own prepopulated plugin registry; plugin inspection checks the loaded
path, and the raw results record binary SHA-256 hashes. One warm-up per variant
is discarded, followed by seven measured rounds with rotating off/before/after
ordering. All elements are selected, PTS age is disabled, and traced runs are
scraped once per second. No compilation ran during these measurements.

The linear workload processes 1,000,000 buffers through one identity; the deeper
workload processes 250,000 buffers through eight identities. Both use `fakesrc`
and `fakesink sync=false`. Values below are median child-process CPU microseconds
per source buffer, including GStreamer and startup/teardown, not tracer-only CPU.

| Workload | Histogram sample rate | Before CPU µs/buffer | After CPU µs/buffer | Change |
| --- | ---: | ---: | ---: | ---: |
| One identity (2 pushes/buffer) | 16 | 1.429 | 1.393 | -2.49% |
| One identity (2 pushes/buffer) | 1 | 2.163 | 2.146 | -0.76% |
| Eight identities (9 pushes/buffer) | 16 | 5.930 | 5.820 | -1.86% |

The sampled workloads use about 1.9–2.5% less CPU per buffer. Observing every
buffer is approximately unchanged (0.8% lower median CPU). These are local
synthetic measurements on a shared desktop; they do not establish an integrated memory nvidia device or
production-video speedup. Wall times are also recorded but include process-wait
polling overhead, so CPU is the primary comparison.

Raw observations, commands, build identities and workload metadata are in
[`benchmarks/2026-10-20-local.json`](benchmarks/2026-10-20-local.json).
Reproduce with two plugin directories named `plugins-before` and `plugins-after`:

```sh
python3 tracer/prometheus/bench_matched.py --root /path/to/plugins \
  --variants off,plugins-before,plugins-after --rounds 7 --sample-rate 16 \
  --name linear16 -- taskset -c 2 gst-launch-1.0 -q \
  fakesrc num-buffers=1000000 ! identity name=work0 ! fakesink sync=false
```

Repeat with `--sample-rate 1 --name linear1`; the raw commands include the deeper
workload. Choose an available performance core on the machine being measured.

### Follow-up validation

- `cargo fmt --all`: passed.
- `cargo clippy --workspace --all-features --all-targets -- -D warnings`: passed.
- `cargo test`: passed, 13 unit tests and 11 integration tests.
- AddressSanitizer: all 13 unit tests passed, including concurrent cache
  replacement, stale unlink protection, thread-cache invalidation and ownership.
  Command: `RUSTFLAGS='-Zsanitizer=address' cargo +nightly test
  -p gst-prometheus-tracer --lib --target x86_64-unknown-linux-gnu`.
- `cargo audit`: reports 22 existing advisories. Auditing the baseline lockfile
  produces the identical set of advisory IDs, package names and versions. No
  dependency versions were changed by this work.
- The required workspace `cargo +nightly udeps ... --all-targets` fails compiling
  the existing `rustix 0.37.28` on current nightly (reserved `rustc` attributes).
  Repeating with `RUSTC_BOOTSTRAP=1 cargo +1.89.0 udeps
  -p gst-otel-tracer -p gst-prometheus-tracer -p gst-pyroscope-tracer --all-targets`
  completes analysis: Prometheus is clean; Pyroscope has existing unused
  `console` and `pyroscope_pyspy` dependencies.

## Local follow-up: I/O, pull timing and metric retirement — 2026-10-20

Compared with `9b7eeaf`, this build adds opt-in per-pad I/O counts, exact request-pad
labels, exclusive pull timing, queue label refresh, and retirement of series for
destroyed objects or old paths. Default push counters retain their semantics.
I/O counts are disabled unless `GST_PROM_LATENCY_IO_COUNTS=true`.

The streaming thread uses local reference counts and a local sampling sequence;
shared qdata ownership remains atomic. A 64-slot direct map serves common hits,
with a capped 256-entry pool reusing snapshots when slots collide. Cache hits
allocate nothing and perform no GObject traversal or metric-registry lookup.
Sampling continues across evictions. Queue property reads and periodic series
cleanup run outside the buffer callbacks; cache misses still acquire metric handles.

Same i5-1345U host, Rust 1.89.0, GStreamer 1.24.2, release builds, logical CPU 2.
One warm-up and seven measured rounds per variant, rotating order, separate
prepopulated registries, verified plugin paths, and a scrape every second.
No compilation ran during measurements. All elements are selected and PTS age
is disabled. The two candidate variants use the same binary.

Values are median **child-process CPU microseconds per source buffer**, including
GStreamer, startup, teardown, scrapes and maintenance. They are not tracer-only
costs. Source buffer counts are 1,000,000 for one identity and churn, 250,000 for
eight identities, and 16,384 for 96 identities. The larger graph exercises cache
collisions; the churn workload adds 200 one-buffer graphs from another thread.

| Workload | Before | Default | With I/O counts | Default change |
| --- | ---: | ---: | ---: | ---: |
| One identity, sample 16 | 1.409 | 1.450 | 1.454 | +2.9% |
| Eight identities, sample 16 | 5.967 | 5.873 | 6.017 | -1.6% |
| One identity, sample 1 | 2.379 | 2.239 | 2.417 | -5.9% |
| 96 identities, sample 16 | 108.106 | 106.936 | 110.049 | -1.1% |
| One identity + 200 graph changes, sample 16 | 1.546 | 1.513 | 1.545 | -2.1% |

The short sampled pipeline used 2.9% more CPU. The other default medians were
1.1–5.9% lower. The shared desktop had variable load and CPU frequency, with
noticeable outliers, particularly in later workloads. Small differences should
not be treated as established effects.
The raw file retains every run, including tracer-off measurements and min/max
CPU values. These tests do not qualify production video throughput or rare tails.

After 200 graph creations/destructions, every baseline run still exported 39,600
time series belonging to those graphs. Both candidate variants exported zero.
Median peak RSS was 49,800 KiB before and 32,248 KiB for the default
candidate. This is a bounded churn check, not a long-running memory soak.

Raw observations, commands, hashes and churn output:
[`benchmarks/2026-10-20-io-pull.json`](benchmarks/2026-10-20-io-pull.json).
Candidate SHA-256: `b62b4c7cb5e7c4441bf8dc3f27a89a420cb17b8fe4b6b7a0741a4bf100f81b0d`.

Reproduce with `plugins-before`, `plugins-after` and `plugins-after-io` directories
containing their respective plugin binaries (the last two are identical):

```sh
python3 tracer/prometheus/bench_matched.py --root /path/to/plugins \
  --variants off,plugins-before,plugins-after,plugins-after-io \
  --io-variants plugins-after-io --rounds 7 --sample-rate 16 \
  --name linear16 -- taskset -c 2 gst-launch-1.0 -q \
  fakesrc num-buffers=1000000 ! identity ! fakesink sync=false
```

The raw commands specify the other chain lengths and sample rate. For churn,
replace the child command after `taskset -c 2` with
`/usr/bin/python3 tracer/prometheus/bench_churn.py` (requires PyGObject).

### Validation of this follow-up

- `cargo fmt --all` and workspace Clippy with all features/targets: passed.
- `cargo test`: 15 unit tests, 16 integration tests, and six isolated
  configuration runs passed. Coverage includes sampling under cache eviction,
  filtering, PTS age, mux/demux pads, ghost forwarding, lists, failed pushes,
  dropped output, request-pad removal, queue moves, graph retirement and pull
  timing with EOS/errors and I/O counts enabled or disabled.
- `python3 tracer/prometheus/test_sanitizer.py` (also invoked by
  `just test-address-sanitizer`): the same suite passed. The recipe now loads
  Cargo's exact sanitized cdylib and runs the full suite by default; its former
  default filter matched no tests.
- C and Rust README examples compiled and returned metrics.
- `cargo audit`: no vulnerabilities; one unmaintained `paste 1.0.15` warning
  remains. Patched dependencies and removal of unused Pyroscope dependencies
  eliminate the earlier advisories and old `rustix` nightly build failure.
- The required nightly `cargo udeps` command for all three tracer packages and
  all targets passed with no unused dependencies.

## GStreamer binding upgrade — 2026-10-20

Updated the Rust bindings to GStreamer 0.24.5 and GLib 0.21.5. Their dependencies
use `pastey`, removing the remaining `paste` maintenance advisory. The metrics
signal uses GLib's new callback API; buffer callbacks are unchanged.

Compared with `8445cda`, using Rust 1.89.0 and native GStreamer 1.24.2 on the same
i5-1345U host, pinned to logical CPU 2. Each case has one warm-up and seven
measured rounds, alternating variant order, with separate registries, verified
plugin paths and one scrape per second. No builds or tests ran concurrently.
Values are median child-process CPU microseconds per source buffer, including
GStreamer, startup, teardown and scrapes.

| Workload | Before | After | Change |
| --- | ---: | ---: | ---: |
| One identity, sample 16 | 1.863 | 1.897 | +1.8% |
| Eight identities, sample 16 | 5.714 | 5.757 | +0.8% |
| One identity, sample 1 | 2.174 | 2.150 | -1.1% |
| 96 identities, sample 16 | 81.510 | 80.812 | -0.9% |
| Eight identities, sample 16, I/O enabled | 6.005 | 5.926 | -1.3% |

All median changes were within 2%; these small differences on a shared desktop
do not establish a speedup or regression. Commands, buffer counts, binary hashes
and every run are in
[`benchmarks/2026-10-20-bindings.json`](benchmarks/2026-10-20-bindings.json).

Formatting, workspace Clippy with all features/targets, `cargo test`, the full
Prometheus ASan suite and the required nightly dependency check passed.
`cargo audit` reports no vulnerabilities or warnings.

A wider `cargo test --workspace` run timed out in OpenTelemetry after 120 seconds.
Its serial basic-pipeline test also timed out with a 90-second command limit on
both this build and a separate checkout of `8445cda`. A standalone one-buffer
OpenTelemetry pipeline completed with the upgraded bindings.

## CPU sampling in nested calls — 2026-10-20

CPU clocks now run only for sampled spans and their direct children. A child's
inclusive CPU time is enough to subtract its work from the sampled parent;
unsampled grandchildren need no clock reads. Child clock failures invalidate
exclusive CPU attribution without discarding a valid inclusive measurement.
Wall counters and per-call histogram sampling are unchanged.

Compared with `7391de1` on the same i5-1345U host, using Rust 1.89.0 and native
GStreamer 1.24.2, pinned to logical CPU 2 with the powersave governor. Release
builds, separate registries, verified plugin paths, one warm-up and seven measured
rounds per variant with alternating order, and one scrape per second. No builds
or tests ran during measurements. All elements are selected and PTS age is off.

Values are median child-process CPU microseconds per source buffer, including
GStreamer, startup, teardown and scrapes. One-identity runs use 1,000,000 buffers,
eight-identity runs use 250,000, and 96-identity runs use 16,384. The longer
full-sampling control uses 1,000,000 buffers.

| Workload | Before | After | Change |
| --- | ---: | ---: | ---: |
| One identity, sample 16 | 1.452 | 1.455 | +0.2% |
| Eight identities, sample 16 | 5.796 | 5.371 | -7.3% |
| One identity, sample 1 | 2.209 | 2.197 | -0.6% |
| Eight identities, sample 1 | 9.172 | 9.614 | +4.8% |
| 96 identities, sample 16 | 105.244 | 78.112 | -25.8% |
| Eight identities, sample 16, I/O enabled | 5.922 | 5.506 | -7.0% |
| Eight identities, sample 1, longer control | 9.195 | 8.441 | -8.2% |

The sampled eight-identity runs used 7.3% less CPU, or 7.0% less with I/O enabled.
The 96-identity median fell 25.8%, with substantial run-to-run variation. Shallow
pipelines were essentially unchanged. The full-sampling control initially showed
a 4.8% increase; a longer repeat showed an 8.2% decrease, despite identical clock
read counts. These controls show host variability, not an established full-sampling
speedup or regression. The raw data retains both runs and all outliers. These
synthetic results do not establish production video throughput gains.

Separate `strace` runs counted `CLOCK_THREAD_CPUTIME_ID` reads over 1,000 buffers:

| Workload | Before | After |
| --- | ---: | ---: |
| One identity, sample 16 | 378 | 390 |
| Eight identities, sample 16 | 4,628 | 2,072 |
| 96 identities, sample 16 | 165,778 | 23,046 |
| Eight identities, sample 1 | 18,000 | 18,000 |

Sampled runs have different process-specific seeds. The deep chains made about
55% and 86% fewer CPU clock reads. Syscall counting ran separately from the
timing benchmarks; no elapsed-time claims come from `strace`.

Commands, binary hashes, syscall counts and every measured run are in
[`benchmarks/2026-10-20-cpu-sampling.json`](benchmarks/2026-10-20-cpu-sampling.json).
Use `bench_matched.py` with the recorded commands and sample rates to reproduce.

Formatting, workspace Clippy with all features/targets, `cargo test`, the full
Prometheus ASan suite, `cargo audit` and the required nightly dependency check
passed. The suite has 17 unit tests, 16 integration tests and six isolated
configuration runs. Added coverage checks unsampled descendants, failed child
clocks, CPU/blocked sample counts in deep and filtered push chains, and pull
sample counts through ghost pads with successful, EOS and error returns.
