# Gstreamer Prometheus Latency Tracer

A GStreamer `Tracer` plugin that measures per-element pad buffer processing latency and exports these metrics in Prometheus format.

A rust reimagination of [gstlatency.c](https://gitlab.freedesktop.org/gstreamer/gstreamer/-/blob/main/subprojects/gstreamer/plugins/tracers/gstlatency.c) written by [Stefan Sauer](ensonic@users.sf.net), with additional features for Prometheus compatibility.

## Building

### Prerequisites

- Rust toolchain (1.60+)
- GStreamer 1.0 development headers
- GLib development headers
- C compiler and `pkg-config`

```bash
# Build in debug mode
cargo build

# Build in release mode
cargo build --release
# The plugin library is generated at:
# target/release/libgstprometheuslatencytracer.so
```

## Installation

Copy the built plugin into a directory on your GStreamer plugin search path, or update `GST_PLUGIN_PATH`:

```bash
# System-wide install (requires permissions)
sudo cp target/release/libgstprometheuslatencytracer.so /usr/lib/gstreamer-1.0/

# Or for a local setup (debug)
export GST_PLUGIN_PATH="$PWD/target/debug/:$GST_PLUGIN_PATH"

# Or local setup (release)
export GST_PLUGIN_PATH="$PWD/target/release/:$GST_PLUGIN_PATH"
```

## Usage

Enable the tracer by setting the following environment variables before running your pipeline:

```bash
export GST_TRACERS='prometheus-latency-tracer(flags=pipeline+element+reported)'
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

Alternatively, you can pull metrics on demand within your application using the `request-metrics` signal. This allows
for dynamic retrieval of metrics without needing an HTTP server & can be used to merge metrics into upstream
Prometheus exporters.

### In C

```c
GstTracer *tracer = gst_tracer_find("prometheus-latency-tracer");
char *metrics = NULL;
g_signal_emit_by_name(tracer, "request-metrics", &metrics);
printf("%s", metrics);
g_free(metrics);
```

### In Rust (glib)

```rust
if let Some(tracer) = gst::Tracer::get_by_name("prometheus-latency-tracer") {
    let metrics: Option<String> = tracer.emit_by_name("request-metrics", &[]);
    if let Some(output) = metrics {
        println!("{}", output);
    }
}
```

## Hotloop Test

To test the performance of the tracer, you can run a hotloop test. This will create a GStreamer pipeline that continuously processes buffers and measures the latency.

```bash
cargo run --release

# run with no tracer to baseline
gst-launch-1.0 fakesrc num-buffers=1000000 ! fakesink
# Execution ended after 0:00:00.864097614

# run gstlatency to get a sense of the overhead
export GST_TRACERS='latency(flags=pipeline+element+reported)'
export GST_DEBUG=GST_TRACER:5
export GST_PLUGIN_PATH=target/debug/
# Execution ended after 0:00:05.805076558

# run with
export GST_TRACERS='prometheus-latency-tracer(flags=pipeline+element+reported)'
export GST_DEBUG=GST_TRACER:5,prometheus-latency-tracer:5
export GST_PLUGIN_PATH=target/release/
export GST_PROMETHEUS_TRACER_PORT=9092
gst-launch-1.0 fakesrc num-buffers=1000000 ! fakesink
# Execution ended after 0:00:03.027939182
```

## Future work

Would like to switch to otel from prometheus.

## License

This library is distributed under the GNU Library General Public License (LGPL) version 2 or later. See the `LICENSE` file for full details.
