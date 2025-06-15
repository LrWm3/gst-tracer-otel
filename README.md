# gst-otel-tracer

A [GStreamer](https://gstreamer.freedesktop.org/) tracer plugin that exports pad-level latency and flow telemetry to [OpenTelemetry](https://opentelemetry.io/), allowing you to visualize and analyze media pipeline performance using observability tools.

**Features:**

- Instruments `pad-push`, `pad-push-list`, and `pad-pull-range` hooks.
- Emits:
  - Spans (`PadPush`) with pad metadata.
  - Histogram metrics for latency (`gstreamer.pad.latency.ns`).
- Sampling ratio controlled via environment.
- Full support for OpenTelemetry OTLP environment variables (e.g. endpoint, headers).

## ⚙️ Usage

### Build

```bash
cargo build --release
````

Ensure the resulting `.so` is on your plugin path:

```bash
export GST_PLUGIN_PATH=$(pwd)/target/release
```

### Run with Tracing

```bash
export GST_TRACERS=oteltracer
export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317
export OTEL_TRACES_SAMPLING_RATIO=1.0  # or 0.01, etc.

gst-launch-1.0 fakesrc num-buffers=3000 ! fakesink
```

Use with any GStreamer pipeline — all pad pushes and pulls will be traced.

## 📊 Collector Setup

Use a local OpenTelemetry Collector or compatible backend (e.g. Grafana Tempo, Honeycomb, Lightstep). Example collector config can be found in the [OpenTelemetry Collector docs](https://opentelemetry.io/docs/collector/).

## 🔧 Environment Variables

| Variable                      | Description                                              |
| ----------------------------- | -------------------------------------------------------- |
| `OTEL_TRACES_SAMPLING_RATIO`  | Fraction of spans to sample (e.g. `0.01`)                |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | OTLP gRPC endpoint (e.g. `http://localhost:4317`)        |
| `GST_TRACERS`                 | Set to `oteltracer` to activate this plugin              |
| `GST_PLUGIN_PATH`             | Location of the compiled `.so` if not globally installed |

## 🛠 Architecture

* Uses [`gst::Tracer`](https://gstreamer.freedesktop.org/documentation/plugin-development/advanced/tracing.html) to hook into pad activity.
* Buffers pad events and timestamps to correlate latency.
* Uses [`opentelemetry-sdk`](https://docs.rs/opentelemetry-sdk) and [`opentelemetry-otlp`](https://docs.rs/opentelemetry-otlp) with Tokio.
* Efficient: minimal allocations, per-pad attribute caching.

## 📄 License

This project is licensed under the [Mozilla Public License 2.0](https://www.mozilla.org/en-US/MPL/2.0/).
