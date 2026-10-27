//! Construction properties select collectors without consulting source environment variables.
#![cfg(all(
    feature = "profiles",
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
use gstoteltracer::ProfileSources;
use gstreamer::{self as gst, prelude::*};
use std::{process::Command, time::Duration};

#[test]
fn profile_properties_work_on_every_entry_point() {
    for entry in ["instrument", "otel", "profiles"] {
        for mode in [
            "default",
            "property",
            "params",
            "invalid",
            "settings",
            "settings-params",
        ] {
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "sources_child", "--ignored", "--nocapture"])
                .env_clear()
                .env("SOURCES_ENTRY", entry)
                .env("SOURCES_MODE", mode)
                .env("GST_DEBUG", "mars-otel:3")
                // Neither the removed selector nor an adapter path enables GPU collection.
                .env("MARS_PROFILE_SOURCES", "gpu")
                .env("GST_OTEL_PROFILE_SOURCES", "gpu")
                .env("MARS_GPU_ADAPTER", "/no-such-gpu-adapter.so")
                .env("MARS_PROFILE_NVTX_ADAPTER", "/removed-nvtx-selector.so")
                .env("NVTX_INJECTION64_PATH", "/no-such-nvtx-adapter.so")
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success(),
                "{entry}/{mode}: {}\n{stderr}",
                String::from_utf8_lossy(&output.stdout)
            );
            if mode == "invalid" {
                assert!(stderr.contains("profile-sources must contain"));
                #[cfg(not(feature = "gpu-profiles"))]
                assert!(stderr.contains("gpu-profiles Cargo feature"));
            }
        }
    }
}

#[test]
#[ignore]
fn sources_child() {
    let entry = std::env::var("SOURCES_ENTRY").unwrap();
    let mode = std::env::var("SOURCES_MODE").unwrap();
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let url = format!("http://{}", server.server_addr());
    std::env::set_var("MARS_PROFILE_URL", &url);
    std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", &url);
    std::env::set_var("MARS_PROFILE_SYMBOLIZATION", "functions");
    // Leave instrument in compatibility mode: profile-sources alone must not require signals.
    std::env::set_var("GST_INSTRUMENT_METRICS", "false");
    let (stop, stopping) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        while stopping.try_recv().is_err() {
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
    if mode == "invalid" {
        for invalid in ["typo", "cpu,cpu", ""] {
            let _tracer = glib::Object::builder_with_type(ty)
                .property("params", format!("profile-sources=\"{invalid}\""))
                .build();
            assert_eq!(
                gstoteltracer::stats(),
                Err(gstoteltracer::FlushError::NotInitialized)
            );
        }
        #[cfg(not(feature = "gpu-profiles"))]
        for invalid in ["gpu", "cpu,gpu"] {
            let _tracer = glib::Object::builder_with_type(ty)
                .property("params", format!("profile-sources=\"{invalid}\""))
                .build();
            assert_eq!(
                gstoteltracer::stats(),
                Err(gstoteltracer::FlushError::NotInitialized)
            );
        }
        // Invalid construction must not claim the process-wide runtime.
    }
    let mut builder = glib::Object::builder_with_type(ty);
    if mode == "property" {
        builder = builder.property("profile-sources", ProfileSources::Cpu);
    } else if mode == "params" {
        builder = builder.property("params", "profile-sources=cpu");
        #[cfg(feature = "gpu-profiles")]
        {
            // GST_TRACERS parameters take precedence over a direct construction property.
            builder = builder.property("profile-sources", ProfileSources::Gpu);
        }
    }
    if mode.starts_with("settings") {
        for key in [
            "MARS_PROFILE_URL",
            "MARS_PROFILE_HZ",
            "MARS_PROFILE_SYMBOLIZATION",
            "MARS_PROFILE_ELEMENTS",
            "MARS_PROFILE_TIMEOUT_MS",
            "MARS_PROFILE_SIGNAL",
        ] {
            std::env::set_var(key, "invalid-overridden-environment");
        }
        // Use explicit built-in defaults too: they must override bad environment values.
        builder = builder
            .property(
                "profile-url",
                if mode == "settings-params" {
                    "invalid-direct-property"
                } else {
                    &url
                },
            )
            .property(
                "profile-hz",
                if mode == "settings-params" {
                    1u32
                } else {
                    100u32
                },
            )
            .property("profile-timeout-ms", 10000u32)
            .property(
                "profile-symbolization",
                gstoteltracer::ProfileSymbolization::Functions,
            )
            .property(
                "profile-elements",
                if mode == "settings-params" {
                    gstoteltracer::Toggle::On
                } else {
                    gstoteltracer::Toggle::Off
                },
            )
            .property("profile-signal", (libc::SIGRTMIN() + 6) as u32);
        if mode == "settings-params" {
            builder = builder.property("params", format!("profile-url=\"{url}\",profile-hz=100,profile-timeout-ms=10000,profile-symbolization=functions,profile-elements=false,profile-signal={}", libc::SIGRTMIN() + 6));
        }
    }
    let tracer = builder.build();
    assert_eq!(
        tracer.property::<ProfileSources>("profile-sources"),
        ProfileSources::Cpu
    );
    assert!(tracer
        .find_property("profile-sources")
        .unwrap()
        .flags()
        .contains(glib::ParamFlags::CONSTRUCT_ONLY));
    gstoteltracer::stats().unwrap();
    let pipeline =
        gst::parse::launch("fakesrc num-buffers=3 ! identity name=work ! fakesink sync=false")
            .unwrap()
            .downcast::<gst::Pipeline>()
            .unwrap();
    pipeline
        .by_name("work")
        .unwrap()
        .connect("handoff", false, |_| {
            let cpu_time = || {
                let mut time = libc::timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                };
                assert_eq!(
                    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) },
                    0
                );
                time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64
            };
            let start = cpu_time();
            while cpu_time() - start < 30_000_000 {
                std::hint::black_box((0u64..1000).sum::<u64>());
            }
            None
        });
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
    drop(message);
    pipeline.set_state(gst::State::Null).unwrap();
    drop(pipeline);
    gstoteltracer::shutdown(Duration::from_secs(5)).unwrap();
    let cpu = gstoteltracer::profile_stats();
    assert!(cpu.accepted_samples > 0);
    if mode.starts_with("settings") {
        assert_eq!(cpu.element_samples + cpu.task_samples, 0);
    }
    assert_eq!(
        cpu.active_threads + cpu.pending_samples + cpu.failed_samples,
        0
    );
    stop.send(()).unwrap();
    worker.join().unwrap();
}
