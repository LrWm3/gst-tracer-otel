//! Real wire/lifecycle test using a fake adapter, requiring no CUDA installation.
#![cfg(all(feature = "gpu-profiles", target_os = "linux"))]
use gstreamer::{self as gst, prelude::*};
use prost::Message;
use pyroscope::encode::gen::{google::Profile, push::PushRequest};
use std::{
    collections::HashSet,
    io::{Read, Write},
    process::Command,
    time::Duration,
};

#[test]
fn deferred_gpu_profiles_keep_element_and_span_without_cpu_sampling() {
    let path = std::env::temp_dir().join(format!("mars-gpu-mock-{}.so", std::process::id()));
    let status = Command::new("cc")
        .args([
            "-std=c11",
            "-shared",
            "-fPIC",
            "-pthread",
            "native/gpu_mock.c",
            "native/gpu_capture.c",
            "-o",
        ])
        .arg(&path)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .unwrap();
    assert!(status.success());
    for mode in [
        "success",
        "reject",
        "timeout",
        "combined",
        "otel",
        "params",
        "combined-params",
        "profiles-only",
        "adapter-property",
        "adapter-params",
        "split-reports",
        "window-property",
        "window-params",
        "combined-window",
    ] {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "gpu_child", "--ignored", "--nocapture"])
            .env_clear()
            .env("MARS_GPU_ADAPTER", &path)
            .env("GPU_TEST_MODE", mode)
            .env("GST_DEBUG", "mars-otel:2")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{mode}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(Command::new("cc")
        .args([
            "-std=c11",
            "-shared",
            "-fPIC",
            "-pthread",
            "-DMARS_GPU_LEGACY_MOCK",
            "native/gpu_mock.c",
            "-o"
        ])
        .arg(&path)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .unwrap()
        .success());
    for mode in ["success", "legacy-window"] {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "gpu_child", "--ignored", "--nocapture"])
            .env_clear()
            .env("MARS_GPU_ADAPTER", &path)
            .env("GPU_TEST_MODE", mode)
            .env("GST_DEBUG", "mars-otel:2")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{mode}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if mode == "legacy-window" {
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("lacks capture-window support")
            );
        }
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
#[ignore]
fn gpu_child() {
    let mode = std::env::var("GPU_TEST_MODE").unwrap();
    let reject = mode == "reject";
    let slow = mode == "timeout";
    let combined = mode.starts_with("combined");
    let split = mode == "split-reports";
    let window = mode.contains("window");
    let traces = mode != "profiles-only";
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let url = format!("http://{}", server.server_addr());
    std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", &url);
    std::env::set_var("MARS_PROFILE_URL", &url);
    std::env::set_var("MARS_PROFILE_SYMBOLIZATION", "functions");
    std::env::set_var("MARS_PROFILE_TIMEOUT_MS", "2000");
    std::env::set_var("MARS_SIGNALS", "traces,profiles");
    let capture = std::env::var_os("GPU_TEST_CAPTURE_DIR").map(std::path::PathBuf::from);
    if let Some(dir) = &capture {
        std::fs::create_dir_all(dir).unwrap();
    }
    let (stop, stopping) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut profiles = Vec::new();
        let mut cpu_samples = 0;
        let mut instance = None;
        while stopping.try_recv().is_err() {
            let Some(mut request) = server.recv_timeout(Duration::from_millis(10)).unwrap() else {
                continue;
            };
            let gpu = request.url().contains("PusherService");
            if gpu {
                let mut compressed = Vec::new();
                request.as_reader().read_to_end(&mut compressed).unwrap();
                if let Some(dir) = &capture {
                    std::fs::write(dir.join(format!("{}.pb.gz", profiles.len())), &compressed)
                        .unwrap();
                }
                let mut bytes = Vec::new();
                libflate::gzip::Decoder::new(compressed.as_slice())
                    .unwrap()
                    .read_to_end(&mut bytes)
                    .unwrap();
                let push = PushRequest::decode(bytes.as_slice()).unwrap();
                if let Some(dir) = &capture {
                    // Recreate the old wire identity as a negative control for
                    // real-backend compaction checks (unique upload IDs retained).
                    let mut control = push.clone();
                    for series in &mut control.series {
                        series.labels.retain(|l| l.name != "service_instance_id");
                        for label in &mut series.labels {
                            if label.name == "service_name" {
                                label.value.push_str("-control");
                            }
                        }
                        for sample in &mut series.samples {
                            let mut profile =
                                Profile::decode(sample.raw_profile.as_slice()).unwrap();
                            profile.time_nanos = profile.time_nanos / 1_000_000_000 * 1_000_000_000;
                            profile.duration_nanos = 1_000_000_000;
                            sample.raw_profile = profile.encode_to_vec();
                        }
                    }
                    let mut gzip = libflate::gzip::Encoder::new(Vec::new()).unwrap();
                    gzip.write_all(&control.encode_to_vec()).unwrap();
                    std::fs::write(
                        dir.join(format!("{}.control.pb.gz", profiles.len())),
                        gzip.finish().into_result().unwrap(),
                    )
                    .unwrap();
                }
                for series in push.series {
                    let is_gpu = series
                        .labels
                        .iter()
                        .any(|label| label.name == "__name__" && label.value == "gpu_kernel");
                    assert!(is_gpu || combined);
                    if is_gpu {
                        let id = &series
                            .labels
                            .iter()
                            .find(|l| l.name == "service_instance_id")
                            .unwrap()
                            .value;
                        assert_eq!(id.len(), 32);
                        assert_eq!(instance.get_or_insert_with(|| id.clone()), id);
                    } else {
                        assert!(!series
                            .labels
                            .iter()
                            .any(|l| l.name == "service_instance_id"));
                    }
                    for sample in series.samples {
                        let profile = Profile::decode(sample.raw_profile.as_slice()).unwrap();
                        if is_gpu {
                            profiles.push(profile);
                        } else {
                            cpu_samples += profile.sample.len();
                        }
                    }
                }
                if slow {
                    std::thread::sleep(Duration::from_millis(300));
                }
            }
            request
                .respond(tiny_http::Response::empty(if gpu && reject {
                    503
                } else {
                    200
                }))
                .unwrap();
        }
        (profiles, cpu_samples)
    });
    gst::init().unwrap();
    let sources = if combined {
        gstoteltracer::ProfileSources::CpuAndGpu
    } else {
        gstoteltracer::ProfileSources::Gpu
    };
    let adapter_path = std::env::var("MARS_GPU_ADAPTER").unwrap();
    if mode.starts_with("adapter-") {
        std::env::set_var("MARS_GPU_ADAPTER", "/invalid-overridden-adapter.so");
        // GPU-only collection must not validate CPU-only environment settings.
        std::env::set_var("MARS_PROFILE_HZ", "invalid-unused-cpu-rate");
        std::env::set_var("MARS_PROFILE_SIGNAL", "invalid-unused-cpu-signal");
        std::env::set_var("MARS_PROFILE_SYMBOLIZATION", "invalid-unused-cpu-mode");
    }
    let tracer: gst::Tracer = if window && mode != "window-params" {
        glib::Object::builder::<gstoteltracer::InstrumentTracer>()
            .property("profile-sources", sources)
            .property("profile-gpu-window-ms", 300u32)
            .property("profile-gpu-period-ms", 1000u32)
            .build()
            .upcast()
    } else if mode == "window-params" {
        glib::Object::builder::<gstoteltracer::InstrumentTracer>()
            .property("params", "signals=\"traces,profiles\",profile-sources=gpu,profile-gpu-window-ms=300,profile-gpu-period-ms=1000")
            .build().upcast()
    } else if mode == "adapter-property" {
        glib::Object::builder::<gstoteltracer::InstrumentTracer>()
            .property("profile-sources", sources)
            .property("profile-gpu-adapter", &adapter_path)
            .build()
            .upcast()
    } else if mode == "adapter-params" {
        glib::Object::builder::<gstoteltracer::InstrumentTracer>()
            .property("profile-gpu-adapter", "/invalid-overridden-property.so")
            .property("params", format!("signals=\"traces,profiles\",profile-sources=gpu,profile-gpu-adapter=\"{adapter_path}\""))
            .build().upcast()
    } else if mode == "otel" {
        glib::Object::builder::<gstoteltracer::TelemetryTracer>()
            .property("profile-sources", sources)
            .build()
            .upcast()
    } else if mode.ends_with("params") {
        glib::Object::builder::<gstoteltracer::InstrumentTracer>()
            .property(
                "params",
                format!(
                    "signals=\"traces,profiles\",profile-sources=\"{}\"",
                    if combined { "cpu,gpu" } else { "gpu" }
                ),
            )
            .build()
            .upcast()
    } else if !traces {
        glib::Object::builder::<gstoteltracer::ProfilesTracer>()
            .property("params", "profile-sources=gpu")
            .build()
            .upcast()
    } else {
        glib::Object::builder::<gstoteltracer::InstrumentTracer>()
            .property("profile-sources", sources)
            .build()
            .upcast()
    };
    assert_eq!(
        tracer.property::<gstoteltracer::ProfileSources>("profile-sources"),
        sources
    );
    if mode == "legacy-window" {
        assert!(gstoteltracer::stats().is_err());
        stop.send(()).unwrap();
        worker.join().unwrap();
        return;
    }
    gstoteltracer::stats().expect("tracer initialization failed");
    let path = std::ffi::CString::new(adapter_path).unwrap();
    let launch: unsafe extern "C" fn() = unsafe {
        let handle = libc::dlopen(path.as_ptr(), libc::RTLD_NOW);
        assert!(!handle.is_null());
        let symbol = libc::dlsym(handle, c"mars_gpu_mock_launch_v1".as_ptr());
        assert!(!symbol.is_null());
        if split {
            let symbol = libc::dlsym(handle, c"mars_gpu_mock_split_v1".as_ptr());
            assert!(!symbol.is_null());
            let second = std::env::var("GPU_TEST_SUBMISSION_SECOND")
                .ok()
                .map(|v| v.parse().unwrap())
                .unwrap_or(0);
            std::mem::transmute::<*mut libc::c_void, unsafe extern "C" fn(u64)>(symbol)(second);
        }
        std::mem::transmute(symbol)
    };
    let pipeline = gst::parse::launch("fakesrc num-buffers=2 ! identity name=first ! queue ! identity name=second ! fakesink sync=false")
        .unwrap().downcast::<gst::Pipeline>().unwrap();
    let expected = std::sync::Arc::new(std::sync::Mutex::new(HashSet::new()));
    for name in ["first", "second"] {
        let expected = expected.clone();
        pipeline
            .by_name(name)
            .unwrap()
            .connect("handoff", false, move |_| {
                let span = gstoteltracer::current_span_context();
                assert_eq!(span.is_some(), traces);
                expected.lock().unwrap().insert((
                    name.to_owned(),
                    span.map(|s| s.span_id().to_string()).unwrap_or_default(),
                ));
                unsafe { launch() };
                if combined {
                    let start = std::time::Instant::now();
                    while start.elapsed() < Duration::from_millis(40) {
                        std::hint::black_box((0u64..1000).sum::<u64>());
                    }
                }
                None
            });
    }
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
    if window {
        // The mock deliberately holds all kernel records until flush: preserve
        // owners after leaving their host scopes and closing the capture window.
        let capture_stats: unsafe extern "C" fn(*mut [u64; 7]) -> i32 = unsafe {
            let handle = libc::dlopen(path.as_ptr(), libc::RTLD_NOW);
            let symbol = libc::dlsym(handle, c"mars_gpu_capture_stats_v1".as_ptr());
            assert!(!symbol.is_null());
            std::mem::transmute(symbol)
        };
        let until = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let mut stats = [0; 7];
            assert_eq!(unsafe { capture_stats(&mut stats) }, 0);
            if stats[5] == 0 {
                break;
            }
            assert!(std::time::Instant::now() < until, "window did not close");
            std::thread::sleep(Duration::from_millis(2));
        }
        for _ in 0..3 {
            unsafe { launch() };
        }
        gstoteltracer::flush(Duration::from_secs(5)).unwrap();
        let gpu = gstoteltracer::gpu_profile_stats();
        assert_eq!(
            gpu.kernels, 4,
            "late completions must survive a closed window"
        );
        assert_eq!(gpu.capture_launches, 4);
        assert_eq!(gpu.capture_skipped_launches, 3);
        assert!(gpu.capture_active_ns < gpu.capture_elapsed_ns);
        assert_eq!(gpu.capture_failures, 0);
    }
    if split {
        for _ in 0..4 {
            gstoteltracer::flush(Duration::from_secs(5)).unwrap();
        }
    }
    if slow {
        let start = std::time::Instant::now();
        assert_eq!(
            gstoteltracer::flush_report(Duration::from_millis(20)).profiles,
            Some(Err(gstoteltracer::FlushError::Timeout))
        );
        assert!(start.elapsed() < Duration::from_millis(250));
    }
    let report = gstoteltracer::shutdown_report(Duration::from_secs(5));
    assert_eq!(
        report.profiles,
        Some(if reject {
            Err(gstoteltracer::FlushError::ExportFailed)
        } else {
            Ok(())
        })
    );
    assert_eq!(report.profile_delivery_confirmed, !reject);
    let gpu = gstoteltracer::gpu_profile_stats();
    assert_eq!(gpu.kernels, 4);
    assert_eq!(gpu.attributed_kernels, 4);
    assert_eq!(gpu.correlated_kernels, if traces { 4 } else { 0 });
    assert_eq!(gpu.accepted_kernels + gpu.failed_kernels, 4);
    assert_eq!(
        gpu.pending_kernels + gpu.pending_uploads + gpu.pending_contexts,
        0
    );
    let cpu = gstoteltracer::profile_stats();
    assert_eq!(cpu.samples > 0, combined);
    assert_eq!(cpu.samples, cpu.accepted_samples);
    assert_eq!(cpu.active_threads, 0);
    assert_eq!(
        cpu.context_descriptions, 0,
        "both workers released element leases"
    );
    assert!(cpu.context_retired >= 5);
    stop.send(()).unwrap();
    let (profiles, cpu_samples) = worker.join().unwrap();
    assert_eq!(cpu_samples > 0, combined);
    let mut actual = HashSet::new();
    let mut nanos = 0;
    if split {
        assert_eq!(profiles.len(), 4);
        assert_eq!(
            profiles
                .iter()
                .map(|p| p.time_nanos)
                .collect::<HashSet<_>>()
                .len(),
            4
        );
        let bucket = profiles[0].time_nanos / 1_000_000_000;
        for profile in &profiles {
            assert_eq!(profile.time_nanos / 1_000_000_000, bucket);
            assert_eq!(
                profile.time_nanos + profile.duration_nanos,
                (bucket + 1) * 1_000_000_000
            );
        }
        println!("BUCKET {bucket}");
    }
    for profile in profiles {
        for sample in profile.sample {
            assert_eq!(sample.location_id.len(), 2);
            let names: Vec<_> = sample
                .location_id
                .iter()
                .map(|id| {
                    let loc = profile.location.iter().find(|l| l.id == *id).unwrap();
                    let function = profile
                        .function
                        .iter()
                        .find(|f| f.id == loc.line[0].function_id)
                        .unwrap();
                    profile.string_table[function.name as usize].as_str()
                })
                .collect();
            assert_eq!(names[0], "same_kernel");
            let element = if names[1].contains("first [identity]") {
                "first"
            } else {
                assert!(names[1].contains("second [identity]"));
                "second"
            };
            let span = sample
                .label
                .iter()
                .find(|l| profile.string_table[l.key as usize] == "span_id");
            assert_eq!(span.is_some(), traces);
            actual.insert((
                element.to_owned(),
                span.map(|s| profile.string_table[s.str as usize].clone())
                    .unwrap_or_default(),
            ));
            if split {
                let trace = sample
                    .label
                    .iter()
                    .find(|l| profile.string_table[l.key as usize] == "trace_id")
                    .unwrap();
                println!(
                    "SAMPLE element={element} span={} trace={} nanos={}",
                    profile.string_table[span.unwrap().str as usize],
                    profile.string_table[trace.str as usize],
                    sample.value[0]
                );
            }
            nanos += sample.value[0];
        }
    }
    assert_eq!(actual, *expected.lock().unwrap());
    assert_eq!(nanos, 40_000_000);
}
