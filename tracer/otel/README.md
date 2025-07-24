# OpenTelemetry Tracer for GStreamer

This folder contains the implementation of an OpenTelemetry (OTLP) tracer for GStreamer, enabling distributed tracing and metrics collection for GStreamer pipelines using the OpenTelemetry protocol.

## Overview

This tracer integrates with GStreamer to provide observability features such as:

- OpenTelemetry spans for GStreamer element pads and traces following Gstreamer buffers.
- Records buffer flow and latency as distributed traces.
- Exports traces via OTLP (HTTP).
- Integrates with GStreamer's logging system for structured logs, with logs containing trace and span context for correlation with traces.
- Supports custom attributes for spans, including pad names, element names, buffer IDs, timestamps, and thread information.
- Future work will include metric collection and export with exemplars for correlation with traces and logs.

## Warning

This plugin was an early experiment when I was still learning the basics of Rust, Gstreamer and OpenTelemetry.

The code is poorly structured, unoptimized, and has many allocations on the critical path. It is not recommended for production use.

It is extraordinarily slow compared to `prom-latency`. It is primarily intended for educational purposes and to demonstrate how one could to integrate OpenTelemetry with GStreamer in a meaningful way.

The [tracing-gstreamer](https://github.com/standard-ai/tracing-gstreamer) repository partially inspired this work, as it
allows for otel traces to be collected via the tracing framework & using an otel bridge, but this was done in such a way that did not feel particularly idiomatic to me from the perspective of a GStreamer developer.

Additionally, I need to revisit latency calculations for this plugin to ensure they are correct, as they were
incorrect in `prom-latency` and were only recently fixed.

Once I am done improving `prom-latency`, I will revisit this plugin to ensure it is correct and performant and
will remove this warning.

## How it works

This plugin uses the OpenTelemetry Rust SDK to create spans for GStreamer elements and their pads. It captures buffer flow through the pipeline, recording timestamps and other metadata. The spans are exported using the OTLP exporter, which can be configured to send data to an OpenTelemetry collector or backend.

Tracing in gstreamer is complicated by the thread management and by the fact that buffers may be artibrarily destroyed, reallocted, or bundled. The tracer handles these complexities by:

- Creating spans for each GStreamer element and its pads.
- Attaching spans to pad Quarks to allow for later retrieval of span information.
- Using Otelmetry's `Context` to manage trace context within thread local storage, allowing for the propagation of active span context within the context of a single thread.
- Using GStreamer buffer metadata to propagate trace context to relate parent and child spans across thread boundaries.

## Installation

First build the plugin:

```bash
# from the root of this repository
cd $(git rev-parse --show-toplevel)

# Build the otel-tracer plugin
cargo build --release -p gst-otel-tracer

# or from anywhere
just build-package gst-otel-tracer
```

Then copy the built plugin into a directory on your GStreamer plugin search path, or update `GST_PLUGIN_PATH`:

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
export GST_PLUGIN_PATH="$PWD/target/release/:$PWD/target/debug/:$GST_PLUGIN_PATH"
export GST_TRACERS='otel-tracer(flags=element)'
# Optionally set GST_DEBUG to see debug logs
# Leave GST_DEBUG unset in production
export GST_DEBUG=GST_TRACER:5,otel-tracer:6

# Configure the OpenTelemetry exporter
# See Otel official documentation for more options: https://opentelemetry.io/docs/specs/otel/configuration/sdk-environment-variables/#general-sdk-configuration
export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318
export OTEL_EXPORTER_OTLP_PROTOCOL=http
export OTEL_SERVICE_NAME=gstreamer-pipeline
```

Deploy a OpenTelemetry collector to receive the traces

```bash
# Run a local OpenTelemetry collector with Grafana as the UI
# 4318 is http otel which we are using; 4040 is profiles and 4317 is grpc otel
# grpc otel has not been tested yet.
docker run -p 3000:3000 -p 4040:4040 -p 4317:4317 -p 4318:4318 -d grafana/otel-lgtm
```

Then launch your pipeline as usual, for example:

```bash
gst-launch-1.0 videotestsrc ! videoconvert ! autovideosink
```

Now navigate to `http://localhost:3000` to access the Grafana UI and view your traces and logs.
