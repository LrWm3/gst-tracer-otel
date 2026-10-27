//! Real GStreamer lifecycle coverage for default-on, process-wide heap trimming.
#![cfg(all(
    feature = "profiles",
    target_os = "linux",
    target_env = "gnu",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
use gstreamer::{self as gst, prelude::*};
use std::{
    process::Command,
    time::{Duration, Instant},
};

#[test]
fn trim_properties_and_lifecycle_on_every_entry_point() {
    for entry in ["instrument", "otel", "profiles"] {
        for mode in [
            "default",
            "off-property",
            "off-params",
            "on-params",
            "shutdown-before-playing",
        ] {
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "trim_child", "--ignored", "--nocapture"])
                .env_clear()
                .env("TRIM_ENTRY", entry)
                .env("TRIM_MODE", mode)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{entry}/{mode}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "trim_child", "--ignored", "--nocapture"])
        .env_clear()
        .env("TRIM_ENTRY", "instrument")
        .env("TRIM_MODE", "no-profiles")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore]
fn trim_child() {
    let mode = std::env::var("TRIM_MODE").unwrap();
    let entry = std::env::var("TRIM_ENTRY").unwrap();
    let enabled = matches!(mode.as_str(), "default" | "on-params");
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let url = format!("http://{}", server.server_addr());
    std::env::set_var("MARS_PROFILE_URL", &url);
    std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", &url);
    std::env::set_var("MARS_PROFILE_SYMBOLIZATION", "functions");
    let (stop, rx) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        while rx.try_recv().is_err() {
            if let Some(request) = server.recv_timeout(Duration::from_millis(10)).unwrap() {
                request.respond(tiny_http::Response::empty(200)).unwrap();
            }
        }
    });
    gst::init().unwrap();
    let ty = match entry.as_str() {
        "instrument" => gstoteltracer::InstrumentTracer::static_type(),
        "otel" => gstoteltracer::TelemetryTracer::static_type(),
        "profiles" => gstoteltracer::ProfilesTracer::static_type(),
        _ => unreachable!(),
    };
    // Trimming must work independently of element attribution's message hook.
    let mut builder = glib::Object::builder_with_type(ty)
        .property("profile-elements", gstoteltracer::Toggle::Off);
    if entry == "instrument" {
        builder = builder.property(
            "signals",
            if mode == "no-profiles" {
                "traces"
            } else {
                "profiles"
            },
        );
    }
    match mode.as_str() {
        "off-property" => {
            builder = builder.property("profile-memory-trim", gstoteltracer::Toggle::Off)
        }
        "off-params" => {
            builder = builder
                .property("profile-memory-trim", gstoteltracer::Toggle::On)
                .property("params", "profile-memory-trim=false")
        }
        "on-params" => {
            builder = builder
                .property("profile-memory-trim", gstoteltracer::Toggle::Off)
                .property("params", "profile-memory-trim=true")
        }
        _ => (),
    }
    let _tracer = builder.build();
    gstoteltracer::stats().expect("tracer initialized");
    assert_eq!(gstoteltracer::profile_stats().memory_trim_attempts, 0);
    if mode == "shutdown-before-playing" {
        gstoteltracer::shutdown(Duration::from_secs(5)).unwrap();
    }
    let mut elapsed = None;
    // Multiple pipelines and pause/resume transitions must not repeat trimming.
    for _ in 0..2 {
        let pipeline = gst::parse::launch("fakesrc num-buffers=2 ! fakesink sync=false")
            .unwrap()
            .downcast::<gst::Pipeline>()
            .unwrap();
        pipeline.set_state(gst::State::Paused).unwrap();
        let (state, current, _) = pipeline.state(gst::ClockTime::from_seconds(2));
        state.unwrap();
        assert_eq!(current, gst::State::Paused);
        assert_eq!(
            gstoteltracer::profile_stats().memory_trim_attempts,
            u64::from(elapsed.is_some())
        );
        for _ in 0..2 {
            pipeline.set_state(gst::State::Playing).unwrap();
            let (state, current, _) = pipeline.state(gst::ClockTime::from_seconds(2));
            state.unwrap();
            assert_eq!(current, gst::State::Playing);
            if enabled {
                let until = Instant::now() + Duration::from_secs(3);
                loop {
                    let stats = gstoteltracer::profile_stats();
                    if stats.memory_trim_attempts == 1 && !stats.memory_trim_pending {
                        break;
                    }
                    assert!(Instant::now() < until, "trim did not finish: {stats:?}");
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
            let stats = gstoteltracer::profile_stats();
            assert_eq!(stats.memory_trim_attempts, u64::from(enabled));
            assert!(!stats.memory_trim_pending);
            assert_eq!(stats.memory_trim_failures, 0);
            if enabled {
                assert_eq!(
                    *elapsed.get_or_insert(stats.memory_trim_elapsed_ns),
                    stats.memory_trim_elapsed_ns
                );
            }
            pipeline.set_state(gst::State::Paused).unwrap();
            pipeline.state(gst::ClockTime::from_seconds(2)).0.unwrap();
        }
        pipeline.set_state(gst::State::Null).unwrap();
    }
    gstoteltracer::shutdown(Duration::from_secs(5)).unwrap();
    assert!(!gstoteltracer::profile_stats().memory_trim_pending);
    stop.send(()).unwrap();
    server.join().unwrap();
}
