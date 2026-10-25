# Follow metrics, traces, logs and CPU profiles in Grafana

Mars emits the correlation IDs automatically. Grafana needs a one-time data-source
configuration to turn them into links. Use
`mars-instrument(signals="metrics,traces,profiles,logs")`, send metrics to Prometheus,
traces to Tempo, logs to Loki, and profiles to Pyroscope. `mars-otel` remains supported for
existing trace/profile configurations. `mars-profiles` alone emits no
traces, although its element names still appear in flame graphs.

[Watch the recorded walkthrough](../../docs/media/correlation.mp4): a 500 ms
measurement opens its factory-named span, then its warning log, back to the same
trace, and into the selected element's 500 ms CPU profile.
The [README animation](../../README.md) previews the same navigation.

## Configure your existing Grafana

Add Prometheus, Tempo, Loki and Pyroscope as data sources. On the Prometheus source,
configure an **Exemplars** internal link: label `trace_id`, destination Tempo.
The metrics backend must retain exemplars. For provisioned data sources:

```yaml
jsonData:
  exemplarTraceIdDestinations:
    - name: trace_id
      datasourceUid: your-tempo-uid
      urlDisplayLabel: Inspect trace
```

Users also need [access to Explore](https://grafana.com/docs/grafana/latest/visualizations/explore/get-started-with-explore/#access-explore):
an Editor or Administrator role, or the data sources explorer role where RBAC
is available. A Viewer can query data sources through the API but cannot open
Explore with the default permissions.

The trace-ID label can differ if a collector renames it; use the label actually
stored by your backend. See Grafana's
[Prometheus exemplar settings](https://grafana.com/docs/grafana/latest/datasources/prometheus/configure/).

In **Connections → Data sources →
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
[complete provisioning file](bench/correlation/datasources.yaml) configures all four
data sources. Grafana documents the [configuration and UI steps](https://grafana.com/docs/grafana/latest/datasources/tempo/configure-tempo-data-source/configure-trace-to-profiles/)
and [YAML provisioning](https://grafana.com/docs/grafana/latest/datasources/tempo/configure-tempo-data-source/provision/).

Eligible spans contain `pyroscope.profile.id`, equal to their span ID. CPU
samples carry matching `span_id` and `trace_id` **sample labels**. Grafana's
built-in span-profile integration uses that marker; do not add `span_id` or
`trace_id` to the service-tag mapping above. They are not profile-series labels.
No additional language-specific profiling bridge is needed for Mars.

### Connect traces and logs in both directions

Enable the `logs` signal and the GStreamer debug categories/levels you want to
collect. The demo uses `GST_DEBUG=0,mars-correlation:4`. The log bridge records
native OTLP trace/span IDs for messages emitted inside a sampled instrumented
call. Messages outside a sampled call remain useful logs but have no trace link.

For Loki's native OTLP ingestion, send logs to `/otlp/v1/logs`, for example:

```sh
OTEL_EXPORTER_OTLP_LOGS_ENDPOINT=http://localhost:43100/otlp/v1/logs
```

Loki must allow structured metadata. It maps `service.name` to `service_name`
and keeps the native IDs as per-record `trace_id` and `span_id`. Preserve these
fields through collectors; do not promote IDs to stream labels.
See [Loki's OTLP mapping](https://grafana.com/docs/loki/latest/send-data/otel/).

Add this alongside `tracesToProfiles` in the **Tempo** data source's `jsonData`:

```yaml
tracesToLogsV2:
  datasourceUid: your-loki-uid
  spanStartTimeShift: '-1s'
  spanEndTimeShift: '1s'
  tags:
    - key: service.name
      value: service_name
  customQuery: true
  query: '{$${__tags}} | trace_id = "$${__trace.traceId}" | span_id = "$${__span.spanId}"'
```

The query selects one span's logs within its time window, expanded by one second
on each side. These are metadata filters; no `json` parser or text search is
needed for native OTLP logs. For all logs in the trace, omit the `span_id` filter.
The `$${...}` escaping is for provisioning YAML; use `${...}` in Grafana's UI.
See [trace-to-logs configuration](https://grafana.com/docs/grafana/latest/datasources/tempo/configure-tempo-data-source/configure-trace-to-logs/).

For the return link, add this to the **Loki** data source's `jsonData`:

```yaml
derivedFields:
  - name: TraceID
    matcherType: label
    matcherRegex: '^trace_id$'
    datasourceUid: your-tempo-uid
    url: '$${__value.raw}'
    urlDisplayLabel: Inspect trace
```

In the UI this is a **Label** derived field matching `trace_id`, with an internal
Tempo link and query `${__value.raw}`. It reads structured metadata, independently
of the message text. Both directions require configuration; Grafana itself needs
no code changes. See [Loki derived fields](https://grafana.com/docs/grafana/latest/datasources/loki/configure/#derived-fields).

## Try the local demo

The demo uses Grafana 13.1.1, Prometheus 3.3.0, Tempo 2.10.0, Loki 3.5.0 and Pyroscope 2.4.0, with local ephemeral
storage and anonymous Editor access so Explore works without a login. All
published ports bind to loopback; this access setting is for the disposable local demo.
It requires a Linux x86-64 or arm64 host, Docker Compose, Python 3, and the normal
Rust/GStreamer development dependencies, including GStreamer 1.22 or newer.

Run from the repository root:

```sh
docker compose -f tracer/otel/bench/correlation/compose.yaml up -d

RUSTFLAGS="-C force-frame-pointers=yes" \
  cargo build --release -p gst-otel-tracer --features profiles \
  --example profile_correlation

python3 tracer/otel/bench/correlation/check.py \
  --binary target/release/examples/profile_correlation --showcase
```

The checker waits for service readiness, runs 48 buffers through two `identity`
elements on different threads, and forwards actual exports to Prometheus,
Tempo, Loki and Pyroscope. One deliberately slower frame creates a visible spike
and a warning log. The source briefly waits before sending the next frame so
the slow observation stays in the bounded recent-exemplar slot for an export
interval; not every observation is guaranteed to survive as an exemplar.
The artificial CPU work gives spans enough samples to inspect; this is not an
overhead benchmark. Omit `--showcase` for the shorter four-buffer check.
It prints a selected exemplar/span and Grafana Explore URLs, and saves the results
and workload log under `target/correlation/`.

The automatic checks verify:

- Grafana loaded the expected data-source UIDs, service mapping and CPU type.
- All 192 emitted spans (16 in the short check) decode correctly, and samples
  match the exported span IDs.
- Stored Prometheus exemplars match exported IDs and histogram observations;
  the metric graph returns nonempty data through Grafana.
- The selected trace and its `pyroscope.profile.id` marker can be queried through
  Grafana's Tempo proxy.
- Grafana's Pyroscope proxy returns exactly the decoded CPU totals for the
  service, selected span and selected trace. An unknown span returns zero.
- The selected span's profile includes the `[element] heavy` root.
- All 98 logs (10 in the short check) retain their original timestamps and native
  IDs in Loki. Two logs outside spans have no correlation IDs.
- The selected log timestamp falls inside the selected span. Its exact service,
  trace ID and span ID return one log; unknown IDs and an unrelated span return
  none. Trace/span IDs are absent from Loki's indexed stream labels.

For the **browser check**, open the printed **Open metrics** URL (or open
[Grafana](http://localhost:43002) and choose **Mars metrics**). Exemplars are
enabled in the generated query. Select the exemplar matching
`selected_exemplar` in the result JSON and follow **Inspect trace**. Expand its
`gst.pad.push identity` span and use **Logs for this span**. The single warning
identifies `element=heavy`, frame 25, and 500 ms of deliberate CPU work. Expand
the log details and follow **Inspect trace** back to the same trace. Select the
slow span again and use **Profiles for this span**.
The printed **Open trace** URL is also a shortcut to the same trace. The selected
span in this demo should contain `[element] heavy`; compare its CPU total with
`queries.span.cpu_ns` in `target/correlation/result.json`.
If Grafana folds the stack into a single bar, click **Expand all groups** in the
flame graph toolbar to reveal the element name and native frames.

The backend checker exercises configuration and queries through Grafana. The
separate [browser recorder](bench/correlation/record.py) follows the actual UI
links with `agent-browser` and headless Chrome, checks the generated IDs/time
ranges, and verifies empty results for unknown/unrelated IDs. After installing
`agent-browser` and its Chrome browser, record or validate the walkthrough with:

```sh
python3 tracer/otel/bench/correlation/record.py \
  --result target/correlation/result.json
# Add --dry-run to check the UI without recording video.
```

It writes screenshots, verification JSON and a raw WebM under
`target/correlation/video-logs/`, using its own browser profile. See the
[current browser validation and limits](../../reports/otel/SIGNALS_REVIEW.md#four-signal-walkthrough--2026-10-25).
With `ffmpeg`, `ffprobe` and DejaVu Sans fonts installed, regenerate the captioned
README MP4/GIF from a successful recording:

```sh
python3 tracer/otel/bench/correlation/render.py
```

The demo creates fresh IDs each run; the video's Explore URLs expire with
backend retention.

The [daily validation workflow](../../.github/workflows/validation.yaml) runs the
backend checker and browser recorder in `--dry-run` mode against a fresh stack.
It retains JSON results, logs and screenshots as artifacts without changing the
README recording. Workflow runs also exercise native lifecycle soaks on both
supported CPU architectures; see [local reproduction and run controls](../../CONTRIBUTING.md#build-and-test).

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

## Missing logs or links

- Enable both the `logs` signal and relevant `GST_DEBUG` categories. Disabled
  GStreamer messages never reach the bridge.
- Confirm `service_name`, `trace_id` and `span_id` in Loki's log details. A
  collector that drops or renames those fields needs corresponding query changes.
- Check both data-source UIDs and the time window. The trace and log must both
  still exist in their backends.
- Logs outside sampled instrumented calls have no native span context. A trace
  link cannot be reconstructed from an element name alone.
- Check instrumentation health for failed/dropped logs. A useful log line does
  not imply successful export of its corresponding trace.

See Mars's [exemplar/log semantics](INSTRUMENT.md#follow-a-measurement) for the
collection boundaries.
