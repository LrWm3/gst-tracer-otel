//! Decode actual Pyroscope uploads and OTLP spans from a fresh process. The
//! child uses real CPU timers, nested pad pushes, and a GStreamer queue thread.
#![cfg(all(
    feature = "profiles",
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use gstreamer::{self as gst, prelude::*};
use opentelemetry_proto::tonic::{
    collector::{
        logs::v1::ExportLogsServiceRequest, metrics::v1::ExportMetricsServiceRequest,
        trace::v1::ExportTraceServiceRequest,
    },
    logs::v1::LogRecord,
    metrics::v1::{metric::Data, Exemplar},
    trace::v1::Span,
};
use prost::Message;
use pyroscope::encode::gen::{google::Profile, push::PushRequest};
use std::{
    collections::{HashMap, HashSet},
    io::Read,
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

#[derive(Default)]
struct Received {
    requests: Vec<String>,
    spans: Vec<Span>,
    profiles: Vec<Profile>,
    exemplars: Vec<Exemplar>,
    logs: Vec<LogRecord>,
}

fn collect(mode: &str) -> (Received, String) {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let address = server.server_addr().to_string();
    let received = Arc::new(Mutex::new(Received::default()));
    let output = received.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = stop.clone();
    let fail = mode.ends_with("slow");
    let worker = std::thread::spawn(move || {
        while !stopping.load(Ordering::Relaxed) {
            let Some(mut request) = server.recv_timeout(Duration::from_millis(10)).unwrap() else {
                continue;
            };
            let mut bytes = Vec::new();
            request.as_reader().read_to_end(&mut bytes).unwrap();
            output
                .lock()
                .unwrap()
                .requests
                .push(request.url().to_owned());
            if request.url() == "/v1/traces" {
                let decoded = ExportTraceServiceRequest::decode(bytes.as_slice()).unwrap();
                for resource in decoded.resource_spans {
                    for scope in resource.scope_spans {
                        output.lock().unwrap().spans.extend(scope.spans);
                    }
                }
            } else if request.url() == "/v1/metrics" {
                let decoded = ExportMetricsServiceRequest::decode(bytes.as_slice()).unwrap();
                for resource in decoded.resource_metrics {
                    for scope in resource.scope_metrics {
                        for metric in scope.metrics {
                            if let Some(Data::Histogram(histogram)) = metric.data {
                                for point in histogram.data_points {
                                    output.lock().unwrap().exemplars.extend(point.exemplars);
                                }
                            }
                        }
                    }
                }
            } else if request.url() == "/v1/logs" {
                let decoded = ExportLogsServiceRequest::decode(bytes.as_slice()).unwrap();
                for resource in decoded.resource_logs {
                    for scope in resource.scope_logs {
                        output.lock().unwrap().logs.extend(scope.log_records);
                    }
                }
            } else {
                assert_eq!(request.url(), "/push.v1.PusherService/Push");
                let mut data = Vec::new();
                libflate::gzip::Decoder::new(bytes.as_slice())
                    .unwrap()
                    .read_to_end(&mut data)
                    .unwrap();
                let decoded = PushRequest::decode(data.as_slice()).unwrap();
                for series in decoded.series {
                    assert!(series
                        .labels
                        .iter()
                        .any(|l| l.name == "service_name" && l.value == "profile-test"));
                    assert!(!series
                        .labels
                        .iter()
                        .any(|l| l.name == "span_id" || l.name == "trace_id"));
                    for sample in series.samples {
                        output
                            .lock()
                            .unwrap()
                            .profiles
                            .push(Profile::decode(sample.raw_profile.as_slice()).unwrap());
                    }
                }
                if fail {
                    // Keep accepting traces while the profile upload stalls.
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_secs(2));
                        let _ = request.respond(tiny_http::Response::empty(503));
                    });
                    continue;
                }
            }
            request.respond(tiny_http::Response::empty(200)).unwrap();
        }
    });
    let mut command = Command::new(std::env::current_exe().unwrap());
    for (key, _) in std::env::vars()
        .filter(|(k, _)| k.starts_with("GST_") || k.starts_with("OTEL_") || k.starts_with("MARS_"))
    {
        command.env_remove(key);
    }
    command
        .args(["--exact", "profile_child", "--ignored", "--nocapture"])
        .env("OTEL_PROFILE_TEST", mode)
        .env("OTEL_SERVICE_NAME", "profile-test")
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", format!("http://{address}"))
        .env(
            "OTEL_TRACES_SAMPLER",
            if mode == "unsampled" {
                "parentbased_always_off"
            } else {
                "parentbased_always_on"
            },
        );
    if matches!(mode, "instrument" | "signals") {
        command
            .env("GST_INSTRUMENT_LOGS", "true")
            .env("GST_DEBUG", "instrument-test:4");
    }
    if mode == "signals" {
        command
            .env("MARS_SIGNALS", "metrics,traces,profiles,logs")
            .env("MARS_METRIC_SAMPLE_RATE", "1");
    }
    if mode == "profiles-only-signals" {
        command.env("MARS_PROFILE_URL", format!("http://{address}"));
    }
    if mode.starts_with("profiles-only") {
        // Profiles-only selection must ignore unrelated telemetry settings,
        // including enabled signals and malformed trace/metric configuration.
        command
            .env("GST_OTEL_LOGS", "true")
            .env("GST_INSTRUMENT_LOGS", "true")
            .env("GST_INSTRUMENT_TRACES", "true")
            .env("GST_INSTRUMENT_METRICS", "true")
            .env("GST_INSTRUMENT_PROFILES", "false")
            .env("OTEL_TRACES_SAMPLER", "not-a-sampler")
            .env("GST_OTEL_QUEUE_CAPACITY", "0")
            .env("GST_OTEL_METRIC_MAX_SERIES", "0")
            .env("GST_DEBUG", "instrument-test:4");
    }
    if mode != "disabled" {
        command.env("GST_OTEL_PROFILE_URL", format!("http://{address}"));
    }
    if mode.ends_with("invalid-rate") {
        command.env("GST_OTEL_PROFILE_HZ", "0");
    }
    if mode.ends_with("invalid-elements") {
        command.env("GST_OTEL_PROFILE_ELEMENTS", "invalid");
    }
    let mut child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let stdout = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).unwrap();
        bytes
    });
    let stderr = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).unwrap();
        bytes
    });
    let deadline = Instant::now() + Duration::from_secs(45);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            break child.wait().unwrap();
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let child = std::process::Output {
        status,
        stdout: stdout.join().unwrap(),
        stderr: stderr.join().unwrap(),
    };
    stop.store(true, Ordering::Relaxed);
    worker.join().unwrap();
    assert!(
        child.status.success(),
        "child failed: {}\n{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
    let result = Arc::try_unwrap(received)
        .ok()
        .unwrap()
        .into_inner()
        .unwrap();
    (result, String::from_utf8(child.stdout).unwrap())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn pipeline_churn_reclaims_descriptions_and_preserves_early_and_late_samples() {
    let (received, stdout) = collect("profiles-only-churn");
    assert!(stdout.contains("CHURN cycles=1800"));
    for name in ["cycle0", "cycle1799"] {
        assert!(
            received.profiles.iter().any(|profile| profile
                .sample
                .iter()
                .any(|sample| scope_frame(profile, sample).contains(name))),
            "missing {name} samples: {stdout}"
        );
    }
}

#[test]
fn real_samples_match_nested_and_queue_spans_in_uploaded_pprof() {
    let (received, stdout) = collect("sampled");
    let expected: HashMap<_, _> = stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.strip_prefix("EXPECTED ")?.split_whitespace();
            let phase = fields.next()?.to_owned();
            Some((
                (fields.next()?.to_owned(), fields.next()?.to_owned()),
                phase,
            ))
        })
        .collect();
    let spans: HashMap<_, _> = received
        .spans
        .iter()
        .map(|span| ((hex(&span.trace_id), hex(&span.span_id)), span))
        .collect();
    let mut phases = HashSet::new();
    let mut tids = HashSet::new();
    let mut nested_body = false;
    for profile in &received.profiles {
        assert_eq!(profile.period, 10_000_000);
        for sample in &profile.sample {
            let labels: HashMap<_, _> = sample
                .label
                .iter()
                .map(|l| {
                    (
                        profile.string_table[l.key as usize].as_str(),
                        profile.string_table[l.str as usize].as_str(),
                    )
                })
                .collect();
            let Some(span_id) = labels.get("span_id") else {
                continue;
            };
            let key = (labels["trace_id"].to_owned(), (*span_id).to_owned());
            let span = spans
                .get(&key)
                .expect("profile sample must refer to an exported span");
            assert!(span
                .attributes
                .iter()
                .any(|a| a.key == "pyroscope.profile.id"
                    && format!("{:?}", a.value).contains(*span_id)));
            if let Some(phase) = expected.get(&key) {
                phases.insert(phase.as_str());
                let root = scope_frame(profile, sample);
                match phase.as_str() {
                    "outer" => assert!(
                        root.starts_with("[element]") && root.contains("GstIdentity:outer"),
                        "{root}"
                    ),
                    "queue" => assert!(
                        root.starts_with("[element]") && root.contains("GstIdentity:worker"),
                        "{root}"
                    ),
                    "nested" => {
                        // A push span includes dispatch before chain-pre and
                        // after chain-post. That work still executes in outer;
                        // the factory-less nested chain body is unattributed.
                        nested_body |= root == "[unattributed GStreamer CPU]";
                        assert!(
                            root == "[unattributed GStreamer CPU]"
                                || (root.starts_with("[element]")
                                    && root.contains("GstIdentity:outer")),
                            "{root}"
                        );
                    }
                    _ => unreachable!(),
                }
            }
            tids.insert(labels["thread_id"].to_owned());
            assert!(!sample.location_id.is_empty());
            assert!(sample.value[0] > 0);
        }
    }
    assert!(phases.contains("outer"), "{phases:?}");
    assert!(phases.contains("nested"), "{phases:?}");
    assert!(phases.contains("queue"), "{phases:?}");
    assert!(
        nested_body,
        "the nested chain body must not inherit its caller's element"
    );
    assert!(tids.len() >= 2, "queue must sample its own native thread");
}

#[test]
fn unsampled_traces_still_profile_without_span_labels() {
    let (received, _) = collect("unsampled");
    assert!(received.spans.is_empty());
    assert!(received.profiles.iter().any(|p| !p.sample.is_empty()));
    for profile in received.profiles {
        for sample in profile.sample {
            assert!(sample.label.iter().all(|l| !matches!(
                profile.string_table[l.key as usize].as_str(),
                "span_id" | "trace_id"
            )));
        }
    }
}

#[test]
fn disabled_or_failed_profiler_initialization_preserves_tracing() {
    for mode in ["disabled", "invalid-rate", "signal-conflict"] {
        let (received, _) = collect(mode);
        assert!(!received.spans.is_empty());
        assert!(received.profiles.is_empty());
        assert!(received
            .spans
            .iter()
            .all(|s| s.attributes.iter().all(|a| a.key != "pyroscope.profile.id")));
    }
}

#[test]
fn slow_profile_receiver_does_not_extend_shutdown_deadline() {
    collect("slow");
}

#[test]
fn profiles_only_uploads_cpu_samples_without_any_otlp_requests() {
    for mode in [
        "profiles-only",
        "profiles-only-list",
        "profiles-only-pull",
        "profiles-only-signals",
    ] {
        let (received, _) = collect(mode);
        assert!(!received.requests.is_empty(), "{mode}");
        assert!(
            received
                .requests
                .iter()
                .all(|path| path == "/push.v1.PusherService/Push"),
            "{mode}: {:?}",
            received.requests
        );
        assert!(
            received.profiles.iter().any(|p| !p.sample.is_empty()),
            "{mode}"
        );
        let mut tids = HashSet::new();
        for profile in &received.profiles {
            assert_eq!(profile.period, 10_000_000);
            for sample in &profile.sample {
                assert!(!sample.location_id.is_empty());
                for label in &sample.label {
                    let key = profile.string_table[label.key as usize].as_str();
                    assert!(!matches!(key, "span_id" | "trace_id"));
                    if key == "thread_id" {
                        tids.insert(profile.string_table[label.str as usize].clone());
                    }
                }
            }
        }
        assert!(!tids.is_empty());
        if mode == "profiles-only" {
            assert!(
                tids.len() >= 2,
                "source and queue threads must both be profiled"
            );
        }
    }
}

#[test]
fn profiles_only_rejects_invalid_configuration_without_starting_other_signals() {
    for mode in [
        "profiles-only-invalid-rate",
        "profiles-only-invalid-elements",
        "profiles-only-signal-conflict",
        "profiles-only-legacy-params",
    ] {
        let (received, _) = collect(mode);
        assert!(received.requests.is_empty());
    }
}

#[test]
fn profiles_only_shutdown_keeps_its_deadline_with_a_slow_receiver() {
    collect("profiles-only-slow");
}

#[test]
fn profile_action_flush_preserves_sampling_for_later_pipelines() {
    let (received, _) = collect("profiles-only-flush");
    assert!(!received.profiles.is_empty());
}

#[inline(never)]
fn busy(phase: &str) {
    static CAT: std::sync::LazyLock<gst::DebugCategory> = std::sync::LazyLock::new(|| {
        gst::DebugCategory::new("instrument-test", gst::DebugColorFlags::empty(), None)
    });
    gst::info!(*CAT, "CPU phase {phase}");
    if let Some(context) = gstoteltracer::current_span_context().filter(|c| c.is_sampled()) {
        println!(
            "EXPECTED {phase} {} {}",
            context.trace_id(),
            context.span_id()
        );
    }
    let start = Instant::now();
    let mut value = 1u64;
    while start.elapsed() < Duration::from_millis(80) {
        for _ in 0..10000 {
            value = std::hint::black_box(value.wrapping_mul(6364136223846793005).wrapping_add(1));
        }
    }
    std::hint::black_box(value);
}

#[test]
#[ignore = "subprocess with isolated signal disposition, GStreamer and profiler globals"]
fn profile_child() {
    let mode = std::env::var("OTEL_PROFILE_TEST").unwrap();
    let profiles_only = mode.starts_with("profiles-only");
    let slow = mode.ends_with("slow");
    if mode.ends_with("signal-conflict") {
        // SAFETY: isolated child reserves the default profiling signal first.
        unsafe {
            libc::signal(libc::SIGRTMIN() + 6, libc::SIG_IGN);
        }
    }
    gst::init().unwrap();
    let _tracer: gst::Tracer = if mode == "profiles-only-signals" {
        glib::Object::builder::<gstoteltracer::InstrumentTracer>()
            .property("params", "signals=profiles")
            .build()
            .upcast()
    } else if profiles_only {
        if mode.ends_with("legacy-params") {
            glib::Object::builder::<gstoteltracer::ProfilesTracer>()
                .property("params", "sample-rate=100")
                .build()
                .upcast()
        } else {
            glib::Object::new::<gstoteltracer::ProfilesTracer>().upcast()
        }
    } else if matches!(mode.as_str(), "instrument" | "signals") {
        glib::Object::new::<gstoteltracer::InstrumentTracer>().upcast()
    } else {
        glib::Object::new::<gstoteltracer::TelemetryTracer>().upcast()
    };
    if profiles_only
        && (mode.ends_with("invalid-rate")
            || mode.ends_with("invalid-elements")
            || mode.ends_with("signal-conflict")
            || mode.ends_with("legacy-params"))
    {
        assert_eq!(
            gstoteltracer::stats(),
            Err(gstoteltracer::FlushError::NotInitialized)
        );
        assert_eq!(
            gstoteltracer::profile_stats(),
            gstoteltracer::ProfileStats::default()
        );
        return;
    }
    // All entries share one process configuration. Later tracer instances must
    // neither enable OTLP nor install duplicate hooks that clear active IDs.
    let _duplicate_profiles = glib::Object::new::<gstoteltracer::ProfilesTracer>();
    let _duplicate_traces = glib::Object::new::<gstoteltracer::TelemetryTracer>();
    let _duplicate_instrument = glib::Object::new::<gstoteltracer::InstrumentTracer>();

    if mode == "profiles-only-churn" {
        for cycle in 0..1800 {
            let pipeline = gst::parse::launch(
                "fakesrc num-buffers=1 ! identity name=work ! fakesink sync=false",
            )
            .unwrap()
            .downcast::<gst::Pipeline>()
            .unwrap();
            pipeline.set_property("name", format!("cycle{cycle}"));
            if cycle == 0 || cycle == 1799 {
                pipeline
                    .by_name("work")
                    .unwrap()
                    .connect("handoff", false, |_| {
                        busy_cpu(80);
                        None
                    });
            }
            let weak = pipeline.downgrade();
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
            drop(message);
            drop(pipeline);
            // A task may release its final message/object reference just after
            // the NULL transition. Require destruction within a bounded grace.
            let deadline = Instant::now() + Duration::from_secs(1);
            while weak.upgrade().is_some() {
                assert!(Instant::now() < deadline, "pipeline retained at {cycle}");
                std::thread::sleep(Duration::from_millis(1));
            }
            if cycle % 200 == 199 {
                gstoteltracer::flush(Duration::from_secs(5)).unwrap();
                let stats = gstoteltracer::profile_stats();
                assert_eq!(stats.context_capacity_rejections, 0, "{stats:?}");
                assert!(stats.context_descriptions < 20, "{stats:?}");
            }
        }
        let stats = gstoteltracer::profile_stats();
        assert!(stats.context_retired > 4096, "{stats:?}");
        println!("CHURN cycles=1800 {stats:?}");
    } else if mode == "profiles-only-elements" {
        profile_element_work();
    } else if mode == "profiles-only-stack" {
        let pipeline = gst::parse::launch(
            "fakesrc num-buffers=1 ! identity name=stack-check ! fakesink sync=false",
        )
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        pipeline
            .by_name("stack-check")
            .unwrap()
            .connect("handoff", false, |_| {
                profile_stack_outer();
                None
            });
        pipeline.set_state(gst::State::Playing).unwrap();
        let message = pipeline
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(10),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .unwrap();
        assert_eq!(message.type_(), gst::MessageType::Eos);
        pipeline.set_state(gst::State::Null).unwrap();
    } else if mode.ends_with("-list") || mode.ends_with("-pull") {
        profile_non_push_work(&mode);
    } else {
        profile_push_work();
    }
    if profiles_only {
        assert!(gstoteltracer::current_span_context().is_none());
        assert_eq!(
            gstoteltracer::stats().unwrap(),
            gstoteltracer::ExportStats::default()
        );
        let metrics = gstoteltracer::metric_stats().unwrap();
        assert_eq!(metrics.recorded_pushes, 0);
        assert_eq!(metrics.active_series, 0);
        assert_eq!(metrics.exported_batches, 0);
        assert_eq!(metrics.failed_exports, 0);
        assert_eq!(gstoteltracer::profile_stats().correlated_samples, 0);
    }
    let invalid: gst::Structure = _tracer.emit_by_name("flush", &[&60001u64]);
    assert!(!invalid.get::<bool>("success").unwrap());
    if slow {
        let start = Instant::now();
        let report = gstoteltracer::flush_report(Duration::from_millis(10));
        assert_eq!(
            report.profiles,
            Some(Err(gstoteltracer::FlushError::Timeout))
        );
        assert!(start.elapsed() < Duration::from_secs(1));
    } else {
        let samples = gstoteltracer::profile_stats().samples;
        let report: gst::Structure = _tracer.emit_by_name("flush", &[&5000u64]);
        assert!(report.get::<bool>("success").unwrap(), "{report:?}");
        assert!(!report.get::<bool>("profile-delivery-confirmed").unwrap());
        assert!(gstoteltracer::profile_stats().reported_samples >= samples);
        if mode == "profiles-only-flush" {
            profile_push_work();
            assert!(gstoteltracer::profile_stats().samples > samples);
        }
    }
    let start = Instant::now();
    let shutdown = gstoteltracer::shutdown(Duration::from_millis(if slow { 30 } else { 5000 }));
    if slow {
        assert_eq!(shutdown, Err(gstoteltracer::FlushError::Timeout));
        assert!(start.elapsed() < Duration::from_secs(1));
        // Cleanup is asynchronous after timeout; a retry must finish it.
        gstoteltracer::shutdown(Duration::from_secs(10)).unwrap();
    } else {
        shutdown.unwrap();
    }
    let stats = gstoteltracer::profile_stats();
    println!("STATS {stats:?}");
    assert_eq!(stats.active_threads, 0);
    if profiles_only || matches!(mode.as_str(), "sampled" | "unsampled") {
        assert!(stats.samples > 0, "{stats:?}");
        assert_eq!(stats.dropped_full, 0, "{stats:?}");
        assert_eq!(stats.dropped_processing, 0, "{stats:?}");
        assert_eq!(stats.dropped_upload, 0, "{stats:?}");
        assert_eq!(stats.registration_failures, 0, "{stats:?}");
        assert_eq!(
            stats.reported_samples, stats.samples,
            "final report must drain samples"
        );
    }
    let report: gst::Structure = _tracer.emit_by_name("shutdown", &[&1000u64]);
    assert!(report.get::<bool>("success").unwrap(), "{report:?}");
}

fn profile_non_push_work(mode: &str) {
    let src = gst::Pad::builder(gst::PadDirection::Src)
        .name("profile-source")
        .getrange_function(|_, _, _, _, size| {
            assert!(gstoteltracer::current_span_context().is_none());
            busy("pull");
            Ok(gst::PadGetRangeSuccess::NewBuffer(
                gst::Buffer::with_size(size as usize).unwrap(),
            ))
        })
        .build();
    let sink = gst::Pad::builder(gst::PadDirection::Sink)
        .name("profile-sink")
        .chain_list_function(|_, _, _| {
            assert!(gstoteltracer::current_span_context().is_none());
            busy("list");
            Ok(gst::FlowSuccess::Ok)
        })
        .build();
    src.link(&sink).unwrap();
    if mode.ends_with("-pull") {
        src.activate_mode(gst::PadMode::Pull, true).unwrap();
        sink.activate_mode(gst::PadMode::Pull, true).unwrap();
        for offset in 0..8 {
            sink.pull_range(offset, 1).unwrap();
        }
        sink.activate_mode(gst::PadMode::Pull, false).unwrap();
        src.activate_mode(gst::PadMode::Pull, false).unwrap();
    } else {
        src.set_active(true).unwrap();
        sink.set_active(true).unwrap();
        src.push_event(gst::event::StreamStart::new("list"));
        src.push_event(gst::event::Segment::new(&gst::FormattedSegment::<
            gst::format::Bytes,
        >::new()));
        for _ in 0..8 {
            let mut list = gst::BufferList::new();
            list.get_mut().unwrap().add(gst::Buffer::new());
            src.push_list(list).unwrap();
        }
        src.set_active(false).unwrap();
        sink.set_active(false).unwrap();
    }
}

fn profile_push_work() {
    let source = gst::Pad::builder(gst::PadDirection::Src)
        .name("nested-source")
        .build();
    let sink = gst::Pad::builder(gst::PadDirection::Sink)
        .name("nested-sink")
        .chain_function(|_, _, _| {
            busy("nested");
            Ok(gst::FlowSuccess::Ok)
        })
        .build();
    source.link(&sink).unwrap();
    source.set_active(true).unwrap();
    sink.set_active(true).unwrap();
    source.push_event(gst::event::StreamStart::new("nested"));
    source.push_event(gst::event::Segment::new(&gst::FormattedSegment::<
        gst::format::Bytes,
    >::new()));
    let pipeline = gst::parse::launch("fakesrc num-buffers=4 ! identity name=outer ! queue ! identity name=worker ! fakesink sync=false")
        .unwrap().downcast::<gst::Pipeline>().unwrap();
    let nested = source.clone();
    pipeline
        .by_name("outer")
        .unwrap()
        .connect("handoff", false, move |_| {
            let before = gstoteltracer::current_span_context();
            busy("outer");
            nested.push(gst::Buffer::new()).unwrap();
            assert_eq!(
                gstoteltracer::current_span_context(),
                before,
                "nested return must restore parent"
            );
            busy("outer");
            None
        });
    pipeline
        .by_name("worker")
        .unwrap()
        .connect("handoff", false, |_| {
            busy("queue");
            None
        });
    pipeline.set_state(gst::State::Playing).unwrap();
    let message = pipeline
        .bus()
        .unwrap()
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(20),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .expect("EOS");
    if let gst::MessageView::Error(e) = message.view() {
        panic!("{} {:?}", e.error(), e.debug());
    }
    pipeline.set_state(gst::State::Null).unwrap();
    source.set_active(false).unwrap();
    sink.set_active(false).unwrap();
}

#[inline(never)]
fn busy_cpu(milliseconds: u64) {
    fn now() -> u64 {
        let mut time = std::mem::MaybeUninit::<libc::timespec>::uninit();
        // SAFETY: checked clock_gettime initializes the output.
        assert_eq!(
            unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, time.as_mut_ptr()) },
            0
        );
        let time = unsafe { time.assume_init() };
        time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64
    }
    let until = now() + milliseconds * 1_000_000;
    let mut value = 1u64;
    while now() < until {
        for _ in 0..10000 {
            value = std::hint::black_box(value.wrapping_mul(6364136223846793005).wrapping_add(1));
        }
    }
}

#[inline(never)]
fn profile_stack_inner() {
    busy_cpu(500);
    std::hint::black_box(());
}

#[inline(never)]
fn profile_stack_outer() {
    profile_stack_inner();
    std::hint::black_box(());
}

#[test]
#[ignore = "requires -C force-frame-pointers=yes; explicitly run by native profiling CI"]
fn native_frame_chain_retains_known_callers_under_an_element_root() {
    let (received, stdout) = collect("profiles-only-stack");
    let mut matched = 0;
    for profile in &received.profiles {
        for sample in &profile.sample {
            let names: Vec<_> = sample
                .location_id
                .iter()
                .flat_map(|id| {
                    profile
                        .location
                        .iter()
                        .find(|l| l.id == *id)
                        .unwrap()
                        .line
                        .iter()
                        .map(|line| {
                            let function = profile
                                .function
                                .iter()
                                .find(|f| f.id == line.function_id)
                                .unwrap();
                            profile.string_table[function.name as usize].as_str()
                        })
                })
                .collect();
            let inner = names
                .iter()
                .position(|n| n.contains("::profile_stack_inner"));
            let outer = names
                .iter()
                .position(|n| n.contains("::profile_stack_outer"));
            if let (Some(inner), Some(outer)) = (inner, outer) {
                assert!(inner < outer, "leaf-first caller ordering: {names:?}");
                assert!(scope_frame(profile, sample).contains("GstIdentity:stack-check"));
                matched += 1;
            }
        }
    }
    assert!(
        matched > 0,
        "no known native caller chain captured; build with -C force-frame-pointers=yes\n{stdout}"
    );
}

fn profile_element_work() {
    let pipeline = gst::parse::launch("fakesrc name=producer num-buffers=10 signal-handoffs=true ! identity name=light ! queue ! identity name=heavy ! fakesink sync=false")
        .unwrap().downcast::<gst::Pipeline>().unwrap();
    for (name, cpu_ms) in [("producer", 20), ("light", 40), ("heavy", 120)] {
        pipeline
            .by_name(name)
            .unwrap()
            .connect("handoff", false, move |_| {
                busy_cpu(cpu_ms);
                None
            });
    }
    pipeline.set_state(gst::State::Playing).unwrap();
    let message = pipeline
        .bus()
        .unwrap()
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(20),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .unwrap();
    assert_eq!(message.type_(), gst::MessageType::Eos);
    pipeline.set_state(gst::State::Null).unwrap();
}

fn scope_frame<'a>(
    profile: &'a Profile,
    sample: &pyroscope::encode::gen::google::Sample,
) -> &'a str {
    let location = profile
        .location
        .iter()
        .find(|l| Some(&l.id) == sample.location_id.last())
        .unwrap();
    let function = profile
        .function
        .iter()
        .find(|f| f.id == location.line[0].function_id)
        .unwrap();
    &profile.string_table[function.name as usize]
}

#[test]
fn profiles_distinguish_same_factory_instances_and_source_task_work() {
    let (received, stdout) = collect("profiles-only-elements");
    assert!(received.spans.is_empty());
    let mut weights = HashMap::<String, i64>::new();
    for profile in &received.profiles {
        for sample in &profile.sample {
            let root = scope_frame(profile, sample);
            assert!(
                root.starts_with("[element]")
                    || root.starts_with("[task owner]")
                    || root.starts_with("[unattributed"),
                "{root}"
            );
            *weights.entry(root.into()).or_default() += sample.value[0];
        }
    }
    let weight = |name: &str, kind: &str| {
        weights
            .iter()
            .filter(|(k, _)| k.starts_with(kind) && k.contains(name))
            .map(|(_, v)| *v)
            .sum::<i64>()
    };
    let light = weight("GstIdentity:light", "[element]");
    let heavy = weight("GstIdentity:heavy", "[element]");
    assert!(light > 200_000_000, "{weights:?}\n{stdout}");
    assert!(
        (2.0..4.0).contains(&(heavy as f64 / light as f64)),
        "{weights:?}"
    );
    assert!(
        weight("GstFakeSrc:producer", "[task owner]") > 100_000_000,
        "{weights:?}"
    );
    assert_eq!(weight("GstIdentity:heavy", "[task owner]"), 0);
}

#[test]
fn instrument_links_metric_exemplars_to_traces_logs_and_real_cpu_samples() {
    check_instrument_correlation("instrument");
}

#[test]
fn explicit_signals_link_full_metrics_traces_logs_and_profiles() {
    check_instrument_correlation("signals");
}

fn check_instrument_correlation(mode: &str) {
    let (received, _) = collect(mode);
    assert!(!received.exemplars.is_empty());
    let mut profiles = HashSet::new();
    for profile in &received.profiles {
        for sample in &profile.sample {
            let labels: HashMap<_, _> = sample
                .label
                .iter()
                .map(|l| {
                    (
                        profile.string_table[l.key as usize].as_str(),
                        profile.string_table[l.str as usize].as_str(),
                    )
                })
                .collect();
            if let (Some(trace), Some(span)) = (labels.get("trace_id"), labels.get("span_id")) {
                profiles.insert(((*trace).to_string(), (*span).to_string()));
            }
        }
    }
    let linked: Vec<_> = received
        .exemplars
        .iter()
        .filter(|e| profiles.contains(&(hex(&e.trace_id), hex(&e.span_id))))
        .collect();
    assert!(
        !linked.is_empty(),
        "expected a metric exemplar with actual CPU samples"
    );
    for example in linked {
        let span = received
            .spans
            .iter()
            .find(|s| s.trace_id == example.trace_id && s.span_id == example.span_id)
            .expect("matching exported trace span");
        assert!(span
            .attributes
            .iter()
            .any(|a| a.key == "pyroscope.profile.id"
                && format!("{:?}", a.value).contains(&hex(&span.span_id))));
    }
    assert!(
        received.logs.iter().any(|log| received
            .spans
            .iter()
            .any(|span| span.trace_id == log.trace_id && span.span_id == log.span_id)),
        "matching log trace/span fields"
    );
}
