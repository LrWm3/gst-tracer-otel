# Mars GStreamer Tracers

Find where your GStreamer pipeline spends time and CPU. Mars adds metrics,
traces, and CPU profiles through `GST_TRACERS`, without changing your pipeline
code.

## Choose a tracer

| I want to… | Use | Send data to |
| --- | --- | --- |
| Measure element latency, blocking, and queue pressure | [mars-metrics](tracer/prometheus/README.md) | Prometheus |
| Collect CPU call stack profiles | [mars-profiles](tracer/otel/PROFILES.md) | Pyroscope |
| Trace buffer pushes across spans | [mars-otel](tracer/otel/README.md) | OpenTelemetry collector |
| Collect correlated metrics, traces, spans, logs and profiles | [mars-instrument](tracer/otel/INSTRUMENT.md) | All-in-one metrics |

Start with **mars-metrics** if you are unsure, as it carries the least overhead of these options.

For a list of available metrics, see the [prometheus plugin README.md](https://github.com/LrWm3/gst-tracer-otel/blob/main/tracer/prometheus/README.md#metric-semantics).

See the [performance reports](reports/README.md) for measured CPU and memory costs on the tested pipelines for each tracer.

## Get your first metrics

You need Rust/Cargo, a C compiler, `pkg-config`, GStreamer and GLib development
headers, GStreamer command-line tools and base plugins, and `curl`. See
[environment setup](CONTRIBUTING.md#environment) for the provided development
container and setup helpers. `just` is optional.

From the repository root, build the metrics plugin and run a live test pipeline:

```sh
cargo build --release -p gst-prometheus-tracer

GST_PLUGIN_PATH="$PWD/target/release" \
GST_TRACERS='mars-metrics(port=9092)' \
  gst-launch-1.0 videotestsrc is-live=true ! videoconvert ! fakesink
```

Leave the pipeline running. In a **second terminal**, fetch its metrics:

```sh
curl http://localhost:9092/metrics
```

Look for `gst_element_latency_count_count` and
`gst_element_sync_wall_seconds_bucket`, with labels identifying pipeline elements.
No Prometheus server is needed to try this; to retain measurements, configure
Prometheus to scrape this endpoint. The metrics server listens on all interfaces
on the chosen port.

To instrument your application, set the same `GST_PLUGIN_PATH` and `GST_TRACERS`
when launching it. Replace the example pipeline with your own, or see the
[metrics guide](tracer/prometheus/README.md) for element filters, sampling,
queue metrics, and PromQL examples.

If the tracer does not load, check that GStreamer can find it:

```sh
GST_PLUGIN_PATH="$PWD/target/release" gst-inspect-1.0 mars-metrics
```

## Add traces or profiles

Each guide includes its build command, backend requirements, and configuration:

- [Combined instrumentation](tracer/otel/INSTRUMENT.md): metrics and traces
  together, with optional correlated logs and CPU profiles.
- [OpenTelemetry traces](tracer/otel/README.md): push-call spans, trace sampling,
  and optional correlated logs and profiles.
- [CPU profiles](tracer/otel/PROFILES.md): profiling alone, including frame
  pointers and debug symbols for useful call stacks.

Choose one of `mars-instrument`, `mars-otel`, or `mars-profiles` per process;
they share one configuration. Profiling requires the `profiles` build feature.

## Know what you are measuring

- **Metrics assume synchronous calls.** Metrics may still hint at non-sync latency
  depending on if the elements underlying queues are saturated.
- **Metrics do not support bin elements.** Latency is measured across bin sub-elements.
  If you see elements you do not recognize from your pipeline, it is possible it is from
  a bin element.
- **No async support.** `mars-metrics` supports push, pull, and buffer
  lists; combined instrumentation currently covers ordinary pushes. Regular
  `queue` and `multiqueue` thread boundaries are supported, but `threadshare`
  asynchronous boundaries are not handled correctly by the element timings.
  If this is important to you, please open an issue and it can be explored.
- **CPU profiling currently requires Linux x86-64.** It samples participating
  streaming threads, with [stack and shutdown limitations](tracer/otel/PROFILES.md#sampling-and-shutdown).
  This may be extended in the future, raise an issue if this is important to you.

## Further reading

- [Performance reports](reports/README.md): measured overhead and reproduction instructions.
- [Contributing](CONTRIBUTING.md): development setup, tests, benchmarks, and remaining work.

## License and credits

Distributed under the GNU Library General Public License (LGPL), version 2 or
later. See [LICENSE](LICENSE).

Inspired by GStreamer's [gstlatency.c](https://gitlab.freedesktop.org/gstreamer/gstreamer/-/blob/main/subprojects/gstreamer/plugins/tracers/gstlatency.c),
written by [Stefan Sauer](mailto:ensonic@users.sf.net), with Rust implementations
and Prometheus, OpenTelemetry, and Pyroscope integration.
