# Trace collection for GStreamer

Use `mars-instrument` for new configurations. To collect traces without metrics,
set `GST_TRACERS='mars-instrument(signals="traces")'`. The
[instrumentation guide](INSTRUMENT.md) covers collecting multiple signals.

`mars-otel` is the compatibility entry point for existing tracing configurations.
It keeps its defaults and uses the same tracing implementation. The settings
and examples below document that interface.

`mars-otel` records `gst_pad_push` and `gst_pad_pull_range` calls and exports
OTLP/HTTP protobuf traces.
It does not create live SDK spans or install a global tracer provider. An
optional `profiles` feature adds an experimental embedded CPU sampler and
Pyroscope delivery backend. It also registers `mars-profiles`, a
[profiles-only entry point](PROFILES.md) using that same sampler.
Normal builds use stock GStreamer; no GStreamer core patch is required or applied.

`mars-instrument` selects metrics, traces, logs and profiles independently, with
full metrics over Prometheus or OTLP. Plain `GST_TRACERS=mars-instrument`
preserves its original metrics/traces preset. See
[setup, measurements, bounds, and current coverage](INSTRUMENT.md).

## Run

```sh
cargo build --release -p gst-otel-tracer
export GST_PLUGIN_PATH="$PWD/target/release"
export GST_TRACERS=mars-otel
export OTEL_SERVICE_NAME=gstreamer-pipeline
export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318
export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf
gst-launch-1.0 videotestsrc num-buffers=100 ! videoconvert ! fakesink
```

The tracer uses OpenTelemetry Rust 0.33.1 for context, IDs, resource detection,
and the optional log exporter. Traces use a dedicated OTLP/HTTP protobuf encoder
and reqwest 0.13 transport. HTTPS uses Rustls with the platform's native trust
roots. This build supports HTTP protobuf without compression. `OTEL_SERVICE_NAME`
and `OTEL_RESOURCE_ATTRIBUTES` are read by the SDK's resource detectors.

`OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` is used exactly; otherwise `/v1/traces` is
appended to `OTEL_EXPORTER_OTLP_ENDPOINT` (default `http://localhost:4318`).
`OTEL_EXPORTER_OTLP_TRACES_HEADERS` and `_TIMEOUT` override their general OTLP
counterparts. Headers use comma-separated, percent-encoded key/value pairs;
timeouts are milliseconds and default to 10000. Unsupported protocol or
compression settings disable tracing with an initialization error.

## Experimental correlated CPU profiles

On Linux x86-64 or arm64 with GStreamer 1.22 or newer, build the optional feature and set
the Pyroscope base URL:

```sh
RUSTFLAGS="-C force-frame-pointers=yes" cargo build --profile profiling -p gst-otel-tracer --features profiles
export GST_PLUGIN_PATH="$PWD/target/profiling"
export GST_TRACERS=mars-otel
export OTEL_SERVICE_NAME=gstreamer-pipeline
export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318
export GST_OTEL_PROFILE_URL=http://localhost:4040
gst-launch-1.0 videotestsrc num-buffers=1000 ! videoconvert ! fakesink
```

The implementation is isolated in [src/profiles.rs](src/profiles.rs), including
signal handling, thread registration, context publication, aggregation, the
Pyroscope 2.1.2 custom backend and shutdown. Capture hooks publish fixed IDs;
they do not call Pyroscope's thread-tag API. The sampler reads those IDs when a
CPU timer interrupts the executing thread, and includes them in the aggregation
key. Nested pushes replace the active ID and restore the parent on return.
Queue workers publish their own active push, using the existing buffer context
propagation. Overflow or uncertain hook nesting suppresses correlation until
the stack recovers. Unsampled pushes still contribute CPU samples without IDs.

Architecture-specific signal registers and return-address normalization live in
[src/profilearch.rs](src/profilearch.rs). AArch64 uses x29 frame records and
removes pointer-authentication signatures from saved return addresses. See
[ARM build and stack limitations](PROFILES.md#linux-arm64).

Element attribution is separate from trace correlation. The hooks in
[src/profilecontext.rs](src/profilecontext.rs) track chain/list/pull calls and
GStreamer task ownership. Each native stack gains one synthetic root identifying
the element instance, or a separately marked task owner or unattributed context.
This works even with trace sampling disabled and adds no series labels. See
[how to read these frames](PROFILES.md#finding-the-element-that-used-cpu) and
[the Grafana screenshots](../../reports/otel/PROFILE_ELEMENT_REVIEW.md).

Profiles use the trace resource's `service.name` as Pyroscope `service_name`.
`span_id`, `trace_id` and native `thread_id` are **pprof sample labels**, not
series labels. Eligible spans have `pyroscope.profile.id` equal to their
16-character span ID. The marker means sampling was enabled for that span;
short spans may receive no samples, and dropped trace/profile exports can leave
missing counterparts. Attribute limits can omit the marker; a string limit
below 16 omits it instead of truncating the identifier. Follow the
[Grafana correlation guide](CORRELATION.md) to configure the Tempo-to-Pyroscope
link, including the data-source UID, service-name mapping and CPU profile type.
Its local demo validates the provisioned configuration, stored Tempo span marker,
and exact aggregate/span/trace CPU totals through Grafana's data-source proxies.
Unknown spans return no samples. Browser click-through remains a separate manual
check; see the [validation report](../../reports/otel/CORRELATION_REVIEW.md).
The [profile benchmark](bench/profiles.py) also supports direct Pyroscope query
validation with `--pyroscope`.

| Setting | Default | Meaning |
| --- | --- | --- |
| `GST_OTEL_PROFILE_URL` | unset (disabled) | HTTP(S) Pyroscope base URL; delivery uses `/push.v1.PusherService/Push` |
| `GST_OTEL_PROFILE_HZ` | `100` | Per-thread CPU sampling frequency, from 1 to 1000 Hz |
| `GST_OTEL_PROFILE_ELEMENTS` | `true` | Group native stacks by element execution context or task owner |
| `GST_OTEL_PROFILE_SIGNAL` | `SIGRTMIN()+6` | Numeric realtime signal reserved by this sampler |

Feature compilation alone does not start profiling. Missing feature/platform,
invalid settings, or an occupied signal log an initialization warning and leave
tracing operational. A thread that blocks the signal or cannot create a timer
is skipped and increments `registration_failures`; it is not retried. Profiling
initialization and shutdown are process-wide and cannot be restarted. Choose one
of `mars-otel`, `mars-instrument`, or `mars-profiles`; the first constructed tracer
selects the signal configuration and later instances install no hooks. Configure
credentials/authentication at a local forwarding proxy for this prototype;
it does not map OTLP headers to Pyroscope requests.

Sampling covers up to 64 simultaneously registered **participating threads**,
including their CPU work between calls after registration. Element attribution
registers GStreamer tasks at ENTER, before source production and the first push;
chain and push/pull hooks provide fallback registration. It does not measure
sleeping time, all process threads, or CPU before registration. Trace correlation
uses the innermost active sampled push; element context uses the innermost
chain/list/pull call. Neither duplicates samples into enclosing spans/elements.
Each thread has a 256-sample ring, a 64-frame limit and an 8 KiB stack
snapshot. Storage is allocated on first registration: about 148 KiB per used
slot, rather than allocating about 9 MiB for all 64 slots at startup. Retiring
threads release their timers and slots; their buffers remain available for
reuse and safe handling of pending signals. Full rings drop samples.

The `mars-profiles` entry point uses the same bounded element context stack,
without trace capture or span correlation. See its
[configuration and migration guide](PROFILES.md).

A dedicated report worker drains rings every second, aggregates raw stacks,
reuses a bounded symbol cache, and interns frames directly into pprof. The SDK
upload queue holds at most ten encoded profiles, each capped at 8 MiB. A full
queue drops the new profile without blocking sampling or report processing.
Oversized reports are dropped before enqueueing. These bounds exclude the
current report, compression/HTTP scratch space and transient debug-file parsing.
The worker uses a stop channel and joins instead of the SDK Agent's timer and
stop-condition-variable lifecycle. Caller shutdown deadlines still apply while
uploads finish in the background.

The symbol cache holds at most 4096 addresses and 4 MiB of frame/string payload,
with at most 16 inline frames per address and 1024 bytes per name/path. Executable
mapping changes invalidate it. ELF files are read directly, with one temporary
DWARF resolver per mapped executable region; no process-wide backtrace mutex or
dynamic-loader enumeration is involved. Embedded and installed build-ID debug
files provide names, file/line and inline frames. Debuglink-only/supplementary
debug files and symbols unloaded before report processing are not guaranteed;
unresolved locations keep their instruction addresses and use a library-name
fallback when the mapping is known. Debug parsing can still raise peak RSS.

Profile time windows use actual report intervals, rather than per-sample
timestamps; reporting delays can place buffered samples in a later window.
The measurements below characterize this prototype, without a production
overhead guarantee.

The signal handler snapshots registers and copies stack memory with Linux
`process_vm_readv`, then follows frame pointers only inside that bounded copy.
Unavailable stack reads retain the interrupted instruction. Build the
application and relevant native dependencies with frame pointers and install
debug symbols for useful stacks; the Rust flag above affects Rust code only.
Optimized GStreamer/dependency builds without frame pointers can yield shallow
or incomplete stacks. Sandboxes denying `process_vm_readv` also limit samples
to the interrupted instruction. This implementation does not provide DWARF
unwinding or JIT stack support.

`gstoteltracer::profile_stats()` reports queued/correlated samples, full-ring
drops, timer overruns, failed stack reads, truncated stacks, registration
failures, reported samples and active timers. `element_samples`, `task_samples`,
and `context_capacity_rejections` expose element attribution coverage and its
bounded registry. `dropped_processing` counts samples
discarded by the encoded-size limit; `dropped_upload` counts samples discarded
when the upload queue is full or disconnected. `reported_samples` counts samples
successfully enqueued to the SDK, **not successful delivery**: the SDK does not expose HTTP
acknowledgements or failures through the backend interface. A successful
`shutdown` therefore cannot guarantee profile delivery.

Stop streaming, then call `gstoteltracer::shutdown(timeout)` to stop the timers,
drain a final profile and finish SDK shutdown. It shares the existing caller
deadline; background cleanup continues after timeout and shutdown may be
retried. `flush()` and the automatic pipeline-NULL drain cover traces only.
`gst-launch-1.0` relies on periodic profile uploads and may lose the final
interval when it exits. Applications needing a final profile drain must use the
explicit Rust shutdown API. Pending timer signals require keeping the small
handler and sampler storage resident after shutdown. GStreamer's normal plugin
loader does this; unloading an rlib's hosting DSO and forking after profiler
initialization are unsupported. Applications must reserve the chosen signal
and not replace its handler or send their own timer notifications through it.

Run `cargo test -p gst-otel-tracer --features profiles` for real CPU-timer tests
that decode Pyroscope and OTLP requests, including nested/queue attribution,
sampling rejection, initialization failures, thread retirement, context recovery
and shutdown under slow delivery. Regression tests also hold glibc's loader
lock during symbolization, exercise a saturated/disconnected upload queue, and
run 1000 Hz sampling alongside allocation, mutexes and thread/loader activity.
These checks do not identify the cause of an earlier production deadlock;
[the lock audit](../../TRACE_PROFILE_INTEGRATION.md#deadlock-audit-and-worker-changes)
records the concrete hazards removed and the remaining limits.

## Capture and export

A source starts a trace for each buffer journey. Synchronous nested pushes use a
bounded thread-local stack. Buffer metadata carries an owned correlation token
across queues and clones ownership when buffers are copied or transformed. A
recycled source buffer starts a fresh journey. An external valid OTel context at
the source supplies the parent, sampling decision, and trace state. OTLP records
whether that parent is remote; subsequent locally captured spans have local
parents, including across queues.

Pad/element names, paths, and peer descriptions are cached per pad. Weak
subscriptions to the pad, its peer, and their ancestors invalidate descriptions
when those objects change. Unrelated pipelines do not invalidate the cache.
Pending records retain their captured description. Cached descriptors own
strings, not pad references. First sampled use and invalidation rebuild metadata
on the streaming thread; the steady path does no descriptive formatting. Thread
descriptions are cached on first sampled use of each thread.

Rejected traces propagate a rejection token without loading descriptions,
reading buffer sizes, creating attributes, or enqueuing records. Source
classification is cached separately; rejected synchronous descendants do not
consult the pad cache at all. Roots with a zero sampling ratio need no random
IDs. Rejected frames store just pad identity and the correlation token; sampled
records use a separate stack. Unchanged rejected metadata tokens are not rewritten,
while external identity and recycled-root decisions remain intact.
Partial sampling generates a trace ID once per root using the SDK 0.33 ratio
algorithm; descendants inherit the result.
Each begin hook resolves the capture thread-local state once. Its borrows end
before calling GStreamer, preserving reentrant callbacks and log correlation.

Sampled pushes capture IDs, monotonic timestamps, buffer identity/size, thread
identity, a descriptor reference, and the flow result. Post hooks use a bounded
nonblocking queue. One worker drains compact records into reusable batches and
encodes them directly into OTLP protobuf. Static site and thread attributes are
encoded once on the worker and cached with their descriptions. Resource and scope
headers are encoded once per exporter. Dynamic integers, IDs, timestamps, status,
and flags are written directly, without `SpanData` or per-span protobuf objects.
The SDK conversion remains a test oracle for decoded message equivalence.

The worker reuses its encoding buffers and sends reference-counted `Bytes`
bodies, without cloning the encoded payload. A single request is in flight at a
time. Both blocking and worker-owned asynchronous reqwest adapters are available;
neither depends on an application Tokio runtime. Logs retain the upstream SDK
exporter. Producer handoffs remain individual records: a measured batching
experiment increased streaming CPU and was removed.
The first sampled hook anchors GStreamer's monotonic clock to wall time; later
wall-clock adjustments do not change recorded durations.

Spans are named `gst.pad.push`, with `src_pad.*`, `sink_pad.*`, `buffer.id`,
`buffer.size`, `thread.id`, `thread.name`, `ts.start`, `ts.end`, and
`gst.flow_return` attributes. A negative flow return sets error status. Duration
is the inclusive push call, including synchronous downstream work; it is not
exclusive element processing time or end-to-end buffer latency.

## Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `OTEL_TRACES_SAMPLER` | `parentbased_always_on` | Also accepts `parentbased_always_off` and `parentbased_traceidratio` |
| `OTEL_TRACES_SAMPLER_ARG` | Required for ratio sampler | Finite number between 0 and 1 |
| `GST_OTEL_QUEUE_CAPACITY` | 8192 | Pending compact records, 1–1048576 |
| `GST_OTEL_BATCH_SIZE` | 512, capped by queue size | Worker batch, 1–queue capacity |
| `GST_OTEL_HTTP_CLIENT` | blocking | `blocking` or `async`; both send one batch at a time |
| `GST_OTEL_EXPORT_INTERVAL_MS` | 200 | Partial batch export interval, 1–60000 ms |
| `GST_OTEL_FLUSH_TIMEOUT_MS` | 10000 | Pipeline-stop drain deadline, 1–60000 ms |
| `OTEL_SPAN_ATTRIBUTE_COUNT_LIMIT` | 128 | 0–4096; omitted attributes increment OTLP's dropped count |
| `OTEL_SPAN_ATTRIBUTE_VALUE_LENGTH_LIMIT` | 4096 | 1–1048576 Unicode characters per descriptive value |
| `GST_OTEL_LOGS` | false | `true` or `1` opts into the separate debug-log bridge |

Invalid tracing configuration disables hooks and emits an `mars-otel` error.
Non-parent-based and custom samplers are not supported. `OTEL_BSP_*` does not
configure this worker; there is no SDK batch span processor. No events or links
are currently produced. SDK span processors and custom global tracer providers
do not intercept this export path.

The memory bound is a queue plus one export batch, up to 256 nested frames per
streaming thread, and metadata for live pads/buffers. Counters and in-progress
submission counts use 64 fixed, cache-aligned shards selected per thread;
additional threads share shards safely, and retired threads leave their totals
available without an accumulating registry. The worker's reusable OTLP buffers
are bounded by the batch and a 64 MiB encoded request limit. Cached encoded
attributes live with their descriptors; invalidation creates a new descriptor
while pending records retain the old one. Thread and pipeline counts remain
application-controlled. Overflow drops the new record; it never waits for the
receiver. A transport or malformed-response failure counts the entire attempted
batch as failed. A valid partial-success response counts only rejected spans as
failed and the remainder as accepted, with no additional retry queue. Trace
transport makes one export attempt per batch, as does log export.
Responses are consumed for connection reuse and capped
at 4 MiB. Non-success HTTP status, request timeout, oversized responses, and
encoded bodies over 64 MiB count the attempted trace batch as failed. A timeout can leave
delivery uncertain, and collector acceptance does not guarantee durable storage.

## Sampling

Sampling is decided once for each new root buffer journey. For example, to
record approximately 10% of roots:

```sh
export OTEL_TRACES_SAMPLER=parentbased_traceidratio
export OTEL_TRACES_SAMPLER_ARG=0.1
```

Every descendant inherits that decision, including across queue boundaries and
buffer copies that preserve metadata. This samples whole trace journeys, not
individual pushes. If an external valid parent is present, its sampled flag
wins over the root ratio: even at 0%, a sampled external parent is recorded.
`parentbased_always_off` rejects new roots with the same parent-preserving behavior;
the default `parentbased_always_on` samples all new roots.

At 0%, rejected roots avoid random IDs, descriptive metadata, record creation,
serialization, and export. GStreamer still dispatches the registered hooks, and
the tracer still tracks nesting, propagates rejection, and counts unsampled
pushes. This prevents downstream stages from resampling rejected work. To remove
the OTel hooks, omit `mars-otel` from `GST_TRACERS` before process startup.
Other active tracers can still incur GStreamer's shared dispatch overhead.

## Performance and sampling cost

The [combined video comparison](../../reports/otel/INSTRUMENT_VIDEO_RESULTS.md)
also measures `mars-otel` with default trace-export settings alongside
`mars-instrument`, both with and without profiles and logs.

**Current profiles-only, trace, and log comparison, 2026-10-23.** See the
[matched 1080p video results](../../reports/otel/PROFILE_VIDEO_RESULTS.md) for
`mars-profiles` and `mars-otel` with profiles and logs enabled. The new runner
includes an untraced baseline, actual per-frame logs, explicit final shutdown,
decoded-output checks, hardware cycle counts, and a default-settings check.
The tables below describe older implementations and configurations.

**Background processing and memory follow-up, 2026-10-21.** Both columns below
enable 100 Hz profiling and 100% trace sampling: the initial embedded prototype
versus the optimized worker. Builds use the same release/frame-pointer flags,
Rust 1.89, stock GStreamer 1.24.2, and CPU affinities described below. The final
worker uses `addr2line` 0.27.1 and `object` 0.40.0. Three measured repetitions
follow a discarded warm-up; the two variants alternate execution order.

Median milliseconds, except peak RSS in MiB; **previous → optimized**:

| Workload | CPU outside streaming threads | Total process CPU | Peak RSS |
| --- | ---: | ---: | ---: |
| 50,000 tiny pushes, one source | 84.1 → 81.4 | 117.6 → 113.6 | 68.4 → 60.5 |
| 200,000 tiny pushes, four sources | 300.5 → 162.4 | 479.7 → 343.2 | 93.2 → 85.1 |
| 240 frames, 1080p conversion | 213.6 → 137.2 | 2724.9 → 2632.0 | 82.6 → 75.0 |
| 1200 frames, 1080p conversion | 1262.5 → 561.4 | 15465.9 → 15332.9 | 84.9 → 76.1 |

CPU outside streaming threads is computed per run as process CPU minus the sum
of source-thread CPU, then the median is taken. It includes trace export and
other process work as well as profiling; it is not an isolated symbolizer
measurement. The two-second drain remains outside pipeline wall time and inside
process CPU. Peak RSS still includes debug parsing and allocator retention.
Cold symbolization remains expensive on tiny workloads; the persistent cache
helps workloads that span multiple reporting intervals.

The long-video case reduced CPU outside streaming threads by 55.5%, and the
short-video case by 35.8%. Peak RSS fell by about 7.6–8.7 MiB across these cases.
Long-video source CPU varied from 12.4 to 18.2 seconds across both variants;
that variation prevents a reliable end-to-end speedup claim. All 1,534,560
spans in the measured follow-up runs arrived without trace export losses, and
every received correlated sample matched an exported span and profile marker.
The final build also passed the separate Pyroscope 2.4.0 ingestion/query check.

[Follow-up raw measurements](../../reports/otel/PROFILE_OPTIMIZATION_RESULTS.csv) retain every
run, including warm-ups. The `previous` mode is the original enabled prototype;
`enabled` is the optimized build. Reproduce with the existing benchmark arguments
plus `--previous /path/to/original-profiles --modes previous enabled`. Keep the
old library before rebuilding. As with the earlier measurements, these are
development-laptop observations, not production overhead guarantees.

**Initial embedded prototype measurements, 2026-10-21.** Compared against trace-only
commit `1b9e13e`, with 100% trace sampling and 100 Hz CPU profiling. All variants
used Rust 1.89 release builds with `-C force-frame-pointers=yes`, stock
GStreamer 1.24.2 and an i5-1345U. Child processes were pinned to the two physical
P cores (CPUs 0 and 2), with the local HTTP receiver on CPU 4. No Rust runtime
code was changed between these measurements and the initial prototype; this
historical table predates the worker and memory changes above.

Median milliseconds; each cell is **trace-only → profiles enabled**:

| Workload | Streaming-thread CPU | Pipeline wall time | Total process CPU, including drain |
| --- | ---: | ---: | ---: |
| 50,000 tiny pushes, one source | 30.58 → 31.54 (+3.1%) | 31.23 → 32.72 (+4.8%) | 59.07 → 118.94 (+101%) |
| 200,000 tiny pushes, four sources | 134.99 → 140.71 (+4.2%) | 81.46 → 90.78 (+11.4%) | 206.43 → 297.87 (+44.3%) |
| 240 frames, 1080p conversion | 2366.93 → 2361.74 (−0.2%) | 2378.32 → 2387.09 (+0.4%) | 2438.69 → 2585.91 (+6.0%) |
| 1200 frames, 1080p conversion | 12821.14 → 13181.80 (+2.8%) | 12838.98 → 13461.34 (+4.8%) | 13124.28 → 14225.81 (+8.4%) |

The tiny workload is `fakesrc` through four `identity` elements into `fakesink`.
The video workload generates 1920×1080 RGBA noise and converts it to I420.
The first and third rows use five repetitions; the longer cases use three.
Every variant had a discarded warm-up, and execution order rotated between
repetitions. Both variants waited two seconds after stopping the pipeline to
include profile/report processing in process CPU. Wall time excludes that wait.
The receiver acknowledged actual uploads without decoding them during timing;
payload decoding and attribution checks happened after each child exited.
The Pyroscope server test ran separately from performance measurements.

The initial prototype added about 45–48 MiB of peak RSS when samples were
processed, including roughly 9 MiB of fixed sampler storage. The longer video run rose from 37.1 to
85.0 MiB. Its remaining CPU overhead shows that profile processing cost persists
beyond startup. A separate 1 Hz tiny-push control captured no samples yet added
16.2 ms of process CPU, indicating that publication, profile-link attributes and
agent management also have a cost. That control does not isolate those components
individually. The follow-up above replaces the per-report cache and per-span
profile-link allocation, and allocates sampling buffers on demand.

Compiling the feature with no profile URL added 2.9% streaming CPU on the
single-source tiny workload and 0.04% on short video. Omitting the feature showed
no consistent streaming regression. These are observations from a development
laptop with other services running, not confidence bounds or production
guarantees; sub-percent differences are within observed variation. Debug symbols
and different native libraries can change stack quality, symbolization time and
memory use. Many native frames in this environment remain raw addresses.
The long-video pairs ranged from +7.1% to +14.3% total CPU; both variants slowed
substantially in the last pair. Treat the +8.4% median as an approximate result,
not a precise overhead bound.

All 3,548,000 spans from the measured runs arrived without trace export losses,
and every correlated profile sample matched an exported span and its profile
attribute. The initial 50,000-push/four-source run captured **no CPU samples**:
each worker finished before its first timer tick. Its rows remain in the raw
data, but the table uses a longer run with samples. At 100 Hz, very short pushes
and short-lived threads can have no profile despite having a link attribute.

[Raw per-run measurements](../../reports/otel/PROFILE_RESULTS.csv) include warm-ups with
`repeat=-1`. [profiles.py](bench/profiles.py) compares three prebuilt plugin
directories: the pre-change baseline, the current default build, and the current
`--features profiles` build. Build each with the same flags and copy its shared
library into a separate directory, then run, for example:

```sh
taskset -c 4 python3 tracer/otel/bench/profiles.py \
  --baseline /path/to/baseline --no-feature /path/to/default \
  --profiles /path/to/profiles --cpus 0,2 --repeats 5
# Longer sampled four-source case; enlarge the queue to retain all spans.
taskset -c 4 python3 tracer/otel/bench/profiles.py \
  --baseline /path/to/baseline --no-feature /path/to/default \
  --profiles /path/to/profiles --modes baseline enabled --workloads push4 \
  --buffers 40000 --queue-capacity 262144 --repeats 3
# Separately validate ingestion and query weights against an isolated server.
python3 tracer/otel/bench/profiles.py \
  --baseline /path/to/baseline --no-feature /path/to/default \
  --profiles /path/to/profiles --modes enabled --workloads video --repeats 1 \
  --pyroscope http://localhost:4040
```

Choose CPU affinities appropriate to the host. The server check requires a fresh
time range/service: it uses `service_name="gst-profile-bench"` and verifies total,
span and trace CPU weights through Pyroscope's
[query API](https://grafana.com/docs/pyroscope/latest/reference-server-api/).

**Earlier trace-only measurements.**

The last isolated plugin comparison used stock GStreamer 1.24.2, Rust 1.89,
release builds, one stream, 10000 buffers, and five pushes per buffer. Logging
was disabled; a local HTTP/1.0 collector received exports with a 65536-record
queue and batches of 512. Median CPU milliseconds over seven repetitions:

| Sampling | Source-thread CPU | Process CPU, including drain |
| --- | ---: | ---: |
| Tracer disabled | 20.92 | 21.82 |
| 0% | 42.87 | 44.00 |
| 10% | 48.08 | 59.44 |
| 100% | 56.09 | 94.88 |

These measurements were recorded for plugin commit `8b519d8`, before merging
`main`'s GStreamer Rust binding upgrade. They include ordinary pipeline work,
and are a small synthetic workload, not a production overhead guarantee. All
healthy fully sampled runs delivered 50000 spans without drops or pending
records. The final TLS optimization reduced 0% source CPU from 44.95 to 42.87 ms
(4.6%); at 16 streams the smaller difference was comparable to control variation.
See [rejected-path measurements](../../reports/otel/REJECTED_RESULTS.md) for paired results,
parallel streams, tee/queue workloads, failure accounting, and raw data.

Zero sampling still costs roughly twice the disabled source CPU in this case.
A separate empty-hook benchmark identifies substantial overhead in GStreamer's
dispatcher, before the tracer's own capture work. Further reductions therefore
have limits within the plugin alone. An experimental GStreamer 1.24.2 dispatcher
patch is retained under `bench/` for research; deploying it is deferred because
it requires rebuilding the core library. Its larger gains are separate from the
plugin measurements above, and it is not part of normal installation or the
supported runtime setup.

## Drain and counters

A successful pipeline transition from READY to NULL flushes completed records.
Stop streaming before requesting the final drain. Flush covers records enqueued
before its queue marker, not open calls or concurrent later submissions. Rust
applications can use:

```rust,ignore
let counters = gstoteltracer::flush(std::time::Duration::from_secs(5))?;
let snapshot = gstoteltracer::stats()?;
gstoteltracer::shutdown(std::time::Duration::from_secs(5))?;
```

Shutdown closes submissions, drains accepted records, and shuts down the
exporter. It is process-wide and cannot be restarted. Records completing after
closure increment `dropped_closed`. On a caller timeout, the worker may still be
finishing a request; counters remain inspectable, and shutdown may be retried.
Abrupt process termination cannot guarantee a drain.

`flush` shares one deadline across traces, metrics, and logs. `shutdown` also
includes the CPU profiler in that deadline. `flush_report(timeout)` and
`shutdown_report(timeout)` return separate trace, metric, log, and profile
results, so a failure in one signal does not hide another. Flush does not yet
request a profile drain; its report has `profiles: None`.

Logs use a bounded background queue, with `OTEL_BLRP_MAX_QUEUE_SIZE` (default
2048, maximum 65536), `OTEL_BLRP_MAX_EXPORT_BATCH_SIZE` (default 512, capped by
queue capacity), and `OTEL_BLRP_SCHEDULE_DELAY` (default 1000 ms, maximum 60000).
All three settings must be positive. Log bodies are capped at 16384 characters,
category names at 256, and file/function attributes at 4096. Log export uses the
same HTTP/protobuf transport and signal-specific OTLP settings as traces.
`log_stats()` exposes accepted, failed, rejected, dropped, and pending records.
Mars diagnostic categories are excluded from the log bridge to prevent export
failures from generating more telemetry for the failing destination.

C and other GStreamer applications can find the tracer through
`gst_tracing_get_active_tracers()` and read its `stats` property, a `GstStructure`.
The same cumulative fields are available through the Rust API:

- `started_spans`, `completed_spans`, `enqueued_spans`, `exported_spans`.
- `dropped_queue_full`, `dropped_closed`, `failed_export_spans`, `failed_exports`.
- `rejected_spans` counts explicit collector rejections (also included in
  `failed_export_spans`); `export_warnings` counts responses with a warning
  message. `exported_spans` counts only accepted spans. HTTP 200 partial-success
  responses are decoded; rejected batches are not retried. Malformed responses
  fail the batch, while warning-only responses preserve successful accounting.
- `pending_spans`, including queued, batched, and currently exporting records.
- `unsampled_pushes`, `unsampled_pulls`, `skipped_closed` (sampled calls begun after shutdown),
  `propagation_misses`, `propagation_fallbacks`,
  `dropped_depth`, `mismatched_hooks`.

At quiescence, completed = exported + failed-export + queue-full + closed +
pending. Started − completed identifies open calls. Counters are a concurrent
snapshot, not an atomic transaction. `pending_spans` is derived from enqueued
minus exported/failed spans and clamps transient inconsistent reads to zero;
it is exact after producers stop and a successful drain completes.
Record loss can produce incomplete traces
and missing parents; sampling decisions do not change when telemetry is dropped.
`flush` reports export failures since the previous flush; cumulative counters
retain all failures, including ones already reported during pipeline stop.

## Correlation boundaries

- Shared buffers cannot safely accept mutable metadata. Branches retain the
  last common ancestor; exact immediate parentage across every branch is not
  promised. A shared root can replace that ancestor through a lazily initialized,
  synchronized slot owned by our metadata. Routine metadata propagation does not
  consult mini-object qdata. An initially shared buffer without our metadata uses
  a qdata sidecar; its installation and GStreamer's qdata lookup use global locks.
  Copying a buffer without our metadata cannot copy the sidecar. Installing or
  copying our metadata into a writable buffer retires its old qdata override.
  A later asynchronous
  push with missing context is counted and suppressed, never resampled as a root.
- Elements that strip metadata or create unrelated buffers asynchronously need
  element-specific propagation. Generic fan-in retains the first copied input
  context and does not reconstruct multi-input links. Buffer lists do not create
  trace spans. Top-level pull reads create roots and attach their sampling
  decision to the returned buffer; downstream pushes inherit it when metadata
  survives. Nested pulls inherit the active span. Pull EOS is normal completion.
  A read is not necessarily a video frame: demuxers can split or combine reads.
- Capture does not attach SDK contexts per call. Application instrumentation can
  explicitly bridge `current_span_context()` if it needs a child SDK span. The
  optional log bridge uses native log trace/span fields from the capture stack.
- Debug log formatting still runs on the caller thread when opted in. Logs use
  their own SDK batch processor, queue, and accounting; the trace limits and
  counters do not cover logs. The standard GStreamer log handler remains active.

## Validation and benchmark

```sh
cargo test -p gst-otel-tracer
cargo build --release -p gst-otel-tracer
python3 tracer/otel/bench/run.py
# Compare against a separately built old plugin with Pyroscope removed:
python3 tracer/otel/bench/run.py --baseline /path/to/old/plugin/directory
# Compare compact-capture versions, including parallel streams and sampling:
python3 tracer/otel/bench/run.py --baseline /path/to/compact-v1 \
  --baseline-sampling --baseline-disabled --streams 1 4 16 --repeats 5 --drain-ms 0
# Measure the GStreamer tracing floor with empty push callbacks:
python3 tracer/otel/bench/run.py --modes none hooks unsampled sampled --drain-ms 0
```

The Linux benchmark needs a C compiler, GStreamer development files, and Python.
It measures 10000 buffers total, divided between `--streams` independent sources,
with five pushes per buffer, against a local receiver. It records pipeline wall
time, summed source-thread CPU, process CPU including export
drain, exporter CPU at Linux scheduler-tick resolution, actual received spans,
and the tracer's counters. Logging is disabled. Healthy cases use a 65536-record
queue to compare full delivery; slow/absent cases deliberately use a queue of
128 and batches of 64. `--churn-every 10` renames an unrelated element every ten
source buffers; `--tee-queues` substitutes tee/queue stages to exercise shared
buffers and asynchronous propagation (source CPU then excludes queue threads).
Results and logs go under `target/otel-bench`. See the
[follow-up measurements](../../reports/otel/FOLLOWUP_RESULTS.md) for results and limitations.
The [0.33.1 upgrade results](../../reports/otel/UPGRADE_RESULTS.md) cover the previous SDK
export path. [Direct export measurements](../../reports/otel/DIRECT_RESULTS.md) compare the
current encoder and transport, rejected producer batching, adapter/batch-size
experiments, and isolated encoding cost. Use `--http-client`, `--batch-size`, and
`--keep-alive` to reproduce transport comparisons. The default benchmark collector
uses HTTP/1.0; `--keep-alive` tests persistent HTTP/1.1 connections.
[Capture propagation measurements](../../reports/otel/CAPTURE_RESULTS.md) compare the next
iteration against direct export, including disabled controls, the empty-hook
diagnostic, and isolated rejected-capture timings. The `hooks` mode registers
benchmark-only empty C callbacks; it does not run the OTel tracer.
[Rejected-path measurements](../../reports/otel/REJECTED_RESULTS.md) cover the subsequent
TLS improvement and the deferred GStreamer 1.24.2 dispatcher prototype. The larger
dispatcher gains require a rebuilt GStreamer; they are separate from improvements
in this plugin. `--gst-library-path` and `--baseline-gst-library-path` select
private core libraries for that experiment; `--baseline-hooks` also compares
empty callbacks.
