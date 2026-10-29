//! Each child has independent GStreamer and exporter globals. Assertions inspect
//! decoded OTLP, not merely whether the pipeline reached EOS.
use gstreamer::{self as gst, prelude::*};
use opentelemetry_proto::tonic::{
    collector::{
        logs::v1::ExportLogsServiceRequest,
        trace::v1::{
            ExportTracePartialSuccess, ExportTraceServiceRequest, ExportTraceServiceResponse,
        },
    },
    trace::v1::Span,
};
use prost::Message;
use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

const LINEAR: &str =
    "fakesrc num-buffers=100 ! identity ! identity ! identity ! identity ! fakesink sync=false";

fn collect(pipeline: &str, sampler: &str, ratio: &str, mode: &str) -> Vec<Span> {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let address = server.server_addr().to_string();
    let spans = Arc::new(Mutex::new(Vec::new()));
    let logs = Arc::new(Mutex::new(Vec::new()));
    let log_sink = logs.clone();
    let resource_seen = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let sink = spans.clone();
    let resource = resource_seen.clone();
    let stopping = stop.clone();
    let slow = mode == "slow";
    let fail = mode.ends_with("fail");
    let response_mode = response_mode(mode).to_owned();
    let receiver = std::thread::spawn(move || {
        while !stopping.load(Ordering::Relaxed) {
            if let Some(mut request) = server.recv_timeout(Duration::from_millis(20)).unwrap() {
                let mut data = Vec::new();
                request.as_reader().read_to_end(&mut data).unwrap();
                if request
                    .headers()
                    .iter()
                    .any(|h| h.field.equiv("Content-Encoding") && h.value.as_str() == "gzip")
                {
                    use std::io::Read;
                    let mut decoded = Vec::new();
                    libflate::gzip::Decoder::new(data.as_slice())
                        .unwrap()
                        .read_to_end(&mut decoded)
                        .unwrap();
                    data = decoded;
                }
                if request.url() == "/v1/logs" {
                    let decoded = ExportLogsServiceRequest::decode(data.as_slice()).unwrap();
                    for resource in decoded.resource_logs {
                        for scope in resource.scope_logs {
                            assert_eq!(scope.scope.unwrap().name, "mars-otel");
                            log_sink.lock().unwrap().extend(scope.log_records);
                        }
                    }
                    request.respond(tiny_http::Response::empty(200)).unwrap();
                    continue;
                }
                assert_eq!(request.url(), "/custom-traces");
                assert!(request.headers().iter().any(|h| h.field.equiv("user-agent")
                    && h.value.as_str() == concat!("mars-otel/", env!("CARGO_PKG_VERSION"))));
                assert!(request
                    .headers()
                    .iter()
                    .any(|h| h.field.equiv("x-test") && h.value.as_str() == "encoded value"));
                assert!(request
                    .headers()
                    .iter()
                    .any(|h| h.field.equiv("content-type")
                        && h.value.as_str() == "application/x-protobuf"));
                let decoded = ExportTraceServiceRequest::decode(data.as_slice()).unwrap();
                let count: usize = decoded
                    .resource_spans
                    .iter()
                    .flat_map(|r| &r.scope_spans)
                    .map(|s| s.spans.len())
                    .sum();
                for rs in decoded.resource_spans {
                    if rs.resource.unwrap().attributes.iter().any(|a| {
                        a.key == "service.name" && format!("{:?}", a.value).contains("otel-test")
                    }) {
                        resource.store(true, Ordering::Relaxed);
                    }
                    for scope in rs.scope_spans {
                        assert_eq!(scope.scope.unwrap().name, "mars-otel");
                        sink.lock().unwrap().extend(scope.spans);
                    }
                }
                if slow {
                    std::thread::sleep(Duration::from_millis(100));
                }
                let response = if response_mode == "malformed" {
                    b"not protobuf".to_vec()
                } else {
                    ExportTraceServiceResponse {
                        partial_success: match response_mode.as_str() {
                            "partial" | "reject" | "warning" | "invalid-count"
                            | "negative-count" => Some(ExportTracePartialSuccess {
                                rejected_spans: match response_mode.as_str() {
                                    "partial" => 1,
                                    "reject" => count as i64,
                                    "invalid-count" => count as i64 + 1,
                                    "negative-count" => -1,
                                    _ => 0,
                                },
                                error_message: "test collector response".into(),
                            }),
                            _ => None,
                        },
                    }
                    .encode_to_vec()
                };
                request
                    // 503 is retryable by default in OTel 0.33. One received
                    // copy of each span verifies our explicit no-retry policy.
                    .respond(
                        tiny_http::Response::from_data(response).with_status_code(if fail {
                            503
                        } else {
                            200
                        }),
                    )
                    .unwrap();
            }
        }
    });
    let mut command = Command::new(std::env::current_exe().unwrap());
    for (key, _) in std::env::vars().filter(|(key, _)| {
        key.starts_with("GST_") || key.starts_with("OTEL_") || key.starts_with("MARS_")
    }) {
        command.env_remove(key);
    }
    let output = command
        .args(["--exact", "pipeline_child", "--ignored", "--nocapture"])
        .env_remove("GST_TRACERS")
        .env_remove("GST_PLUGIN_PATH")
        .env("GST_DEBUG", if mode == "logs" { "identity:7" } else { "0" })
        .env(
            "GST_OTEL_LOGS",
            if mode == "logs" { "true" } else { "false" },
        )
        .env("OTEL_SERVICE_NAME", "otel-test")
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", format!("http://{address}"))
        .env(
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
            format!("http://{address}/custom-traces"),
        )
        .env(
            "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
            "x-test=encoded%20value",
        )
        .env(
            "GST_OTEL_HTTP_CLIENT",
            if mode.starts_with("async") {
                "async"
            } else {
                "blocking"
            },
        )
        .env("OTEL_TRACES_SAMPLER", sampler)
        .env("OTEL_TRACES_SAMPLER_ARG", ratio)
        .env("GST_OTEL_QUEUE_CAPACITY", if slow { "16" } else { "65536" })
        .env("GST_OTEL_BATCH_SIZE", if slow { "8" } else { "128" })
        .env(
            "GST_OTEL_EXPORT_CONCURRENCY",
            if mode.contains("parallel") { "4" } else { "1" },
        )
        .env(
            "OTEL_EXPORTER_OTLP_COMPRESSION",
            if mode.contains("gzip") {
                "gzip"
            } else {
                "none"
            },
        )
        .env("OTEL_TEST_PIPELINE", pipeline)
        .env("OTEL_TEST_MODE", mode)
        .output()
        .unwrap();
    stop.store(true, Ordering::Relaxed);
    receiver.join().unwrap();
    assert!(
        output.status.success(),
        "child failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let spans = spans.lock().unwrap().clone();
    for span in &spans {
        let attribute = |key| {
            span.attributes
                .iter()
                .find(|a| a.key == key)
                .and_then(|a| a.value.as_ref())
                .and_then(|v| v.value.as_ref())
                .and_then(|v| match v {
                    opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(s) => {
                        Some(s.as_str())
                    }
                    _ => None,
                })
                .unwrap()
        };
        let operation = attribute("gst.operation");
        let factory = attribute(if operation == "gst.pad.push" {
            "sink_pad.factory"
        } else {
            "src_pad.factory"
        });
        assert_eq!(
            span.name,
            if factory.is_empty() {
                operation.to_owned()
            } else {
                format!("{operation} {factory}")
            }
        );
    }
    if !spans.is_empty() {
        assert!(resource_seen.load(Ordering::Relaxed));
    }
    if mode == "logs" {
        let logs = logs.lock().unwrap();
        assert!(
            logs.iter().any(|log| spans
                .iter()
                .any(|span| log.trace_id == span.trace_id && log.span_id == span.span_id)),
            "expected native log trace correlation"
        );
    }
    spans
}

fn complete_traces(spans: &[Span], size: usize) -> usize {
    let mut traces: HashMap<&[u8], Vec<&Span>> = HashMap::new();
    for span in spans {
        traces.entry(&span.trace_id).or_default().push(span);
    }
    for trace in traces.values() {
        assert_eq!(trace.len(), size, "incomplete or merged trace");
        let ids: HashSet<_> = trace.iter().map(|s| s.span_id.as_slice()).collect();
        assert_eq!(ids.len(), size);
        assert_eq!(
            trace.iter().filter(|s| s.parent_span_id.is_empty()).count(),
            1
        );
        for span in trace {
            assert!(span.start_time_unix_nano <= span.end_time_unix_nano);
            assert_eq!(span.flags & 1, 1);
            assert_eq!(span.flags & 0x300, 0x100, "expected a local parent");
            assert!(span.parent_span_id.is_empty() || ids.contains(span.parent_span_id.as_slice()));
            assert!(span.name.starts_with("gst.pad.push"), "{}", span.name);
        }
    }
    traces.len()
}

#[test]
fn exports_complete_traces_and_keeps_pad_references_stable() {
    let spans = collect(LINEAR, "parentbased_always_on", "1", "normal");
    assert_eq!(complete_traces(&spans, 5), 100);
}

#[test]
fn default_period_limits_real_source_pushes() {
    let pipeline = "fakesrc num-buffers=10000 ! identity ! fakesink sync=false";
    let spans = collect(pipeline, "parentbased_always_on", "1", "period-default");
    let count = complete_traces(&spans, 2);
    assert!((1..10000).contains(&count), "captured {count} roots");
    let mut starts: Vec<_> = spans
        .iter()
        .filter(|s| s.parent_span_id.is_empty())
        .map(|s| s.start_time_unix_nano)
        .collect();
    starts.sort_unstable();
    assert!(starts
        .windows(2)
        .all(|pair| pair[1] - pair[0] >= 16_000_000));
}

#[test]
fn journey_budgets_bound_real_children_across_a_queue() {
    let spans = collect(
        "fakesrc num-buffers=100 ! identity ! queue ! identity ! fakesink sync=false",
        "parentbased_always_on",
        "1",
        "budget-spans",
    );
    assert_eq!(complete_traces(&spans, 2), 100);
    let spans = collect(
        "fakesrc num-buffers=10 ! identity sleep-time=10000 ! queue ! identity ! fakesink sync=false",
        "parentbased_always_on", "1", "budget-age",
    );
    assert_eq!(complete_traces(&spans, 1), 10);
    assert!(spans
        .iter()
        .all(|s| s.end_time_unix_nano - s.start_time_unix_nano >= 1_000_000));
}

#[test]
fn tracer_parameters_can_disable_root_period() {
    let spans = collect(LINEAR, "parentbased_always_on", "1", "period-params");
    assert_eq!(complete_traces(&spans, 5), 100);
}
#[test]
fn nested_pull_pipeline_exports_reads_including_normal_eos() {
    let pipeline = "fakesrc num-buffers=3 sizetype=fixed sizemax=64 can-activate-push=false ! identity ! fakesink can-activate-pull=true can-activate-push=false sync=false";
    let spans = collect(pipeline, "parentbased_always_on", "1", "normal");
    assert_eq!(spans.len(), 8); // Two pulls per buffer, and two for EOS.
    let mut traces: HashMap<&[u8], Vec<&Span>> = HashMap::new();
    for span in &spans {
        assert!(
            span.name.starts_with("gst.pad.pull_range "),
            "{}",
            span.name
        );
        assert_eq!(span.status.as_ref().map_or(0, |s| s.code), 0);
        traces.entry(&span.trace_id).or_default().push(span);
    }
    assert_eq!(traces.len(), 4);
    for trace in traces.values() {
        assert_eq!(trace.len(), 2);
        let root = trace.iter().find(|s| s.parent_span_id.is_empty()).unwrap();
        let child = trace.iter().find(|s| !s.parent_span_id.is_empty()).unwrap();
        assert_eq!(child.parent_span_id, root.span_id);
    }
    assert!(collect(pipeline, "parentbased_always_off", "0", "normal").is_empty());
}

#[test]
fn async_transport_delivers_equivalent_traces_and_does_not_retry_failures() {
    for mode in ["async", "async-fail"] {
        assert_eq!(
            complete_traces(&collect(LINEAR, "parentbased_always_on", "1", mode), 5),
            100
        );
    }
}
#[test]
fn sampling_is_preserved_across_queue_boundaries() {
    let pipeline = "fakesrc num-buffers=500 ! identity ! queue max-size-buffers=3 ! identity ! fakesink sync=false";
    assert!(collect(pipeline, "parentbased_always_off", "0", "unsampled").is_empty());
    let partial = collect(pipeline, "parentbased_traceidratio", "0.5", "normal");
    let count = complete_traces(&partial, 4);
    assert!(
        (150..350).contains(&count),
        "implausible sampling count: {count}"
    );
    assert_eq!(
        complete_traces(
            &collect(pipeline, "parentbased_always_on", "1", "normal"),
            4
        ),
        500
    );
}
#[test]
fn transforms_and_queued_tee_branches_preserve_owned_context() {
    let video = "videotestsrc num-buffers=30 ! video/x-raw,format=I420,width=64,height=64 ! videoconvert ! video/x-raw,format=RGB ! fakesink sync=false";
    assert_eq!(
        complete_traces(&collect(video, "parentbased_always_on", "1", "normal"), 4),
        30
    );
    let tee = "fakesrc num-buffers=100 ! tee name=t t. ! queue ! identity ! fakesink sync=false t. ! queue ! identity ! fakesink sync=false";
    assert_eq!(
        complete_traces(&collect(tee, "parentbased_always_on", "1", "normal"), 7),
        100
    );
}
#[test]
fn slow_and_failed_receivers_leave_accounted_losses() {
    let pipeline = "fakesrc num-buffers=10000 ! identity ! fakesink sync=false";
    let spans = collect(pipeline, "parentbased_always_on", "1", "slow");
    assert!(!spans.is_empty() && spans.len() < 20000);
    assert_eq!(
        collect(LINEAR, "parentbased_always_on", "1", "fail").len(),
        500
    );
}
#[test]
fn optional_logs_use_native_trace_context() {
    let pipeline = "fakesrc num-buffers=5 ! identity silent=false ! fakesink sync=false";
    assert_eq!(
        complete_traces(&collect(pipeline, "parentbased_always_on", "1", "logs"), 2),
        5
    );
}

#[test]
fn bins_keep_a_connected_trace() {
    let pipeline = "fakesrc num-buffers=10 ! ( identity ! identity ) ! fakesink sync=false";
    assert_eq!(
        complete_traces(
            &collect(pipeline, "parentbased_always_on", "1", "normal"),
            5
        ),
        10
    );
}

#[test]
fn metadata_refreshes_after_rename() {
    let pipeline = "fakesrc name=source num-buffers=5 ! identity ! fakesink name=sink sync=false";
    let spans = collect(pipeline, "parentbased_always_on", "1", "rename");
    assert_eq!(complete_traces(&spans, 2), 10);
    assert!(
        spans
            .iter()
            .flat_map(|s| &s.attributes)
            .any(|a| a.key == "src_pad.element"
                && format!("{:?}", a.value).contains("renamed-source"))
    );
}

#[test]
fn trace_responses_account_for_rejections_and_warnings_without_retries() {
    for adapter in ["", "async-", "parallel-", "async-parallel-"] {
        for mode in [
            "partial",
            "reject",
            "warning",
            "malformed",
            "invalid-count",
            "negative-count",
        ] {
            let spans = collect(
                LINEAR,
                "parentbased_always_on",
                "1",
                &format!("{adapter}{mode}"),
            );
            assert_eq!(complete_traces(&spans, 5), 100);
        }
    }
}

fn response_mode(mode: &str) -> &str {
    let mode = mode.strip_prefix("async-").unwrap_or(mode);
    mode.strip_prefix("parallel-").unwrap_or(mode)
}

#[test]
fn gzip_with_parallel_requests_preserves_complete_traces() {
    for mode in ["parallel-gzip", "async-parallel-gzip"] {
        let spans = collect(LINEAR, "parentbased_always_on", "1", mode);
        assert_eq!(complete_traces(&spans, 5), 100);
    }
}

#[test]
#[ignore = "spawned by collector tests with a fresh process and explicit environment"]
fn pipeline_child() {
    gst::init().unwrap();
    let mode = std::env::var("OTEL_TEST_MODE").unwrap();
    let mut builder = glib::Object::builder::<gstoteltracer::TelemetryTracer>();
    if mode == "period-params" {
        builder = builder.property(
            "params",
            "trace-root-min-period-us=0,trace-max-age-ms=0,trace-max-spans=0",
        );
    } else if mode != "period-default" {
        builder = builder
            .property("trace-root-min-period-us", 0u32)
            .property(
                "trace-max-age-ms",
                if mode == "budget-age" { 1u32 } else { 0u32 },
            )
            .property(
                "trace-max-spans",
                if mode == "budget-spans" { 2u32 } else { 0u32 },
            );
    }
    let _tracer = builder.build();
    let pipeline = gst::parse::launch(&std::env::var("OTEL_TEST_PIPELINE").unwrap())
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
    let mut pad_refs = Vec::new();
    for element in pipeline.iterate_recurse().into_iter().map(Result::unwrap) {
        for pad in element.pads() {
            pad_refs.push((pad.ref_count(), pad));
        }
    }
    let run = || {
        pipeline.set_state(gst::State::Playing).unwrap();
        let message = pipeline
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(20),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .expect("pipeline timed out");
        if let gst::MessageView::Error(error) = message.view() {
            panic!("pipeline error: {} {:?}", error.error(), error.debug());
        }
        pipeline.set_state(gst::State::Null).unwrap();
    };
    run();
    if mode == "rename" {
        let source = pipeline.by_name("source").unwrap();
        let downstream = source
            .static_pad("src")
            .unwrap()
            .peer()
            .unwrap()
            .parent_element()
            .unwrap();
        source.unlink(&downstream);
        pipeline.remove(&source).unwrap();
        source.set_property("name", "renamed-source");
        assert_eq!(source.name(), "renamed-source");
        pipeline.add(&source).unwrap();
        source.link(&downstream).unwrap();
        run();
    }
    for (count, pad) in pad_refs {
        assert_eq!(pad.ref_count(), count, "pad reference leak: {}", pad.name());
    }
    let _ = gstoteltracer::flush(Duration::from_secs(5));
    let stats = gstoteltracer::stats().unwrap();
    assert_eq!(stats.pending_spans, 0, "{stats:?}");
    assert_eq!(stats.started_spans, stats.completed_spans);
    assert_eq!(
        stats.completed_spans,
        stats.exported_spans
            + stats.dropped_queue_full
            + stats.dropped_closed
            + stats.failed_export_spans
    );
    assert_eq!(stats.mismatched_hooks, 0);
    assert_eq!(stats.propagation_misses, 0, "{stats:?}");
    match response_mode(&mode) {
        "budget-spans" => assert!(stats.trace_span_rejections >= 100),
        "budget-age" => assert!(stats.trace_age_rejections >= 10),
        "period-default" => assert!(stats.throttled_roots > 0),
        "unsampled" => {
            assert_eq!(stats.started_spans, 0);
            assert_eq!(stats.unsampled_pushes, 2000);
        }
        "slow" => assert!(stats.dropped_queue_full > 0),
        "partial" => {
            assert!(stats.rejected_spans > 0);
            assert_eq!(stats.failed_export_spans, stats.rejected_spans);
            assert_eq!(stats.failed_exports, stats.rejected_spans);
            assert_eq!(stats.export_warnings, stats.failed_exports);
            assert_eq!(stats.exported_spans + stats.rejected_spans, 500);
        }
        "reject" => {
            assert_eq!(stats.rejected_spans, 500);
            assert_eq!(stats.failed_export_spans, 500);
            assert_eq!(stats.exported_spans, 0);
        }
        "warning" => {
            assert!(stats.export_warnings > 0);
            assert_eq!(stats.exported_spans, 500);
            assert_eq!(stats.failed_exports, 0);
            assert_eq!(stats.rejected_spans, 0);
        }
        "fail" | "malformed" | "invalid-count" | "negative-count" => {
            assert_eq!(stats.failed_export_spans, 500);
            assert_eq!(stats.exported_spans, 0);
        }
        _ => {
            assert_eq!(stats.dropped_queue_full, 0);
            assert_eq!(stats.failed_export_spans, 0);
        }
    }
    println!("{stats:?}");
    gstoteltracer::shutdown(Duration::from_secs(5)).unwrap();
}
