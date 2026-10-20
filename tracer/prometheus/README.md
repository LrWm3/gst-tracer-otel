# Prometheus Tracer Plugin for GStreamer

This folder contains `prom-latency`, a GStreamer tracer plugin that exports
synchronous push/pull cost, queue pressure, and optional I/O counts in Prometheus format.

The synchronous metrics time `gst_pad_push()` from an upstream source pad until
the downstream call returns. Direct child pushes are subtracted, including
multiple sequential tee branches. This is useful for separating CPU work from
blocking/backpressure, but it is not queue residence time, GPU completion time,
or end-to-end source latency. Pull calls are timed separately and attributed to
the upstream element servicing the request.

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

### Histogram Sampling

Histogram observation can be sampled using:

```bash
export GST_PROM_LATENCY_HISTOGRAM_SAMPLE_RATE=1000
```

- `16` (default): pseudo-randomly observe approximately one event in 16.
- `1`: observe every event.
- `N > 1`: observe approximately one event in `N`, without periodic batch aliasing.
- This sampling affects `gst_element_sync_wall_seconds`,
  `gst_element_sync_cpu_seconds`, and `gst_element_sync_blocked_seconds`.
- `gst_element_latency_last_gauge`, `gst_element_latency_sum_count`, and `gst_element_latency_count_count` are always recorded for every latency event.

Buckets begin at one microsecond so short decoder, inference, and tracker stages
do not collapse into one millisecond bucket.

### Element Selection

Metric updates dominate tracer overhead in buffer-heavy pipelines. Restrict
synchronous metrics to useful downstream element names with comma-separated
exact names or simple `*` wildcards:

```bash
export GST_PROM_LATENCY_INCLUDE='bench-decoder,*infer*,*tracker,*publish'
```

Unselected child calls are still subtracted from a selected parent. If no
selected parent is active, they skip stack and Prometheus work.

### Input and Output Counts

Enable per-pad buffer counts with:

```bash
export GST_PROM_LATENCY_IO_COUNTS=true
```

`gst_element_buffers_in_total` and `gst_element_buffers_out_total` use `element`,
`pad`, and `path` labels. Pushes count buffers offered to the real sink and
submitted by the real source, including failed pushes. Pulls count buffers only
when a buffer is returned successfully. A buffer list contributes its length;
the legacy latency count still counts calls. These counters describe pad traffic,
not successful decoding or processing. They obey `GST_PROM_LATENCY_INCLUDE`.

Counts are disabled by default to avoid extra counter updates per buffer.
An observed element exports zero counts for its other real pads until they carry data.
Ghost forwarding is counted once at each real endpoint. Exact pad names keep
request pads such as `sink_100` and `sink_101` separate. Muxers and demuxers can
change buffer counts, so an input/output difference alone does not prove a drop.

### Pull-Mode Latency

Pull calls export `gst_element_pull_wall_seconds`, `gst_element_pull_cpu_seconds`,
`gst_element_pull_blocked_seconds`, and the unsampled `gst_element_pull_calls_total`.
`gst_element_pull_last_nanoseconds` and `gst_element_pull_wall_nanoseconds_total`
provide unsampled last/sum values. The histogram sample rate and element filter
also apply here. Labels identify the upstream element servicing the pull and
the real source/sink pads. Nested
pull and push work is subtracted from its caller, including ghost-pad forwarding.
EOS and failed calls contribute timing and call counts, but no I/O buffer count.
Existing push metrics keep their names and meaning.

### Live Buffer Age

For live pipelines whose PTS shares pipeline running-time semantics, enable a
sampled age measurement:

```bash
export GST_PROM_LATENCY_PTS_AGE=true
```

`gst_buffer_pts_age_seconds` measures current pipeline running-time minus
segment-adjusted buffer PTS at each selected element input. Keep this disabled
for unpaced file benchmarks and sources with unrelated timestamp domains.
Future PTS values are clamped to zero age. This is not camera wall-clock latency.
Buffer-list pushes do not currently contribute PTS-age samples.

### Queue Sampling

`queue` and `queue2` levels are polled by one background thread. No per-buffer
queue probe is added. The default interval is 100 ms:

```bash
export GST_PROM_LATENCY_QUEUE_SAMPLE_INTERVAL_MS=100
```

The tracer exports current buffer, byte, and media-time levels plus overrun and
underrun counters. Gauges answer whether backlog exists. They do not provide a
per-buffer queue-residence distribution.
Only `queue` provides overrun/underrun signals; zero counters on `queue2` do not
prove absence of pressure. Signal callbacks increment atomic counters, exported
by the sampler. Levels can miss spikes between polls, and Prometheus can miss
polled peaks between scrapes. These gauges cannot establish an exact worst-case
queue capacity. Queue labels are initialized after the element reaches PAUSED
and refreshed after reparenting or renaming.

### Metric Semantics

| Metric | Meaning |
| --- | --- |
| `gst_element_sync_wall_seconds` | Exclusive synchronous wall time |
| `gst_element_sync_cpu_seconds` | Exclusive streaming-thread CPU time |
| `gst_element_sync_blocked_seconds` | `wall - CPU`; wait, scheduling, or backpressure |
| `gst_element_pull_{wall,cpu,blocked}_seconds` | Exclusive synchronous upstream pull timing |
| `gst_element_pull_calls_total` | Pull calls, including EOS and errors |
| `gst_element_buffers_{in,out}_total` | Opt-in buffer counts per real pad |
| `gst_buffer_pts_age_seconds` | Opt-in sampled live buffer age at selected inputs |
| `gst_queue_current_level_*` | Sampled queue backlog |
| `gst_queue_overruns_total` | Queue full signals |
| `gst_queue_underruns_total` | Queue empty signals |

Asynchronous CUDA/VIC/TensorRT completion requires native metrics at submission
and completion points. Generic pad hooks cannot infer that interval correctly.
CPU and wall clocks are read sequentially, not simultaneously. Nested tracer
bookkeeping also contributes noise, particularly for microsecond-scale stages.
`blocked` is an estimate of off-CPU wall time, not a measurement of a particular
lock or GPU fence. Failed CPU clock reads omit CPU/blocked observations instead
of inventing a zero-CPU sample. Push and pull histograms describe synchronous
calls, including failed calls.

Compatibility: the three legacy gauge/sum/count names remain. The former
`gst_element_latency_seconds_histogram` was replaced by
`gst_element_sync_wall_seconds`; histogram queries must migrate to that name.

### PromQL Examples

Sampled p95 synchronous wall time:

```promql
histogram_quantile(
  0.95,
  sum by (le, element) (rate(gst_element_sync_wall_seconds_bucket[5m]))
)
```

Legacy unsampled average synchronous wall time, converted from nanoseconds to
seconds:

```promql
sum by (element) (rate(gst_element_latency_sum_count[5m]))
/
sum by (element) (rate(gst_element_latency_count_count[5m]))
/
1e9
```

Do not graph `rate(gst_element_latency_sum_count[5m])` alone as latency. That
value is accumulated nanoseconds per second. Likewise,
`rate(gst_element_latency_count_count[5m])` is event throughput, not latency.

Maximum scraped queue depth over five minutes (not every intervening peak):

```promql
max_over_time(gst_queue_current_level_buffers[5m])
```

### Validation

Controlled pipelines confirmed these boundaries:

- `identity sleep-time=10000` reports about 10 ms wall and blocked time, with
  much lower thread CPU time.
- A one-buffer queue feeding a slow consumer reports admission blocking.
- A deep queue feeding the same consumer reports low synchronous push time but
  rising `gst_queue_current_level_buffers`, proving push time is not residence
  time.
- Multiple sequential tee branches are accumulated and subtracted from their
  parent instead of retaining only the final child span.

Pad labels are resolved on first use and refreshed after relinking, reparenting,
renaming, pad changes, or a READY-to-PAUSED transition. Ghost-pad forwarding
records each real sink once. Series for destroyed objects or old paths are
retired on scrape or within one second. Stopped pipelines retain series for
pads and elements that remain alive.

An earlier exploratory microbenchmark on an integrated memory nvidia device measured 2.857 s versus 3.043 s,
but used different element-selection configurations. It does not establish
matched per-edge overhead or a CPU-percentage estimate. Subsequent
[matched benchmarks on an integrated memory nvidia device](BENCHMARK.md) compare tracer-off, original,
pre-review and reviewed builds with identical instrumentation settings and
actual scraping. Short processor runs showed no material throughput penalty;
the buffer-only loop did reveal additional instrumentation cost. The report
separates build versions, startup fixes and an off-run shutdown failure.
Long-soak and rare-tail qualification remain outstanding.

See the [benchmark report](BENCHMARK.md) for local comparisons of the cache
changes, I/O counts, and graph cleanup, with raw results and reproduction commands.

See [Rust review and cleanup plan](REVIEW.md) for fixes and remaining limitations.

### Example Output

```plaintext
# HELP gst_element_latency_count_count Count of latency measurements per element
# TYPE gst_element_latency_count_count counter
gst_element_latency_count_count{element="fakesink0",path="/GstPipeline:pipeline0",sink_pad="sink",src_pad="src"} 591573
gst_element_latency_count_count{element="identity0",path="/GstPipeline:pipeline0",sink_pad="sink",src_pad="src"} 591573
# HELP gst_element_latency_last_gauge Last latency in nanoseconds per element
# TYPE gst_element_latency_last_gauge gauge
gst_element_latency_last_gauge{element="fakesink0",path="/GstPipeline:pipeline0",sink_pad="sink",src_pad="src"} 5104
gst_element_latency_last_gauge{element="identity0",path="/GstPipeline:pipeline0",sink_pad="sink",src_pad="src"} 14423
# HELP gst_element_latency_sum_count Sum of latencies in nanoseconds per element
# TYPE gst_element_latency_sum_count counter
gst_element_latency_sum_count{element="fakesink0",path="/GstPipeline:pipeline0",sink_pad="sink",src_pad="src"} 3036567246
gst_element_latency_sum_count{element="identity0",path="/GstPipeline:pipeline0",sink_pad="sink",src_pad="src"} 7819315483
# HELP gst_element_sync_wall_seconds Sampled exclusive synchronous gst_pad_push wall time in seconds
# TYPE gst_element_sync_wall_seconds histogram
gst_element_sync_wall_seconds_bucket{element="identity0",path="/GstPipeline:pipeline0",sink_pad="sink",src_pad="src",le="0.0001"} 36973
gst_element_sync_wall_seconds_sum{element="identity0",path="/GstPipeline:pipeline0",sink_pad="sink",src_pad="src"} 0.488707
gst_element_sync_wall_seconds_count{element="identity0",path="/GstPipeline:pipeline0",sink_pad="sink",src_pad="src"} 36973
# HELP gst_queue_current_level_buffers Current number of buffers held by a GStreamer queue
# TYPE gst_queue_current_level_buffers gauge
gst_queue_current_level_buffers{element="infer_queue",path="/GstPipeline:pipeline0/GstQueue:infer_queue"} 2
```

## Collecting Metrics via the `metrics` Signal

> Requires building against GStreamer 1.18 or later to use `gst_tracing_get_active_tracers()`.

Alternatively, you can pull metrics on demand within your application using the `metrics` signal. This allows
for dynamic retrieval of metrics without needing an HTTP server & can be used to merge metrics into upstream
Prometheus exporters.

### In C

```c
#include <gst/gst.h>

int main(int argc, char *argv[]) {
    gst_init(&argc, &argv);
    GstElement *pipeline = gst_parse_launch("fakesrc num-buffers=10 ! fakesink", NULL);
    gst_element_set_state(pipeline, GST_STATE_PLAYING);
    GstBus *bus = gst_element_get_bus(pipeline);
    GstMessage *message = gst_bus_timed_pop_filtered(
        bus, 5 * GST_SECOND, GST_MESSAGE_EOS | GST_MESSAGE_ERROR);
    if (message) gst_message_unref(message);
    gst_object_unref(bus);

    GList *tracers = gst_tracing_get_active_tracers();
    for (GList *l = tracers; l != NULL; l = l->next) {
        if (g_signal_lookup("metrics", G_OBJECT_TYPE(l->data))) {
            gchar *metrics = NULL;
            g_signal_emit_by_name(l->data, "metrics", &metrics);
            if (metrics) g_print("%s", metrics);
            g_free(metrics);
        }
    }
    g_list_free_full(tracers, gst_object_unref);
    gst_element_set_state(pipeline, GST_STATE_NULL);
    gst_object_unref(pipeline);
    return 0;
}
```

### In Rust (glib)

```rust
use gstreamer as gst;
use gst::prelude::*;

fn main() {
    gst::init().unwrap();
    let pipeline = gst::parse::launch("fakesrc num-buffers=10 ! fakesink").unwrap();
    pipeline.set_state(gst::State::Playing).unwrap();
    let bus = pipeline.bus().unwrap();
    bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(5),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    ).expect("pipeline timed out");

    for tracer in gst::active_tracers() {
        if tracer.type_().name() == "promlatencytracer" {
            if let Some(metrics) = tracer.emit_by_name::<Option<String>>("metrics", &[]) {
                print!("{metrics}");
            }
        }
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
