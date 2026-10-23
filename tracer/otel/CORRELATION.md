# Link traces and CPU profiles in Grafana

Mars emits the correlation IDs automatically. Grafana needs a one-time data-source
configuration to turn them into links. Use `mars-otel` or `mars-instrument` with
[CPU profiling enabled](README.md#experimental-correlated-cpu-profiles), send
traces to Tempo, and send profiles to Pyroscope. `mars-profiles` alone emits no
traces, although its element names still appear in flame graphs.

## Configure your existing Grafana

Add both Tempo and Pyroscope as data sources. In **Connections → Data sources →
your Tempo data source → Traces to profiles**, select:

| Setting | Value for Mars |
| --- | --- |
| Data source | Your Pyroscope data source |
| Tags | `service.name`, renamed to `service_name` |
| Profile type | `process_cpu:cpu:nanoseconds:cpu:nanoseconds` |
| Use custom query | Off |

Save and test the configuration. The service names must match: Mars uses the
trace resource's `service.name` as Pyroscope's `service_name`; set
`OTEL_SERVICE_NAME` explicitly to make that choice predictable. Keep those fields
when forwarding through a collector.

For provisioned data sources, add this to the **Tempo** entry in your Grafana
provisioning YAML, replacing the UID with your actual Pyroscope data-source UID:

```yaml
jsonData:
  tracesToProfiles:
    datasourceUid: your-pyroscope-uid
    tags:
      - key: service.name
        value: service_name
    profileTypeId: process_cpu:cpu:nanoseconds:cpu:nanoseconds
```

A UID is not the data source's display name or plugin type. Apply the updated
provisioning file and restart Grafana as needed. The demo's
[complete provisioning file](bench/correlation/datasources.yaml) configures both
data sources. Grafana documents the [configuration and UI steps](https://grafana.com/docs/grafana/latest/datasources/tempo/configure-tempo-data-source/configure-trace-to-profiles/)
and [YAML provisioning](https://grafana.com/docs/grafana/latest/datasources/tempo/configure-tempo-data-source/provision/).

Eligible spans contain `pyroscope.profile.id`, equal to their span ID. CPU
samples carry matching `span_id` and `trace_id` **sample labels**. Grafana's
built-in span-profile integration uses that marker; do not add `span_id` or
`trace_id` to the service-tag mapping above. They are not profile-series labels.
No additional language-specific profiling bridge is needed for Mars.

## Try the local demo

The demo uses Grafana 13.1.1, Tempo 2.10.0 and Pyroscope 2.4.0, with local ephemeral
storage and anonymous Grafana viewing. All published ports bind to loopback.
It requires a Linux x86-64 or arm64 host, Docker Compose, Python 3, and the normal
Rust/GStreamer development dependencies, including GStreamer 1.22 or newer.

Run from the repository root:

```sh
docker compose -f tracer/otel/bench/correlation/compose.yaml up -d

RUSTFLAGS="-C force-frame-pointers=yes" \
  cargo build --release -p gst-otel-tracer --features profiles \
  --example profile_correlation

python3 tracer/otel/bench/correlation/check.py \
  --binary target/release/examples/profile_correlation
```

The checker waits for service readiness, runs a four-buffer pipeline with two
CPU-heavy `identity` elements on different threads, and forwards the actual
trace/profile exports to Tempo and Pyroscope. The artificial CPU work gives
short demo spans enough samples to inspect; this is not an overhead benchmark.
It prints a selected span ID and a Grafana Explore URL, and saves the results
and workload log under `target/correlation/`.

The automatic checks verify:

- Grafana loaded the expected data-source UIDs, service mapping and CPU type.
- All 16 emitted spans decode correctly, and samples match the exported span IDs.
- The selected trace and its `pyroscope.profile.id` marker can be queried through
  Grafana's Tempo proxy.
- Grafana's Pyroscope proxy returns exactly the decoded CPU totals for the
  service, selected span and selected trace. An unknown span returns zero.
- The selected span's profile includes the `[element] heavy` root.

For the **browser check**, open the printed Explore URL (or open
[Grafana](http://localhost:43002), choose **Mars traces**, and query the printed
trace ID). Expand the selected span and inspect its `pyroscope.profile.id`.
Use **Profiles for this span** and check the resulting flame graph. The selected
span in this demo should contain `[element] heavy`; compare its CPU total with
`queries.span.cpu_ns` in `target/correlation/result.json`.

The automatic checker exercises configuration and backend queries through
Grafana; it does **not** automate this browser action. See the
[recorded validation and limits](../../reports/otel/CORRELATION_REVIEW.md).

To remove the demo containers and their temporary data:

```sh
docker compose -f tracer/otel/bench/correlation/compose.yaml down
```

## If a link is missing or the profile is empty

- Check the span's `pyroscope.profile.id`. Profiling must have been active when
  the sampled push ran. Attribute limits can omit the marker; a string limit
  below 16 cannot retain its span ID.
- Check the Pyroscope UID, service-name mapping and CPU profile type. Additional
  tag mappings only work when those labels are actually exported to Pyroscope;
  Mars does not copy all OTel resource attributes into profile-series labels.
- Check that trace and profile retention cover the same period. Profiles upload
  about once per second. Stop streaming and call `gstoteltracer::shutdown(timeout)`
  when an exact final drain matters; `gst-launch-1.0` can exit before its last upload.
- A marker means the span was eligible, not that it received samples. At 100 Hz,
  the nominal interval is 10 ms of thread CPU. Short or mostly blocked spans can
  have no profile, and export losses can leave a missing counterpart.
- Correlation follows the innermost sampled push. A parent span's profile does
  not automatically include samples attributed to its nested child spans.
  Synthetic element/task roots describe where sampled CPU ran, independently of
  trace sampling.

## Metrics and logs

`mars-instrument` includes trace/span IDs in metric exemplars, and the optional
GStreamer log bridge puts them in logs emitted inside active sampled pushes.
Those identifiers do not configure Grafana navigation by themselves:

| Navigation | Additional setup |
| --- | --- |
| Metric exemplar → trace | Preserve exemplars in your metrics backend; configure the Prometheus data source's exemplar link to Tempo using the trace-ID label exposed by that backend. |
| Log → trace | Preserve log trace fields; configure Loki derived fields to target Tempo. |
| Trace → logs | Configure the Tempo data source's **Trace to logs** mapping for your log schema. |

See Grafana's [signal-linking overview](https://grafana.com/docs/grafana/latest/datasources/tempo/configure-tempo-data-source/#connect-traces-to-other-signals)
and Mars's [exemplar/log semantics](INSTRUMENT.md#measurements-and-correlation).
The local demo above covers trace/profile correlation only; it does not deploy
a metrics or logs backend or validate those UI links.
