//! Real log receivers exercise the public deadline and per-signal result APIs.
use gstreamer::{self as gst, prelude::*};
use opentelemetry_proto::tonic::collector::logs::v1::{
    ExportLogsPartialSuccess, ExportLogsServiceRequest, ExportLogsServiceResponse,
};
use prost::Message;
use std::{
    process::Command,
    sync::mpsc,
    time::{Duration, Instant},
};

#[test]
fn log_failures_and_slow_receivers_preserve_cleanup_deadlines() {
    for mode in ["slow", "fail", "reject", "malformed"] {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr().to_string();
        let (stop, stopping) = mpsc::channel();
        let worker =
            std::thread::spawn(move || {
                while stopping.try_recv().is_err() {
                    let Some(mut request) = server.recv_timeout(Duration::from_millis(20)).unwrap()
                    else {
                        continue;
                    };
                    assert_eq!(request.url(), "/v1/logs");
                    let mut body = Vec::new();
                    request.as_reader().read_to_end(&mut body).unwrap();
                    let records = ExportLogsServiceRequest::decode(body.as_slice()).unwrap();
                    let count = records
                        .resource_logs
                        .iter()
                        .flat_map(|r| &r.scope_logs)
                        .map(|s| s.log_records.len())
                        .sum::<usize>();
                    assert!(count > 0);
                    if mode == "slow" {
                        std::thread::sleep(Duration::from_millis(400));
                    }
                    let body = if mode == "malformed" {
                        b"invalid protobuf".to_vec()
                    } else {
                        ExportLogsServiceResponse {
                            partial_success: (mode == "reject").then(|| ExportLogsPartialSuccess {
                                rejected_log_records: count as i64,
                                error_message: "test rejection".into(),
                            }),
                        }
                        .encode_to_vec()
                    };
                    request
                        .respond(
                            tiny_http::Response::from_data(body)
                                .with_status_code(if mode == "fail" { 503 } else { 200 }),
                        )
                        .unwrap();
                }
            });
        let mut child = Command::new(std::env::current_exe().unwrap());
        for (key, _) in std::env::vars().filter(|(k, _)| {
            k.starts_with("GST_") || k.starts_with("OTEL_") || k.starts_with("MARS_")
        }) {
            child.env_remove(key);
        }
        let output = child
            .args(["--exact", "lifecycle_child", "--ignored", "--nocapture"])
            .env("MARS_TEST_MODE", mode)
            .env("GST_DEBUG", "lifecycle-test:3")
            .env("GST_OTEL_FLUSH_TIMEOUT_MS", "10")
            .env("OTEL_BLRP_SCHEDULE_DELAY", "60000")
            .env("OTEL_EXPORTER_OTLP_ENDPOINT", format!("http://{address}"))
            .output()
            .unwrap();
        stop.send(()).unwrap();
        worker.join().unwrap();
        assert!(
            output.status.success(),
            "{mode}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
#[ignore = "isolated global tracer and explicit receiver"]
fn lifecycle_child() {
    gst::init().unwrap();
    let _tracer: gstoteltracer::InstrumentTracer = glib::Object::builder()
        .property("params", "signals=\"logs\"")
        .build();
    let cat = gst::DebugCategory::new("lifecycle-test", gst::DebugColorFlags::empty(), None);
    let pipeline = gst::parse::launch("fakesrc num-buffers=1 ! fakesink sync=false")
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
    gst::warning!(cat, "before pipeline stop");
    let start = Instant::now();
    pipeline.set_state(gst::State::Null).unwrap();
    assert!(
        start.elapsed() < Duration::from_millis(200),
        "pipeline stop exceeded shared 10ms budget"
    );
    gst::warning!(cat, "before process shutdown");
    let slow = std::env::var("MARS_TEST_MODE").unwrap() == "slow";
    if slow {
        let start = Instant::now();
        let report = gstoteltracer::flush_report(Duration::from_millis(10));
        assert!(report.traces.is_ok() && report.metrics.is_ok());
        assert_eq!(report.logs, Err(gstoteltracer::FlushError::Timeout));
        assert!(start.elapsed() < Duration::from_millis(200));
        assert_eq!(
            gstoteltracer::shutdown(Duration::from_millis(10)),
            Err(gstoteltracer::FlushError::Timeout)
        );
    }
    let concurrent = std::thread::spawn(|| gstoteltracer::shutdown_report(Duration::from_secs(5)));
    let report = gstoteltracer::shutdown_report(Duration::from_secs(5));
    let other = concurrent.join().unwrap();
    assert!(report.traces.is_ok() && report.metrics.is_ok());
    assert_eq!(report.logs, other.logs);
    if slow {
        report.result().unwrap();
    } else {
        assert_eq!(report.logs, Err(gstoteltracer::FlushError::ExportFailed));
        assert_eq!(
            report.result(),
            Err(gstoteltracer::FlushError::ExportFailed)
        );
        assert!(gstoteltracer::log_stats().failed > 0);
    }
    assert_eq!(gstoteltracer::log_stats().pending, 0);
}
