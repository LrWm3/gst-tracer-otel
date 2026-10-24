//! Exercise the real hooks and decode the actual OTLP requests in isolated processes.
use gstreamer::{self as gst, prelude::*};
use opentelemetry_proto::tonic::{
    collector::{
        metrics::v1::{
            ExportMetricsPartialSuccess, ExportMetricsServiceRequest, ExportMetricsServiceResponse,
        },
        trace::v1::ExportTraceServiceRequest,
    },
    metrics::v1::{metric::Data, HistogramDataPoint},
    trace::v1::Span,
};
use prost::Message;
use std::{
    collections::HashMap,
    io::Read,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const LINEAR: &str = "fakesrc name=source num-buffers=100 ! identity name=first ! queue ! identity name=second ! fakesink sync=false";
#[derive(Default)]
struct Received {
    spans: Vec<Span>,
    points: HashMap<(String, String), HistogramDataPoint>,
    examples: Vec<(Vec<u8>, Vec<u8>)>,
}
fn collect(mode: &str, pipeline: &str) -> Received {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let address = server.server_addr().to_string();
    let received = Arc::new(Mutex::new(Received::default()));
    let output = received.clone();
    let (stop, stopping) = std::sync::mpsc::channel();
    let mode_owned = mode.to_owned();
    let worker = std::thread::spawn(move || {
        let mut requests = Vec::new();
        while stopping.try_recv().is_err() {
            let Some(mut request) = server.recv_timeout(Duration::from_millis(10)).unwrap() else {
                continue;
            };
            let mut body = Vec::new();
            request.as_reader().read_to_end(&mut body).unwrap();
            let is_metrics = request.url() == "/custom-metrics";
            if is_metrics {
                assert!(request
                    .headers()
                    .iter()
                    .any(|h| h.field.equiv("x-metrics") && h.value.as_str() == "metric value"));
                let request = ExportMetricsServiceRequest::decode(body.as_slice()).unwrap();
                for resource in request.resource_metrics {
                    assert!(resource
                        .resource
                        .unwrap()
                        .attributes
                        .iter()
                        .any(|a| a.key == "service.name"
                            && format!("{:?}", a.value).contains("instrument-test")));
                    for scope in resource.scope_metrics {
                        assert_eq!(scope.scope.unwrap().name, "mars-instrument");
                        for metric in scope.metrics {
                            let Data::Histogram(histogram) = metric.data.unwrap() else {
                                panic!("histogram expected")
                            };
                            assert_eq!(histogram.aggregation_temporality, 2);
                            for point in histogram.data_points {
                                assert_eq!(point.bucket_counts.iter().sum::<u64>(), point.count);
                                assert!(point.time_unix_nano >= point.start_time_unix_nano);
                                assert!(!point
                                    .attributes
                                    .iter()
                                    .any(|a| matches!(a.key.as_str(), "trace_id" | "span_id")));
                                let src = point
                                    .attributes
                                    .iter()
                                    .find(|a| a.key == "src_pad.element")
                                    .unwrap();
                                let opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(src) = src.value.as_ref().unwrap().value.as_ref().unwrap() else { panic!() };
                                let mut output = output.lock().unwrap();
                                for example in &point.exemplars {
                                    assert_eq!(example.trace_id.len(), 16);
                                    assert_eq!(example.span_id.len(), 8);
                                    assert!(example.time_unix_nano <= point.time_unix_nano);
                                    output
                                        .examples
                                        .push((example.trace_id.clone(), example.span_id.clone()));
                                }
                                output
                                    .points
                                    .insert((metric.name.clone(), src.clone()), point);
                            }
                        }
                    }
                }
            } else {
                assert_eq!(request.url(), "/v1/traces");
                let request = ExportTraceServiceRequest::decode(body.as_slice()).unwrap();
                for resource in request.resource_spans {
                    for scope in resource.scope_spans {
                        output.lock().unwrap().spans.extend(scope.spans);
                    }
                }
            }
            let slow = (is_metrics && mode_owned == "slow-metrics")
                || (!is_metrics && mode_owned == "slow-traces");
            let fail = is_metrics && mode_owned == "reject-metrics";
            requests.push(std::thread::spawn(move || {
                if slow {
                    std::thread::sleep(Duration::from_millis(200));
                }
                let body = if fail {
                    ExportMetricsServiceResponse {
                        partial_success: Some(ExportMetricsPartialSuccess {
                            rejected_data_points: 1,
                            error_message: "test rejection".into(),
                        }),
                    }
                    .encode_to_vec()
                } else {
                    Vec::new()
                };
                let _ = request.respond(tiny_http::Response::from_data(body));
            }));
        }
        for request in requests {
            request.join().unwrap();
        }
    });
    let mut command = Command::new(std::env::current_exe().unwrap());
    for (key, _) in std::env::vars()
        .filter(|(k, _)| k.starts_with("GST_") || k.starts_with("OTEL_") || k.starts_with("MARS_"))
    {
        command.env_remove(key);
    }
    command
        .args(["--exact", "instrument_child", "--ignored", "--nocapture"])
        .env("INSTRUMENT_TEST_MODE", mode)
        .env("INSTRUMENT_TEST_PIPELINE", pipeline)
        .env("OTEL_SERVICE_NAME", "instrument-test")
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", format!("http://{address}"))
        .env(
            "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
            format!("http://{address}/custom-metrics"),
        )
        .env("OTEL_EXPORTER_OTLP_HEADERS", "x-metrics=wrong")
        .env(
            "OTEL_EXPORTER_OTLP_METRICS_HEADERS",
            "x-metrics=metric%20value",
        )
        .env("GST_INSTRUMENT_CPU_SAMPLE_RATE", "1")
        .env("GST_INSTRUMENT_METRIC_INTERVAL_MS", "60000")
        .env(
            "GST_OTEL_FLUSH_TIMEOUT_MS",
            if mode == "slow-metrics" { "30" } else { "5000" },
        )
        .env(
            "GST_OTEL_QUEUE_CAPACITY",
            if mode == "slow-traces" { "16" } else { "65536" },
        )
        .env(
            "GST_OTEL_BATCH_SIZE",
            if mode == "slow-traces" { "8" } else { "512" },
        )
        .env(
            "OTEL_TRACES_SAMPLER",
            match mode {
                "unsampled" => "parentbased_always_off",
                "partial" => "parentbased_traceidratio",
                _ => "parentbased_always_on",
            },
        )
        .env("OTEL_TRACES_SAMPLER_ARG", "0.5")
        .env(
            "GST_INSTRUMENT_TRACES",
            if matches!(mode, "metrics-only" | "all-off") {
                "false"
            } else {
                "true"
            },
        )
        .env(
            "GST_INSTRUMENT_METRICS",
            if matches!(mode, "traces-only" | "all-off") {
                "false"
            } else {
                "true"
            },
        );
    if mode == "async" {
        command.env("GST_OTEL_HTTP_CLIENT", "async");
    }
    if mode == "traces-only" {
        command.env(
            "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
            "deliberately invalid",
        );
    }
    if mode == "metrics-only" {
        // Disabled tracing must not even construct/validate a trace HTTP client.
        command.env("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "deliberately invalid");
    }
    if mode == "capacity" {
        command.env("GST_INSTRUMENT_METRIC_MAX_SERIES", "1");
    }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let stdout = std::thread::spawn(move || {
        let mut b = String::new();
        stdout.read_to_string(&mut b).unwrap();
        b
    });
    let stderr = std::thread::spawn(move || {
        let mut b = String::new();
        stderr.read_to_string(&mut b).unwrap();
        b
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            child.kill().unwrap();
            break child.wait().unwrap();
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let stdout = stdout.join().unwrap();
    let stderr = stderr.join().unwrap();
    stop.send(()).unwrap();
    worker.join().unwrap();
    assert!(status.success(), "child failed: {stdout}\n{stderr}");
    Arc::try_unwrap(received)
        .ok()
        .unwrap()
        .into_inner()
        .unwrap()
}
fn wall_count(received: &Received) -> u64 {
    received
        .points
        .iter()
        .filter(|((name, _), _)| name == "gst.pad.push.wall")
        .map(|(_, p)| p.count)
        .sum()
}
fn linked_examples(received: &Received) {
    assert!(!received.examples.is_empty());
    for (trace, span) in &received.examples {
        assert!(
            received
                .spans
                .iter()
                .any(|s| &s.trace_id == trace && &s.span_id == span),
            "exemplar must identify an actual exported span"
        );
    }
}
#[test]
fn metrics_and_exemplars_use_shared_context_across_queues() {
    let received = collect("normal", LINEAR);
    assert_eq!(wall_count(&received), 400);
    assert_eq!(received.spans.len(), 400);
    linked_examples(&received);
    #[cfg(unix)]
    assert_eq!(
        received
            .points
            .iter()
            .filter(|((n, _), _)| n == "gst.pad.push.exclusive.cpu")
            .map(|(_, p)| p.count)
            .sum::<u64>(),
        400
    );
}
#[test]
fn metrics_are_independent_of_trace_sampling_and_enablement() {
    for mode in [
        "async",
        "unsampled",
        "partial",
        "metrics-only",
        "traces-only",
        "all-off",
        "duplicate",
    ] {
        let received = collect(mode, LINEAR);
        assert_eq!(
            wall_count(&received),
            if matches!(mode, "traces-only" | "all-off") {
                0
            } else {
                400
            },
            "{mode}"
        );
        match mode {
            "unsampled" | "metrics-only" | "all-off" => {
                assert!(received.spans.is_empty());
                assert!(received.examples.is_empty());
            }
            "partial" => {
                assert!((100..300).contains(&received.spans.len()));
                linked_examples(&received);
            }
            "duplicate" | "async" => {
                assert_eq!(received.spans.len(), 400);
                linked_examples(&received);
            }
            "traces-only" => assert_eq!(received.spans.len(), 400),
            _ => unreachable!(),
        }
    }
}
#[test]
fn trace_queue_drops_do_not_drop_metric_observations_or_create_known_missing_exemplars() {
    let received = collect(
        "slow-traces",
        "fakesrc num-buffers=10000 ! identity ! fakesink sync=false",
    );
    assert_eq!(wall_count(&received), 20000);
    assert!(received.spans.len() < 20000);
    linked_examples(&received);
}
#[test]
fn exclusive_wall_time_subtracts_downstream_work() {
    let received = collect("normal", "fakesrc name=source num-buffers=10 ! identity name=outer sleep-time=1000 ! identity name=inner sleep-time=5000 ! fakesink sync=false");
    let inclusive = received.points[&("gst.pad.push.wall".into(), "source".into())]
        .sum
        .unwrap();
    let exclusive = received.points[&("gst.pad.push.exclusive.wall".into(), "source".into())]
        .sum
        .unwrap();
    assert!(inclusive > exclusive + 0.045, "{inclusive} {exclusive}");
    assert!(exclusive > 0.009, "{exclusive}");
    assert_eq!(wall_count(&received), 30);
}
#[test]
fn exclusive_wall_subtracts_both_sequential_tee_branches() {
    let received = collect("normal", "fakesrc name=source num-buffers=10 ! tee name=t t. ! identity sleep-time=5000 ! fakesink sync=false async=false t. ! identity sleep-time=7000 ! fakesink sync=false async=false");
    let inclusive = received.points[&("gst.pad.push.wall".into(), "source".into())]
        .sum
        .unwrap();
    let exclusive = received.points[&("gst.pad.push.exclusive.wall".into(), "source".into())]
        .sum
        .unwrap();
    assert!(
        inclusive > exclusive + 0.11,
        "both branches must be subtracted: {inclusive} {exclusive}"
    );
    assert_eq!(received.spans.len(), 50);
}

#[test]
fn slow_and_rejecting_metric_receivers_are_bounded_and_accounted() {
    for mode in ["slow-metrics", "reject-metrics"] {
        let received = collect(mode, LINEAR);
        assert_eq!(wall_count(&received), 400);
        assert_eq!(received.spans.len(), 400);
    }
}
#[test]
fn topology_refresh_and_series_cap_are_visible() {
    let received = collect("rename", LINEAR);
    assert_eq!(wall_count(&received), 800);
    assert!(received
        .points
        .contains_key(&("gst.pad.push.wall".into(), "renamed".into())));
    let received = collect("capacity", LINEAR);
    assert_eq!(wall_count(&received), 100);
}

#[test]
#[ignore = "run by the parent with isolated globals and explicit endpoints"]
fn instrument_child() {
    gst::init().unwrap();
    let mode = std::env::var("INSTRUMENT_TEST_MODE").unwrap();
    let _tracer: gstoteltracer::InstrumentTracer = glib::Object::new();
    let _duplicate =
        (mode == "duplicate").then(glib::Object::new::<gstoteltracer::TelemetryTracer>);
    let pipeline = gst::parse::launch(&std::env::var("INSTRUMENT_TEST_PIPELINE").unwrap())
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
    let run = || {
        pipeline.set_state(gst::State::Playing).unwrap();
        let message = pipeline
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(15),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .expect("EOS");
        if let gst::MessageView::Error(e) = message.view() {
            panic!("{} {:?}", e.error(), e.debug());
        }
        pipeline.set_state(gst::State::Null).unwrap();
    };
    run();
    if mode == "rename" {
        let source = pipeline.by_name("source").unwrap();
        let next = source
            .static_pad("src")
            .unwrap()
            .peer()
            .unwrap()
            .parent_element()
            .unwrap();
        source.unlink(&next);
        pipeline.remove(&source).unwrap();
        source.set_property("name", "renamed");
        pipeline.add(&source).unwrap();
        source.link(&next).unwrap();
        run();
    }
    let traces = gstoteltracer::stats().unwrap();
    let metrics = gstoteltracer::metric_stats().unwrap();
    assert_eq!(traces.mismatched_hooks, 0);
    assert_eq!(metrics.dropped_invalid, 0);
    if mode == "slow-traces" {
        assert!(traces.dropped_queue_full > 0);
    }
    if mode == "capacity" {
        assert!(metrics.dropped_capacity > 0);
        assert_eq!(metrics.active_series, 1);
    }
    if mode == "reject-metrics" {
        assert!(metrics.failed_exports > 0);
    }
    let start = Instant::now();
    let result = gstoteltracer::shutdown(Duration::from_millis(if mode == "slow-metrics" {
        10
    } else {
        5000
    }));
    match mode.as_str() {
        "slow-metrics" => {
            assert_eq!(result, Err(gstoteltracer::FlushError::Timeout));
            assert!(start.elapsed() < Duration::from_secs(1));
            // Verify cleanup can finish after the caller's deadline expires.
            gstoteltracer::shutdown(Duration::from_secs(5)).unwrap();
        }
        "reject-metrics" => assert_eq!(result, Err(gstoteltracer::FlushError::ExportFailed)),
        _ => {
            result.unwrap();
        }
    }
    println!("{traces:?} {metrics:?}");
}
