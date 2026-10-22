# Mars GStreamer Tracers

A collection of GStreamer `Tracer` plugins for Prometheus latency metrics,
OpenTelemetry push-call traces and correlated logs, and Pyroscope profiles.
The experimental `mars-instrument` entry point combines metrics, traces, logs,
and CPU profiles through shared push capture.

A rust reimagination of [gstlatency.c](https://gitlab.freedesktop.org/gstreamer/gstreamer/-/blob/main/subprojects/gstreamer/plugins/tracers/gstlatency.c) written by [Stefan Sauer](mailto:ensonic@users.sf.net), with additional features for Prometheus & Otel compatibility.

## Plugins available

The table below contains the plugins available in this repository.

| plugin name                                 | description                                                                                                               | performance | stability |
| ------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------- | ----------- | --------- |
| [mars-metrics](tracer/prometheus/README.md) | captures per element latencies as prometheus metrics                                                                      | [measured overhead](tracer/prometheus/METRICS_RESULTS.md) | alpha     |
| [mars-instrument](tracer/otel/INSTRUMENT.md) | shared push metrics with exemplars, traces, optional logs and CPU profiles | [measured overhead](tracer/otel/bench/INSTRUMENT_RESULTS.md) | experimental |
| [mars-otel](tracer/otel/README.md)        | exports inclusive push-call spans over OTLP/HTTP, with optional correlated GStreamer logs | [measured overhead](tracer/otel/README.md#performance-and-sampling-cost) | pre-alpha |
| [mars-profiles](tracer/pyroscope/README.md)     | captures Pyroscope profiles for the Gstreamer pipeline, requires debug symbols for best results | optimized   | alpha |
| [mars-noop](tracer/noop/README.md)       | a test plugin, likely not useful for any real purpose                                                                     | slow        | none      |

In general `mars-metrics` is recommended for now, and `mars-otel` is still a work in progress.

### Migrating tracer names

Set `GST_TRACERS` to the Mars entry points below. The previous entry-point names
are no longer registered by these plugins.

| Previous entry point | Current entry point |
| --- | --- |
| `gst-instrument` | `mars-instrument` |
| `prom-latency` | `mars-metrics` |
| `otel-tracer` | `mars-otel` |
| `pyroscope` | `mars-profiles` |
| `noop-latency` | `mars-noop` |

Cargo package names, shared-library filenames, and configuration variables
(`GST_INSTRUMENT_*`, `GST_PROM_LATENCY_*`, `GST_OTEL_*`, and `OTEL_*`) retain their
existing names. For example, build `gst-otel-tracer` and set
`GST_TRACERS=mars-instrument`. Metric names and the Pyroscope protocol attribute
`pyroscope.profile.id` are also unchanged.

Debug categories now use the Mars entry-point names (`mars-instrument` shares
`mars-otel`'s debug category). The GObject type names are `MarsInstrumentTracer`,
`MarsMetricsTracer`, `MarsOtelTracer`, `MarsProfilesTracer`, and `MarsNoopTracer`.
OTLP trace/log scopes are named `mars-otel`; the metric scope is
`mars-instrument`. Update any scope/type/debug-category filters accordingly.
The standalone profiler's default application name is now `mars-profiles`;
set its `tracer-name` parameter explicitly to keep an existing application name.

`mars-otel` caches descriptive metadata, captures compact records, and encodes
and exports traces on a worker through a bounded queue with explicit loss
accounting. It uses OpenTelemetry Rust 0.33.1 without creating live SDK spans.
Parent-based sampling preserves decisions across synchronous pushes, queues,
and buffer copies. These improvements work with stock GStreamer.
`mars-instrument` adds cumulative push metrics with trace exemplars using the
same capture stack. An opt-in embedded sampler provides experimental Pyroscope
trace correlation on Linux x86-64; see the
[profile setup and limitations](tracer/otel/README.md#experimental-correlated-cpu-profiles).

See the [trace/profile integration investigation](TRACE_PROFILE_INTEGRATION.md)
for the design rationale, backend constraints, and remaining validation work.

> Currently, the way per element latency is calculated using the `mars-metrics` element does not cleanly handle
> thread-boundaries introduced by the `threadshare` plugin.
>
> However, it should work fine for most pipelines that do not use async runtime powered elements; additionally it works fine across regular thread boundaries introduced by elements like `queue` and `multiqueue`.
>
> A future change will address this issue to more precisely measure latency across elements even in the presence of
> async runtime elements.

## Setup

### Prerequisites

- GStreamer 1.0 libraries and development headers
- GLib development headers
- Rust toolchain (tested with 1.89)
- C compiler and `pkg-config`
- `just` task runner

### Setup options

#### Using the `.setup.sh` script

Run the provided setup script to install dependencies and set up the environment:

```bash
./.devcontainer/setup.sh
```

This will install the necessary GStreamer and GLib development packages, Rust toolchain, just and other dependencies.

However, it is only tested on Ubuntu 24.04, so you may need to adapt it for your system.

#### Using `just`

Install [just](https://github.com/casey/just) task runner, and then run the following command to set up the project:

```bash
just setup
```

#### Using DevContainer

Alternatively, you can use the provided DevContainer setup. This requires Docker and VSCode with the Remote - Containers extension.

## Quickstart

### Prometheus metrics

Build

```bash
just build-package gst-prometheus-tracer
export GST_PLUGIN_PATH="$PWD/target/release/:$GST_PLUGIN_PATH"
export GST_TRACERS='mars-metrics(port=9092)'
export GST_DEBUG=GST_TRACER:5

# Run a GStreamer pipeline to test the tracer
gst-launch-1.0 fakesrc ! identity ! fakesink

# While its running, you can check the metrics at http://localhost:9092/metrics
curl http://localhost:9092/metrics

# Output:
# HELP gst_element_latency_count_count Count of latency measurements per element
# TYPE gst_element_latency_count_count counter
gst_element_latency_count_count{element="fakesink0",path="/GstPipeline:pipeline0",sink_pad="sink",src_pad="src"} 591573
gst_element_latency_count_count{element="identity0",path="/GstPipeline:pipeline0",sink_pad="sink",src_pad="src"} 591573
# HELP gst_element_latency_last_gauge Last latency in nanoseconds per element
# TYPE gst_element_latency_last_gauge gauge
gst_element_latency_last_gauge{element="fakesink0",path="/GstPipeline:pipeline0",sink_pad="sink",src_pad="src"} 5104
.. etc. ..
```

### Correlated telemetry with `mars-instrument`

Enable the metrics and traces pipelines on an OTLP/HTTP collector listening on
port 4318, then run:

```bash
cargo build --release -p gst-otel-tracer
export GST_PLUGIN_PATH="$PWD/target/release/:$GST_PLUGIN_PATH"
export GST_TRACERS=mars-instrument
export OTEL_SERVICE_NAME=gstreamer-pipeline
export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318
export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf
gst-launch-1.0 videotestsrc num-buffers=300 ! videoconvert ! fakesink sync=false
```

Metrics and traces are enabled by default. Histogram exemplars carry trace/span
IDs, allowing a metric observation to link to its trace. Metric collection
continues when traces are sampled out or the trace export queue is full.
Set `GST_INSTRUMENT_TRACES=false` for metrics-only operation.

For CPU profiles on Linux x86-64, rebuild with
`cargo build --release -p gst-otel-tracer --features profiles` and set
`GST_OTEL_PROFILE_URL=http://localhost:4040` before starting the pipeline.
Profiles use the same service identity and label samples with active span IDs.
Enable correlated GStreamer logs with `GST_INSTRUMENT_LOGS=true` and select
categories using `GST_DEBUG`.

Select one of `mars-instrument` or `mars-otel` per process. This first version
covers ordinary push calls; pull mode, buffer lists, and the remaining
`mars-metrics` diagnostics are still separate. See the
[configuration and correlation guide](tracer/otel/INSTRUMENT.md) for signal
controls, metric semantics, memory bounds, backend links, and shutdown behavior,
and the [benchmark results](tracer/otel/bench/INSTRUMENT_RESULTS.md) for measured
overhead and limitations.

### OpenTelemetry traces

With an OTLP/HTTP collector listening on port 4318:

```bash
cargo build --release -p gst-otel-tracer
export GST_PLUGIN_PATH="$PWD/target/release/:$GST_PLUGIN_PATH"
export GST_TRACERS=mars-otel
export OTEL_SERVICE_NAME=gstreamer-pipeline
export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318
export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf
# Sample 10% of new root traces; children inherit their parent's decision.
export OTEL_TRACES_SAMPLER=parentbased_traceidratio
export OTEL_TRACES_SAMPLER_ARG=0.1
gst-launch-1.0 videotestsrc num-buffers=1000 ! videoconvert ! fakesink
```

The default sampler records all new roots. A 0% root ratio still runs tracing
hooks and propagates decisions, and sampled external parents are still honored.
To remove the OTel tracing hooks, omit `mars-otel` from `GST_TRACERS` before
starting the process. See the [OTel README](tracer/otel/README.md) for sampling,
limits, delivery counters, shutdown, and performance measurements.

## Usage

See the individual plugin README files for usage instructions:

- [mars-metrics](tracer/prometheus/README.md)
- [mars-instrument](tracer/otel/INSTRUMENT.md)
- [mars-otel](tracer/otel/README.md)
- [mars-profiles](tracer/pyroscope/README.md)

## Testing

Run the tests using `just` or `cargo`:

```bash
just test
# or
cargo test
# The default member is Prometheus; also run the OTel tests explicitly.
cargo test -p gst-otel-tracer
# Include the embedded profiler and metrics → trace → profile correlation tests.
cargo test -p gst-otel-tracer --features profiles
```

## Roadmap

- [x] Implement a working version of `mars-metrics`, for measuring latency across elements.
- [x] Implement a working version of `mars-otel`, for spans & traces as buffers flow through the pipeline & log traceid correlation.
- [x] Implement a working version of `mars-profiles`, for collecting profiles from gstreamer pipelines.
- [x] Have `mars-metrics` cache pad relationships and metric labels, refreshing them on topology changes to keep per-buffer lookups cheap.
- [x] Have `mars-metrics` latency across elements individually rather than cumulatively across all following elements until next thread boundary or sink element.
- ~~Have `mars-metrics` measure whole-bin elapsed latency across queues and parallel branches.~~ Won't do: input/output matching and completion are ambiguous.
- [x] In `mars-metrics`, add opt-in per-pad input/output buffer counts for muxers and demuxers.
- [x] In `mars-metrics`, distinguish multiple sink/src pads and test branching, merging, and ghost forwarding.
- [x] In `mars-metrics`, measure exclusive pull-mode latency at the upstream element.
- [x] In `mars-metrics`, implement `pad_push_list_pre` and `pad_push_list_post` hooks to capture latency.
- [x] In `mars-otel`, cache descriptions, preserve sampling decisions, and move compact records through bounded queues to worker-side OTLP encoding and export.
- [ ] Further reduce `mars-otel` capture overhead on stock GStreamer; see the [measured limits](tracer/otel/README.md#performance-and-sampling-cost).
- [x] Implement experimental opt-in Pyroscope trace correlation separately from span capture (Linux x86-64).
- [x] Add push metrics with trace/span exemplars through the `mars-instrument` entry point.
- [x] In `mars-profiles`, determine how to get debug symbols for all of gstreamers dependencies.
- [x] Create an experimental `mars-instrument` tracer with shared push capture for correlated metrics, traces, logs and CPU profiles. See [scope and setup](tracer/otel/INSTRUMENT.md).
- [ ] Extend unified capture to pull mode and buffer lists, bring over remaining Prometheus diagnostics, and complete production soak/platform validation.

## License

> This library is distributed under the GNU Library General Public License (LGPL) version 2 or later. See the [LICENSE](LICENSE) file for full details.
