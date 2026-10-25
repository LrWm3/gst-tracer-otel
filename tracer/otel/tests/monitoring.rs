//! Assert self-monitoring selection and routing using decoded backend data.
use gstreamer::{self as gst, prelude::*};
use opentelemetry_proto::tonic::{
    collector::metrics::v1::ExportMetricsServiceRequest,
    metrics::v1::{metric::Data, Metric},
};
use prost::Message;
use std::{
    process::Command,
    sync::{mpsc, Arc, Mutex},
    time::Duration,
};

#[test]
fn monitoring_levels_route_to_the_selected_metrics_exporter() {
    for exporter in ["otlp", "prometheus"] {
        for level in ["none", "core", "all", "default"] {
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let address = server.server_addr().to_string();
            let metrics = Arc::new(Mutex::new(Vec::<Metric>::new()));
            let received = metrics.clone();
            let (stop, stopping) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                while stopping.try_recv().is_err() {
                    let Some(mut request) = server.recv_timeout(Duration::from_millis(10)).unwrap()
                    else {
                        continue;
                    };
                    let mut body = Vec::new();
                    request.as_reader().read_to_end(&mut body).unwrap();
                    let status = match request.url() {
                        "/v1/traces" => 503,
                        "/v1/metrics" => {
                            let request =
                                ExportMetricsServiceRequest::decode(body.as_slice()).unwrap();
                            for resource in request.resource_metrics {
                                for scope in resource.scope_metrics {
                                    received.lock().unwrap().extend(scope.metrics);
                                }
                            }
                            200
                        }
                        other => panic!("unexpected signal: {other}"),
                    };
                    request.respond(tiny_http::Response::empty(status)).unwrap();
                }
            });
            let mut child = Command::new(std::env::current_exe().unwrap());
            for (key, _) in std::env::vars().filter(|(k, _)| {
                k.starts_with("GST_") || k.starts_with("OTEL_") || k.starts_with("MARS_")
            }) {
                child.env_remove(key);
            }
            let output = child
                .args(["--exact", "monitoring_child", "--ignored", "--nocapture"])
                .env("MARS_TEST_EXPORTER", exporter)
                .env("MARS_TEST_LEVEL", level)
                .env("MARS_METRIC_INTERVAL_MS", "60000")
                .env("OTEL_EXPORTER_OTLP_ENDPOINT", format!("http://{address}"))
                .output()
                .unwrap();
            stop.send(()).unwrap();
            worker.join().unwrap();
            assert!(
                output.status.success(),
                "{exporter}/{level}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let metrics = metrics.lock().unwrap();
            let text = if exporter == "prometheus" {
                assert!(metrics.is_empty(), "Prometheus selection sent OTLP metrics");
                String::from_utf8(output.stdout).unwrap()
            } else {
                assert!(!metrics.is_empty());
                metrics
                    .iter()
                    .map(|m| m.name.as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            assert_eq!(
                text.contains("mars_instrument_records_total"),
                level != "none"
            );
            assert_eq!(
                text.contains("mars_instrument_work_duration_seconds"),
                level == "all"
            );
            for metric in metrics
                .iter()
                .filter(|m| m.name.starts_with("mars_instrument_"))
            {
                let attributes: Vec<_> = match metric.data.as_ref().unwrap() {
                    Data::Sum(s) => s.data_points.iter().map(|p| &p.attributes).collect(),
                    Data::Gauge(s) => s.data_points.iter().map(|p| &p.attributes).collect(),
                    Data::Histogram(s) => s.data_points.iter().map(|p| &p.attributes).collect(),
                    _ => panic!("unexpected health metric type"),
                };
                for labels in attributes {
                    assert!(labels.iter().all(|a| [
                        "signal",
                        "outcome",
                        "reason",
                        "stage",
                        "component",
                        "counter"
                    ]
                    .contains(&a.key.as_str())));
                }
            }
            if exporter == "otlp" && level != "none" {
                assert!(metrics.iter().filter(|m| m.name == "mars_instrument_export_failures_total").any(|m| {
                    let Some(Data::Sum(sum)) = &m.data else { return false };
                    sum.data_points.iter().any(|p| matches!(p.value, Some(opentelemetry_proto::tonic::metrics::v1::number_data_point::Value::AsDouble(v)) if v > 0.0))
                }), "trace export failure was not visible in health metrics");
            }
        }
    }
}

#[test]
#[ignore = "fresh process with explicit endpoints"]
fn monitoring_child() {
    gst::init().unwrap();
    let exporter = std::env::var("MARS_TEST_EXPORTER").unwrap();
    let level = std::env::var("MARS_TEST_LEVEL").unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut params =
        format!("signals=\"metrics,traces\",metrics-exporter={exporter},metrics-port={port}");
    if level != "default" {
        params.push_str(&format!(",monitoring={level}"));
    }
    let _tracer: gstoteltracer::InstrumentTracer =
        glib::Object::builder().property("params", params).build();
    let pipeline = gst::parse::launch("fakesrc num-buffers=20 ! identity ! fakesink sync=false")
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
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
    assert!(gstoteltracer::stats().unwrap().failed_exports > 0);
    let _ = gstoteltracer::flush(Duration::from_secs(5));
    if exporter == "prometheus" {
        let response = reqwest::blocking::get(format!("http://127.0.0.1:{port}/metrics"))
            .unwrap()
            .text()
            .unwrap();
        println!("{response}");
    }
    gstoteltracer::shutdown(Duration::from_secs(5)).unwrap();
}
