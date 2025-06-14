## 📄 README.md ― *gst-otel-tracer*

> **GStreamer ⇄ OpenTelemetry bridge**
> A drop-in `GstTracer` plug-in that turns every GStreamer pipeline into a
> first-class OpenTelemetry (OTLP) data source, emitting **traces, metrics and
> exemplars** with nanosecond precision.

---

### Table of Contents

1. [Features](#features)
2. [Quick start](#quick-start)
3. [Building from source](#building-from-source)
4. [Running](#running)
5. [Signals emitted](#signals-emitted)
6. [Tuning & environment variables](#tuning--environment-variables)
7. [FAQ](#faq)
8. [License](#license)

---

### Features

| Signal             | Details                                                                                  |
| ------------------ | ---------------------------------------------------------------------------------------- |
| **Histogram**      | `gstreamer.element.latency.ns` – raw nanoseconds, explicit buckets from **100 ns → 1 s** |
| **Exemplars**      | attached automatically to hot latency buckets, link back to per-buffer **PadPush** spans |
| **Spans / events** | *Pipeline → Bin → Element → PadPush* hierarchy with state-change events                  |
| **Sampling**       | Head-based sampler for PadPush spans (`1 / 1000` by default)                             |
| **Zero parsing**   | Uses GStreamer core hooks – no log scraping, < 1 µs overhead at 60 fps                   |

---

### Quick start

```bash
# ❶ Build
cargo build --release
export GST_PLUGIN_PATH="$(pwd)/target/release"

# ❷ Point to your OpenTelemetry Collector
export OTEL_SERVICE_NAME=gst-pipeline
export OTEL_EXPORTER_OTLP_ENDPOINT="http://otel-collector:4317"

# ❸ Run any pipeline with the tracer enabled
GST_TRACERS=otel-tracer \
gst-launch-1.0 videotestsrc is-live=true ! videoconvert ! autovideosink
```

Open the collector UI (Grafana, Tempo, Honeycomb, etc.) and search for
`service.name = "gst-pipeline"` – you will see:

* A **root span** representing the pipeline lifetime
* Child spans for each bin / element
* A live histogram of element latencies with exemplar dots that pivot straight
  into the `PadPush` span for the corresponding buffer

---

### Building from source

```bash
# Prerequisites: Rust ≥ 1.75, pkg-config, GStreamer dev headers (>= 1.20)
sudo apt install libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev

git clone https://github.com/your-org/gst-otel-tracer.git
cd gst-otel-tracer
cargo build --release
```

The build produces **`target/release/libgst_otel_tracer.so`** (Linux) or
`.dylib` / `.dll` on other platforms.  Copy or add its directory to
`GST_PLUGIN_PATH`.

---

### Running

#### Minimal pipeline

```bash
GST_TRACERS=otel-tracer \
gst-launch-1.0 fakesrc num-buffers=1000 ! fakesink sync=false
```

#### Real-world example (RTP encode)

```bash
GST_TRACERS=otel-tracer \
gst-launch-1.0 -e \
  udpsrc port=5000 caps="application/x-rtp" ! rtpbin.recv_rtp_sink_0   \
  rtpbin. ! rtph264depay ! h264parse ! nvh264dec ! videoconvert         \
         ! autovideosink
```

Latency spikes in `nvh264dec` immediately appear in the histogram; exemplars let
you jump to the outlier frame’s PadPush span.

---

### Signals emitted

| Metric / Span                    | Unit | Labels                                     |
| -------------------------------- | ---- | ------------------------------------------ |
| `gstreamer.element.latency.ns`   | ns   | `element`, pipeline/host/resource attr set |
| `gstreamer.pad.frames.count`¹    | 1    | `pad.direction`, `element`                 |
| `gstreamer.buffer.dropped`¹      | 1    | `reason`, `element`                        |
| **Spans**: `PadPush` / `PadPull` | —    | `pad.direction`, `element`, `thread.id`    |

¹ Counters are wired in the source but commented out – uncomment if needed.

---

### Tuning & environment variables

| Variable                      | Default   | Meaning                                        |
| ----------------------------- | --------- | ---------------------------------------------- |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | —         | OTLP-gRPC endpoint (`http://host:4317`)        |
| `OTEL_SERVICE_NAME`           | `gst-app` | Service name resource attribute                |
| `GST_OTEL_PAD_SAMPLING`       | `1000`    | Head sampler denominator (`1/N` PadPush spans) |
| `GST_DEBUG`                   | —         | Set to `GST_TRACER:7` for verbose hook logs    |

Change the sampling rate at runtime:

```bash
GST_OTEL_PAD_SAMPLING=100 \
GST_TRACERS=otel-tracer gst-launch-1.0 …
```

---

### FAQ

| Q                                    | A                                                                                                                                                  |
| ------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Does it work on Windows & macOS?** | Yes – GStreamer & Tokio are cross-platform.  Ensure you ship the `.dll` / `.dylib` and set `GST_PLUGIN_PATH`.                                      |
| **Collectorless dev setup?**         | Run `otel-collector --config examples/otel-stdout.yaml` to print spans & metrics to stdout.                                                        |
| **High-FPS pipeline overhead?**      | ELEMENT\_LATENCY hook costs \~0.4 µs; sampled PadPush spans add < 0.1 µs avg. Disable PadPush spans entirely by setting `GST_OTEL_PAD_SAMPLING=0`. |
| **Can I add my own attributes?**     | Fork `src/lib.rs`, edit the `attrs` arrays; rebuild.                                                                                               |
| **Prometheus instead of OTLP?**      | See the `prometheus` branch for a Prom-native variant.                                                                                             |
