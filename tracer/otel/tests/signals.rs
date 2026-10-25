//! Actual hooks, both export protocols, disabled-signal isolation and deadlines.
use gstreamer::{self as gst, prelude::*};
use opentelemetry_proto::tonic::{
    collector::{
        metrics::v1::{
            ExportMetricsPartialSuccess, ExportMetricsServiceRequest, ExportMetricsServiceResponse,
        },
        trace::v1::ExportTraceServiceRequest,
    },
    metrics::v1::metric::Data,
    trace::v1::Span,
};
use prost::Message;
use std::{
    io::Read,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Default)]
struct Received {
    metrics: Vec<opentelemetry_proto::tonic::metrics::v1::Metric>,
    spans: Vec<Span>,
}

fn run(mode: &str) -> (Received, String) {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let address = server.server_addr().to_string();
    let received = Arc::new(Mutex::new(Received::default()));
    let output = received.clone();
    let (stop, stopping) = std::sync::mpsc::channel();
    let worker_mode = mode.to_owned();
    let worker = std::thread::spawn(move || {
        while stopping.try_recv().is_err() {
            let Some(mut request) = server.recv_timeout(Duration::from_millis(10)).unwrap() else {
                continue;
            };
            let metrics = request.url() == "/v1/metrics";
            let mut body = Vec::new();
            request.as_reader().read_to_end(&mut body).unwrap();
            if metrics {
                let request = ExportMetricsServiceRequest::decode(body.as_slice()).unwrap();
                for resource in request.resource_metrics {
                    for scope in resource.scope_metrics {
                        output.lock().unwrap().metrics.extend(scope.metrics);
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
            if metrics && worker_mode == "slow" {
                std::thread::sleep(Duration::from_millis(300));
            }
            let body = ExportMetricsServiceResponse {
                partial_success: (metrics && worker_mode == "reject").then(|| {
                    ExportMetricsPartialSuccess {
                        rejected_data_points: 1,
                        error_message: "test".into(),
                    }
                }),
            }
            .encode_to_vec();
            let _ = request.respond(tiny_http::Response::from_data(body));
        }
    });
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    for (key, _) in std::env::vars()
        .filter(|(k, _)| k.starts_with("GST_") || k.starts_with("OTEL_") || k.starts_with("MARS_"))
    {
        cmd.env_remove(key);
    }
    cmd.args(["--exact", "signals_child", "--ignored", "--nocapture"])
        .env("SIGNALS_CASE", mode)
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", format!("http://{address}"))
        .env("OTEL_EXPORTER_OTLP_TIMEOUT", "2000")
        .env(
            "GST_OTEL_FLUSH_TIMEOUT_MS",
            if mode == "slow" { "10" } else { "2000" },
        )
        .env("MARS_METRIC_INTERVAL_MS", "60000")
        .env("MARS_METRIC_SAMPLE_RATE", "1")
        .env("MARS_METRIC_IO_COUNTS", "true")
        .env("MARS_METRIC_QUEUE_INTERVAL_MS", "10")
        // A URL alone must not enable profiling or log export in explicit mode.
        .env("MARS_PROFILE_URL", "deliberately invalid")
        .env("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT", "deliberately invalid");
    if mode == "metrics" || mode == "prometheus" {
        cmd.env("OTEL_TRACES_SAMPLER", "invalid-disabled-setting")
            .env("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "invalid");
    }
    if mode == "traces" {
        cmd.env("MARS_METRICS_EXPORTER", "invalid-disabled-setting")
            .env("MARS_METRIC_SAMPLE_RATE", "invalid")
            .env("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT", "invalid");
    }
    if mode == "unsampled" || mode == "lists-unsampled" {
        cmd.env("OTEL_TRACES_SAMPLER", "parentbased_always_off");
    }
    if mode == "lists-legacy" {
        cmd.env_remove("MARS_PROFILE_URL")
            .env("GST_INSTRUMENT_CPU_SAMPLE_RATE", "1");
    }
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut s = String::new();
        stdout.read_to_string(&mut s).unwrap();
        s
    });
    let err = std::thread::spawn(move || {
        let mut s = String::new();
        stderr.read_to_string(&mut s).unwrap();
        s
    });
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if start.elapsed() > Duration::from_secs(20) {
            child.kill().unwrap();
            break child.wait().unwrap();
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let stdout = out.join().unwrap();
    let stderr = err.join().unwrap();
    stop.send(()).unwrap();
    worker.join().unwrap();
    assert!(status.success(), "{mode}: {stdout}\n{stderr}");
    (
        Arc::try_unwrap(received)
            .ok()
            .unwrap()
            .into_inner()
            .unwrap(),
        stdout,
    )
}

fn count(received: &Received, name: &str) -> u64 {
    received
        .metrics
        .iter()
        .rev()
        .find(|m| m.name == name)
        .map_or(0, |m| match m.data.as_ref().unwrap() {
            Data::Histogram(h) => h.data_points.iter().map(|p| p.count).sum(),
            Data::Sum(s) => s
                .data_points
                .iter()
                .map(|p| match p.value.unwrap() {
                    opentelemetry_proto::tonic::metrics::v1::number_data_point::Value::AsDouble(
                        v,
                    ) => v as u64,
                    _ => panic!(),
                })
                .sum(),
            _ => panic!(),
        })
}

#[test]
fn full_metrics_and_exemplars_survive_otlp_translation() {
    let (received, _) = run("full");
    assert_eq!(received.spans.len(), 302);
    assert_eq!(count(&received, "gst_element_latency_count_count"), 301);
    assert_eq!(count(&received, "gst_element_sync_wall_seconds"), 301);
    assert_eq!(count(&received, "gst_element_pull_calls_total"), 2);
    assert!(received
        .metrics
        .iter()
        .any(|m| m.name == "gst_queue_current_level_buffers"));
    assert!(received
        .metrics
        .iter()
        .any(|m| m.name == "gst_element_buffers_in_total"));
    let pulls: Vec<_> = received
        .spans
        .iter()
        .filter(|s| s.name.starts_with("gst.pad.pull_range"))
        .collect();
    assert_eq!(pulls.len(), 2);
    for span in &pulls {
        assert_eq!(span.status.as_ref().map_or(0, |s| s.code), 0);
        for (key, expected) in [
            ("src_pad.element", "pull_source"),
            ("sink_pad.element", "pull_sink"),
        ] {
            assert!(span.attributes.iter().any(|a| a.key == key && matches!(a.value.as_ref().and_then(|v| v.value.as_ref()), Some(opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(v)) if v == expected)));
        }
    }
    let mut pull_exemplars = 0;
    let mut exemplars = 0;
    for metric in &received.metrics {
        if let Some(Data::Histogram(hist)) = &metric.data {
            for p in &hist.data_points {
                assert_eq!(p.bucket_counts.iter().sum::<u64>(), p.count);
                for e in &p.exemplars {
                    exemplars += 1;
                    if metric.name.contains("pull") {
                        assert!(pulls
                            .iter()
                            .any(|s| s.trace_id == e.trace_id && s.span_id == e.span_id));
                        pull_exemplars += 1;
                    }
                    assert!(received
                        .spans
                        .iter()
                        .any(|s| s.trace_id == e.trace_id && s.span_id == e.span_id));
                }
            }
        }
    }
    assert!(exemplars >= 3);
    assert!(pull_exemplars > 0);
}

#[test]
fn buffer_lists_cross_a_queue_with_connected_spans_and_matching_exemplars() {
    for mode in ["lists", "lists-traces", "lists-legacy", "lists-unsampled"] {
        let (received, _) = run(mode);
        if mode == "lists-unsampled" {
            assert!(received.spans.is_empty());
            continue;
        }
        assert_eq!(
            received.spans.len(),
            30,
            "{mode}: {:?}",
            received.spans.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        let lists: Vec<_> = received
            .spans
            .iter()
            .filter(|s| s.name.starts_with("gst.pad.push_list"))
            .collect();
        assert_eq!(lists.len(), 12);
        for span in &lists {
            assert!(matches!(
                span.name.as_str(),
                "gst.pad.push_list queue" | "gst.pad.push_list identity"
            ));
            assert!(span.attributes.iter().any(|a| a.key == "buffer.list.length"
                && matches!(
                    a.value.as_ref().and_then(|v| v.value.as_ref()),
                    Some(opentelemetry_proto::tonic::common::v1::any_value::Value::IntValue(3))
                )));
            assert!(span.attributes.iter().any(|a| a.key == "buffer.size"
                && matches!(
                    a.value.as_ref().and_then(|v| v.value.as_ref()),
                    Some(opentelemetry_proto::tonic::common::v1::any_value::Value::IntValue(12))
                )));
        }
        let mut traces = std::collections::HashMap::new();
        for span in &received.spans {
            traces
                .entry(&span.trace_id)
                .or_insert_with(Vec::new)
                .push(span);
        }
        assert_eq!(traces.len(), 6);
        for spans in traces.values() {
            assert_eq!(spans.len(), 5);
            assert_eq!(
                spans.iter().filter(|s| s.parent_span_id.is_empty()).count(),
                1
            );
            for span in spans.iter().filter(|s| !s.parent_span_id.is_empty()) {
                assert!(spans
                    .iter()
                    .any(|parent| parent.span_id == span.parent_span_id));
            }
        }
        if mode != "lists-traces" {
            assert!(
                received.metrics.iter().any(|m| match &m.data {
                    Some(Data::Histogram(h)) => h.data_points.iter().any(|p| p
                        .exemplars
                        .iter()
                        .any(|e| lists
                            .iter()
                            .any(|s| s.trace_id == e.trace_id && s.span_id == e.span_id))),
                    _ => false,
                }),
                "list operations must carry metric exemplars: {mode}"
            );
        }
    }
}

#[test]
fn disabled_signals_ignore_exporters_and_other_signal_settings() {
    let (metrics, _) = run("metrics");
    assert_eq!(count(&metrics, "gst_element_sync_wall_seconds"), 301);
    assert!(metrics.spans.is_empty());
    let (unsampled, _) = run("unsampled");
    assert_eq!(count(&unsampled, "gst_element_sync_wall_seconds"), 301);
    assert!(unsampled.spans.is_empty());
    assert!(unsampled.metrics.iter().all(|m| match &m.data {
        Some(Data::Histogram(h)) => h.data_points.iter().all(|p| p.exemplars.is_empty()),
        _ => true,
    }));
    let (traces, _) = run("traces");
    assert_eq!(traces.spans.len(), 302);
    assert!(traces.metrics.is_empty());
}

#[test]
fn prometheus_export_and_otlp_use_the_full_collector() {
    let (received, text) = run("prometheus");
    assert!(received.metrics.is_empty() && received.spans.is_empty());
    let total = |name: &str| {
        text.lines()
            .filter(|l| l.starts_with(&format!("{name}{{")))
            .map(|l| l.split('}').nth(1).unwrap().trim().parse::<f64>().unwrap())
            .sum::<f64>()
    };
    assert_eq!(total("gst_element_sync_wall_seconds_count"), 301.0);
    assert_eq!(total("gst_element_latency_count_count_total"), 301.0);
    assert_eq!(total("gst_element_pull_calls_total"), 2.0);
    assert!(text.contains("gst_queue_current_level_buffers{"));
    let (received, text) = run("prometheus-linked");
    assert_eq!(received.spans.len(), 302);
    assert!(text.contains(" # {trace_id=\""));
    for span in &received.spans {
        let id =
            opentelemetry::trace::SpanId::from_bytes(span.span_id.as_slice().try_into().unwrap())
                .to_string();
        if text.contains(&format!("span_id=\"{id}\"")) {
            return;
        }
    }
    panic!("scraped exemplars must reference exported spans");
}

#[test]
fn collector_failure_and_shutdown_deadline_are_observable() {
    run("slow");
    run("reject");
    run("port-conflict");
}

#[test]
#[ignore]
fn signals_child() {
    gst::init().unwrap();
    let mode = std::env::var("SIGNALS_CASE").unwrap();
    let prometheus = mode.starts_with("prometheus") || mode == "port-conflict";
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    if mode != "port-conflict" {
        drop(reservation);
    }
    let signals = match mode.as_str() {
        "metrics" | "prometheus" | "port-conflict" => "metrics",
        "traces" | "lists-traces" => "traces",
        _ => "metrics,traces",
    };
    let params = format!(
        "signals=\"{signals}\",metrics-exporter={},metrics-port={port}",
        if prometheus { "prometheus" } else { "otlp" }
    );
    let _tracer: gstoteltracer::InstrumentTracer = if mode == "lists-legacy" {
        glib::Object::new()
    } else {
        glib::Object::builder().property("params", params).build()
    };
    if mode.starts_with("lists") {
        let pipeline = gst::parse::launch(
            "appsrc name=source format=time ! queue ! identity ! fakesink sync=false",
        )
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let source = pipeline.by_name("source").unwrap();
        for _ in 0..6 {
            let mut list = gst::BufferList::new();
            for _ in 0..3 {
                list.get_mut()
                    .unwrap()
                    .add(gst::Buffer::with_size(4).unwrap());
            }
            assert_eq!(
                source.emit_by_name::<gst::FlowReturn>("push-buffer-list", &[&list]),
                gst::FlowReturn::Ok
            );
        }
        assert_eq!(
            source.emit_by_name::<gst::FlowReturn>("end-of-stream", &[]),
            gst::FlowReturn::Ok
        );
        let message = pipeline
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(5),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .unwrap();
        assert_eq!(message.type_(), gst::MessageType::Eos);
        pipeline.set_state(gst::State::Null).unwrap();
        let stats = gstoteltracer::shutdown(Duration::from_secs(5)).unwrap();
        assert_eq!(
            stats.mismatched_hooks
                + stats.propagation_misses
                + stats.failed_export_spans
                + stats.dropped_queue_full,
            0,
            "{stats:?}"
        );
        assert_eq!(stats.pending_spans, 0);
        if mode == "lists-unsampled" {
            assert_eq!(stats.unsampled_lists, 12);
        }
        return;
    }
    if mode == "port-conflict" {
        assert_eq!(
            gstoteltracer::stats(),
            Err(gstoteltracer::FlushError::NotInitialized)
        );
        return;
    }
    let pipeline = gst::parse::launch("fakesrc name=source num-buffers=100 ! identity name=work sleep-time=1000 ! queue name=buffered ! fakesink sync=false").unwrap().downcast::<gst::Pipeline>().unwrap();
    pipeline.set_state(gst::State::Playing).unwrap();
    let message = pipeline
        .bus()
        .unwrap()
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(5),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .unwrap();
    assert!(matches!(message.view(), gst::MessageView::Eos(_)));
    pipeline.set_state(gst::State::Null).unwrap();
    let _manual = manual_list_and_pull();
    if prometheus {
        let response = reqwest::blocking::get(format!("http://127.0.0.1:{port}/metrics")).unwrap();
        assert_eq!(
            response.headers()["content-type"],
            "application/openmetrics-text; version=1.0.0; charset=utf-8"
        );
        println!("{}", response.text().unwrap());
    }
    let start = Instant::now();
    let result = gstoteltracer::shutdown(Duration::from_millis(if mode == "slow" {
        10
    } else {
        3000
    }));
    match mode.as_str() {
        "slow" => {
            assert_eq!(result, Err(gstoteltracer::FlushError::Timeout));
            assert!(start.elapsed() < Duration::from_millis(500));
            gstoteltracer::shutdown(Duration::from_secs(5)).unwrap();
        }
        "reject" => {
            assert_eq!(result, Err(gstoteltracer::FlushError::ExportFailed));
            assert!(gstoteltracer::metric_stats().unwrap().failed_exports > 0);
        }
        _ => {
            result.unwrap();
        }
    }
    assert_eq!(gstoteltracer::stats().unwrap().mismatched_hooks, 0);
    assert_eq!(
        gstoteltracer::metric_stats().unwrap().recorded_pushes,
        if mode == "traces" { 0 } else { 301 }
    );
}

fn manual_list_and_pull() -> gst::Pipeline {
    let graph = gst::Pipeline::with_name("manual");
    let source = gst::Bin::with_name("list_source");
    let sink = gst::Bin::with_name("list_sink");
    graph.add_many([&source, &sink]).unwrap();
    let src = gst::Pad::builder(gst::PadDirection::Src)
        .name("src")
        .build();
    let dst = gst::Pad::builder(gst::PadDirection::Sink)
        .name("sink")
        .chain_list_function(|_, _, _| Ok(gst::FlowSuccess::Ok))
        .build();
    source.add_pad(&src).unwrap();
    sink.add_pad(&dst).unwrap();
    src.link(&dst).unwrap();
    src.set_active(true).unwrap();
    dst.set_active(true).unwrap();
    src.push_event(gst::event::StreamStart::new("list"));
    src.push_event(gst::event::Segment::new(&gst::FormattedSegment::<
        gst::ClockTime,
    >::new()));
    let mut list = gst::BufferList::new();
    for _ in 0..4 {
        list.get_mut().unwrap().add(gst::Buffer::new());
    }
    src.push_list(list).unwrap();
    src.set_active(false).unwrap();
    dst.set_active(false).unwrap();
    let source = gst::Bin::with_name("pull_source");
    let sink = gst::Bin::with_name("pull_sink");
    graph.add_many([&source, &sink]).unwrap();
    let src = gst::Pad::builder(gst::PadDirection::Src)
        .name("src")
        .getrange_function(|_, _, offset, _, size| {
            if offset == 1 {
                return Err(gst::FlowError::Eos);
            }
            Ok(gst::PadGetRangeSuccess::NewBuffer(
                gst::Buffer::with_size(size as usize).unwrap(),
            ))
        })
        .build();
    let dst = gst::Pad::builder(gst::PadDirection::Sink)
        .name("sink")
        .build();
    source.add_pad(&src).unwrap();
    sink.add_pad(&dst).unwrap();
    src.link(&dst).unwrap();
    src.activate_mode(gst::PadMode::Pull, true).unwrap();
    dst.activate_mode(gst::PadMode::Pull, true).unwrap();
    assert_eq!(dst.pull_range(0, 16).unwrap().size(), 16);
    assert_eq!(dst.pull_range(1, 16), Err(gst::FlowError::Eos));
    dst.activate_mode(gst::PadMode::Pull, false).unwrap();
    src.activate_mode(gst::PadMode::Pull, false).unwrap();
    graph
}
