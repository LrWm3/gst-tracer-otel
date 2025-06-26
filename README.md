# Prometheus Latency Tracer

A GStreamer `Tracer` plugin that measures per-element pad buffer processing latency and exports these metrics in Prometheus format.

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
export GST_DEBUG=GST_TRACER:7

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

## License

This library is distributed under the GNU Library General Public License (LGPL) version 2 or later. See the `LICENSE` file for full details.
