//! Both collectors must interpret actual OTLP HTTP responses identically.
use gstreamer::{self as gst, prelude::*};
use opentelemetry_proto::tonic::collector::metrics::v1::{
    ExportMetricsPartialSuccess, ExportMetricsServiceRequest, ExportMetricsServiceResponse,
};
use prost::Message;
use std::{process::Command, time::Duration};

#[test]
fn metric_responses_preserve_warnings_and_reject_invalid_counts() {
    for collector in ["full", "compatibility"] {
        for response in [
            "empty",
            "zero",
            "warning",
            "reject",
            "negative",
            "excess",
            "malformed",
        ] {
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "response_child", "--ignored", "--nocapture"])
                .env_clear()
                .env("METRIC_COLLECTOR", collector)
                .env("METRIC_RESPONSE", response)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{collector}/{response}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}

#[test]
#[ignore]
fn response_child() {
    let mode = std::env::var("METRIC_RESPONSE").unwrap();
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    std::env::set_var(
        "OTEL_EXPORTER_OTLP_ENDPOINT",
        format!("http://{}", server.server_addr()),
    );
    std::env::set_var("OTEL_EXPORTER_OTLP_TIMEOUT", "1000");
    std::env::set_var("MARS_METRIC_INTERVAL_MS", "60000");
    std::env::set_var("GST_INSTRUMENT_METRIC_INTERVAL_MS", "60000");
    std::env::set_var("GST_INSTRUMENT_CPU_SAMPLE_RATE", "1");
    let body = if mode == "malformed" {
        vec![0xff]
    } else {
        ExportMetricsServiceResponse {
            partial_success: (mode != "empty").then(|| ExportMetricsPartialSuccess {
                rejected_data_points: match mode.as_str() {
                    "reject" => 1,
                    "negative" => -1,
                    "excess" => i64::MAX,
                    _ => 0,
                },
                error_message: if mode == "warning" {
                    "accepted with a suggestion".into()
                } else {
                    String::new()
                },
            }),
        }
        .encode_to_vec()
    };
    let (stop, stopping) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut requests = 0;
        while stopping.try_recv().is_err() {
            let Some(mut request) = server.recv_timeout(Duration::from_millis(10)).unwrap() else {
                continue;
            };
            let mut data = Vec::new();
            request.as_reader().read_to_end(&mut data).unwrap();
            let metrics = request.url() == "/v1/metrics";
            if metrics {
                let decoded = ExportMetricsServiceRequest::decode(data.as_slice()).unwrap();
                assert!(!decoded.resource_metrics.is_empty());
                requests += 1;
            }
            request
                .respond(tiny_http::Response::from_data(if metrics {
                    body.clone()
                } else {
                    vec![]
                }))
                .unwrap();
        }
        requests
    });
    gst::init().unwrap();
    let _tracer: gstoteltracer::InstrumentTracer =
        if std::env::var("METRIC_COLLECTOR").unwrap() == "full" {
            glib::Object::builder()
                .property("params", "signals=metrics")
                .build()
        } else {
            glib::Object::new()
        };
    let pipeline =
        gst::parse::launch("fakesrc num-buffers=5 ! identity ! fakesink sync=false").unwrap();
    pipeline.set_state(gst::State::Playing).unwrap();
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
    let expected = if matches!(mode.as_str(), "empty" | "zero" | "warning") {
        Ok(())
    } else {
        Err(gstoteltracer::FlushError::ExportFailed)
    };
    assert_eq!(
        gstoteltracer::flush_report(Duration::from_secs(3)).metrics,
        expected
    );
    assert_eq!(
        gstoteltracer::shutdown_report(Duration::from_secs(3)).metrics,
        expected
    );
    stop.send(()).unwrap();
    let requests = worker.join().unwrap();
    assert!(requests >= 2);
    let stats = gstoteltracer::metric_stats().unwrap();
    if expected.is_ok() {
        assert_eq!(stats.failed_exports, 0);
        assert_eq!(stats.exported_batches, requests);
        assert_eq!(
            stats.export_warnings,
            if mode == "warning" { requests } else { 0 }
        );
    } else {
        assert_eq!(stats.failed_exports, requests);
        assert_eq!(stats.exported_batches + stats.export_warnings, 0);
    }
}
