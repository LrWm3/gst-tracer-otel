# Gstreamer Otel Tracers

A collection of GStreamer `Tracer` plugins that measures per-element pad buffer processing latency and exports these metrics in Prometheus format & Otel format.

A rust reimagination of [gstlatency.c](https://gitlab.freedesktop.org/gstreamer/gstreamer/-/blob/main/subprojects/gstreamer/plugins/tracers/gstlatency.c) written by [Stefan Sauer](ensonic@users.sf.net), with additional features for Prometheus & Otel compatibility.

## Plugins available

The table below contains the plugins available in this repository.

| plugin name                                 | description                                                                                                               | performance | stability |
| ------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------- | ----------- | --------- |
| [prom-latency](tracer/prometheus/README.md) | captures per element latencies as prometheus metrics                                                                      | optimized   | alpha     |
| [otel-tracer](tracer/otel/README.md)        | captures per element latencies as otel traces, gst::logs as otel logs, and otel-compatiable metrics with full association | very slow   | pre-alpha |
| [pyroscope](tracer/pyroscope/README.md)     | captures pyroscope profiles for the Gstreamer pipeline, requires debug symbols for best results | optimized   | alpha |
| [noop-latency](tracer/noop/README.md)       | a test plugin, likely not useful for any real purpose                                                                     | slow        | none      |

In general `prom-latency` is recommended for now, and `otel-tracer` is still a work in progress.

> Currently, the way per element latency is calculated using the `prom-latency` element does not cleanly handle
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

Build

```bash
just build-package gst-prometheus-tracer
export GST_PLUGIN_PATH="$PWD/target/release/:$GST_PLUGIN_PATH"
export GST_TRACERS='prom-latency(port=9092)'
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

## Usage

See the individual plugin README files for usage instructions:

- [prom-latency](tracer/prometheus/README.md)
- [otel-tracer](tracer/otel/README.md)
- [pyroscope](tracer/pyroscope/README.md)

## Testing

Run the tests using `just` or `cargo`:

```bash
just test
# or
cargo test
```

## Roadmap

- [x] Implement a working version of `prom-latency`, for measuring latency across elements.
- [x] Implement a working version of `otel-tracer`, for spans & traces as buffers flow through the pipeline & log traceid correlation.
- [x] Implement a working version of `pyroscope`, for collecting profiles from gstreamer pipelines.
- [x] Have `prom-latency` cache pad relationships and metric labels, refreshing them on topology changes to keep per-buffer lookups cheap.
- [x] Have `prom-latency` latency across elements individually rather than cumulatively across all following elements until next thread boundary or sink element.
- [ ] Have `prom-latency` measure whole-bin elapsed latency across queues and parallel branches.
- [x] In `prom-latency`, add opt-in per-pad input/output buffer counts for muxers and demuxers.
- [x] In `prom-latency`, distinguish multiple sink/src pads and test branching, merging, and ghost forwarding.
- [x] In `prom-latency`, measure exclusive pull-mode latency at the upstream element.
- [x] In `prom-latency`, implement `pad_push_list_pre` and `pad_push_list_post` hooks to capture latency.
- [ ] In `otel-tracer`, port performance & implementation improvements made to `prom-latency` such that `otel-tracer` can be used in production with similarly low overhead.
- [ ] `otel-tracer` to collect metrics with trace and span data included in exemplars.
- [x] In `pyroscope`, determine how to get debug symbols for all of gstreamers dependencies.
- [ ] Create an all-in-one tracer, `gst-instrument`, which can collect correlated logs, metrics, spans, traces and profiles with performance improvements & lessons learned from the above.

## License

> This library is distributed under the GNU Library General Public License (LGPL) version 2 or later. See the [LICENSE](LICENSE) file for full details.
