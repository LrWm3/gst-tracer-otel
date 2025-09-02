# Prometheus Tracer Plugin for GStreamer

This folder contains `prom-latency`, a GStreamer tracer plugin that captures per-element pad buffer processing latency and exports these metrics in a Prometheus format.

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
export GST_TRACERS='prom-latency(port=9092)'
export GST_DEBUG=GST_TRACER:5
```

Then launch your pipeline as usual, for example:

```bash
gst-launch-1.0 fakesrc ! identity ! fakesink
```

## Collecting Metrics via HTTP

If you wish to have Prometheus scrape metrics over HTTP, configure the tracer with a `port`:

```bash
export GST_TRACERS='prom-latency(port=9092)'
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

## Collecting Metrics via the `metrics` Signal

> Requires building against GStreamer 1.18 or later to use `gst_tracing_get_active_tracers()`.

Alternatively, you can pull metrics on demand within your application using the `metrics` signal. This allows
for dynamic retrieval of metrics without needing an HTTP server & can be used to merge metrics into upstream
Prometheus exporters.

### In C

```c
#include <gst/gst.h>
#include <gst/tracing/tracer.h>
#include <unistd.h>

int main(int argc, char *argv[]) {
    gst_init(&argc, &argv);

    GstElement *pipeline = gst_parse_launch("videotestsrc ! autovideosink", NULL);
    gst_element_set_state(pipeline, GST_STATE_PLAYING);

    sleep(3);

    const GList *tracers = gst_tracing_get_active_tracers();
    for (const GList *l = tracers; l != NULL; l = l->next) {
        GstTracer *tracer = GST_TRACER(l->data);
        const gchar *name = gst_object_get_name(GST_OBJECT(tracer));

        if (g_str_has_prefix(name, "promlatency")) {
            GstStructure *metrics = gst_tracer_emit(tracer, "metrics", NULL);
            if (metrics) {
                gchar *s = gst_structure_to_string(metrics);
                g_print("Metrics: %s\n", s);
                g_free(s);
                gst_structure_free(metrics);
            } else {
                g_print("No metrics returned.\n");
            }
        }
    }

    gst_element_set_state(pipeline, GST_STATE_NULL);
    gst_object_unref(pipeline);
    return 0;
}

```

### In Rust (glib)

```rust
use std::{thread, time::Duration};
use gstreamer as gst;
use gst::prelude::*;

fn main() {
    gst::init().unwrap();

    let pipeline = gst::parse_launch("videotestsrc ! autovideosink").unwrap();
    pipeline.set_state(gst::State::Playing).unwrap();

    thread::sleep(Duration::from_secs(3));

    if let Some(tracer) = gst::tracing::get_active_tracers()
        .iter()
        .find(|t| t.name().starts_with("promlatency"))
    {
        match tracer.emit("metrics", &[]) {
            Ok(Some(metrics)) => {
                println!("{:?}", metrics);
            }
            Ok(None) => {
                println!("No metrics emitted.");
            }
            Err(err) => {
                eprintln!("Error requesting metrics: {:?}", err);
            }
        }
    } else {
        println!("Latency tracer not found.");
    }

    pipeline.set_state(gst::State::Null).unwrap();
}
```

### In Python

```python
import time
import gi

gi.require_version('Gst', '1.0')
from gi.repository import Gst

Gst.init(None)

pipeline = Gst.parse_launch("videotestsrc ! autovideosink")
pipeline.set_state(Gst.State.PLAYING)

time.sleep(3)

latency_tracer = next((t for t in Gst.tracing_get_active_tracers() if t.get_name().startswith('promlatency')), None)
metrics = latency_tracer.emit("metrics")
print(metrics)
```
