# Gstreamer Otel Tracers

A collection of GStreamer `Tracer` plugins that measures per-element pad buffer processing latency and exports these metrics in Prometheus format & Otel format.

A rust reimagination of [gstlatency.c](https://gitlab.freedesktop.org/gstreamer/gstreamer/-/blob/main/subprojects/gstreamer/plugins/tracers/gstlatency.c) written by [Stefan Sauer](ensonic@users.sf.net), with additional features for Prometheus & Otel compatibility.

## Plugins available

The table below contains the plugins available in this repository.

| plugin name  | description                                                                                                               | performance  | stability |
| ------------ | ------------------------------------------------------------------------------------------------------------------------- | ------------ | --------- |
| prom-latency | captures per element latencies as prometheus metrics                                                                      | optimized    | alpha     |
| otel-tracer  | captures per element latencies as otel traces, gst::logs as otel logs, and otel-compatiable metrics with full association | very slow    | pre-alpha |
| noop-latency | a test plugin, likely not useful for any real purpose                                                                     | slow         | none      |

> Currently, the way per element latency is calculated is confusing in that it captures all
> latency between the measured element and the next thread boundary, such as a queue, or a sink element.
>
> A future change will address this issue to capture latency across the element in question only, but for now
> prom-latency and otel-tracer serve more as a POC that this is possible.

## Setup

### Prerequisites

- GStreamer 1.0 libraries and development headers
- GLib development headers
- Rust toolchain (1.60+)
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

## Building

The plugins can be built with the command below:

```bash
just build
# or
cargo build

# individually build only the plugin(s) you want
cargo build -p gst-prometheus-tracer
# or
cargo build -p gst-otel-tracer
```

If using in production, building in release mode is recommended.

## Installation

Copy the built plugin into a directory on your GStreamer plugin search path, or update `GST_PLUGIN_PATH`:

```bash
# System-wide install (requires permissions)
sudo cp target/release/libgst*.so /usr/lib/gstreamer-1.0/

# Or for a local setup (debug)
export GST_PLUGIN_PATH="$PWD/target/debug/:$GST_PLUGIN_PATH"

# Or local setup (release)
export GST_PLUGIN_PATH="$PWD/target/release/:$GST_PLUGIN_PATH"
```

## Usage

Enable the tracer by setting the following environment variables before running your pipeline:

```bash
export GST_TRACERS='prom-latency(flags=element)'
export GST_DEBUG=GST_TRACER:5

# Optionally, set the tracer to expose metrics over a specific port
# If not set, it will not expose metrics over HTTP
export GST_PROMETHEUS_TRACER_PORT=9092
```

Then launch your pipeline as usual, for example:

```bash
gst-launch-1.0 videotestsrc ! videoconvert ! autovideosink
```

## Collecting Metrics via HTTP

If you wish to have Prometheus scrape metrics over HTTP, set `GST_PROMETHEUS_TRACER_PORT` to a valid port number:

```bash
export GST_PROMETHEUS_TRACER_PORT=9092
```

The plugin will spawn an HTTP server on `0.0.0.0:9092`. To retrieve metrics:

```bash
curl http://localhost:9092
```

### Example Output

```plaintext
# HELP gstreamer_element_latency_count_count Count of latency measurements per element
# TYPE gstreamer_element_latency_count_count counter
gstreamer_element_latency_count_count{element="fakesink0",sink_pad="fakesink0.sink",src_pad="identity0.src"} 591573
gstreamer_element_latency_count_count{element="identity0",sink_pad="identity0.sink",src_pad="fakesrc0.src"} 591573
# HELP gstreamer_element_latency_last_gauge Last latency in nanoseconds per element
# TYPE gstreamer_element_latency_last_gauge gauge
gstreamer_element_latency_last_gauge{element="fakesink0",sink_pad="fakesink0.sink",src_pad="identity0.src"} 5104
gstreamer_element_latency_last_gauge{element="identity0",sink_pad="identity0.sink",src_pad="fakesrc0.src"} 14423
# HELP gstreamer_element_latency_sum_count Sum of latencies in nanoseconds per element
# TYPE gstreamer_element_latency_sum_count counter
gstreamer_element_latency_sum_count{element="fakesink0",sink_pad="fakesink0.sink",src_pad="identity0.src"} 3036567246
gstreamer_element_latency_sum_count{element="identity0",sink_pad="identity0.sink",src_pad="fakesrc0.src"} 7819315483
```

## Collecting Metrics via the `request-metrics` Signal

> Requires building against GStreamer 1.18 or later.

Alternatively, you can pull metrics on demand within your application using the `request-metrics` signal. This allows
for dynamic retrieval of metrics without needing an HTTP server & can be used to merge metrics into upstream
Prometheus exporters.

### In C

```c
GstTracer *tracer = gst_tracer_find("prom-latency");
char *metrics = NULL;
g_signal_emit_by_name(tracer, "request-metrics", &metrics);
printf("%s", metrics);
g_free(metrics);
```

### In Rust (glib)

```rust
if let Some(tracer) = gst::Tracer::get_by_name("prom-latency") {
    let metrics: Option<String> = tracer.emit_by_name("request-metrics", &[]);
    if let Some(output) = metrics {
        println!("{}", output);
    }
}
```

## Testing

Run the tests using `just` or `cargo`:

```bash
just test
# or
cargo test
```

## Ongoing work

- [x] Cache relationship information on `pad_link_post` and `pad_unlink_post` to minimize the `pad_push_pre` and `pad_push_post` look-up time.
- [ ] Measure latency across elements individually rather than cumulatively across all following elements until next thread boundary or sink element.
- [ ] Port performance improvements made to prom-latency to the otel plugin.
- [ ] Support latency measurements across bin elements.
- [ ] Split count metric into `buf_in_count` and `buf_out_count` to capture behavior of muxer & demuxer elements.
- [ ] Better support latency measurements for elements and bins with multiple sink and src pads.
- [ ] Reimplement `pad_pull_pre` and `pad_pull_post` hooks to properly capture latency (unsure exactly how this will look at this point).

## License

This library is distributed under the GNU Library General Public License (LGPL) version 2 or later. See the `LICENSE` file for full details.
