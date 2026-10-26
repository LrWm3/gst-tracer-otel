//! Real wire/lifecycle test using a fake adapter, requiring no CUDA installation.
#![cfg(all(feature = "gpu-profiles", target_os = "linux"))]
use gstreamer::{self as gst, prelude::*};
use prost::Message;
use pyroscope::encode::gen::{google::Profile, push::PushRequest};
use std::{collections::HashSet, io::Read, process::Command, time::Duration};

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
            "-o",
        ])
        .arg(&path)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .unwrap();
    assert!(status.success());
    for mode in ["success", "reject", "timeout", "combined"] {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "gpu_child", "--ignored", "--nocapture"])
            .env_clear()
            .env("MARS_GPU_ADAPTER", &path)
            .env("GPU_TEST_MODE", mode)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{mode}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
#[ignore]
fn gpu_child() {
    let mode = std::env::var("GPU_TEST_MODE").unwrap();
    let reject = mode == "reject";
    let slow = mode == "timeout";
    let combined = mode == "combined";
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let url = format!("http://{}", server.server_addr());
    std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", &url);
    std::env::set_var("MARS_PROFILE_URL", &url);
    std::env::set_var(
        "MARS_PROFILE_SOURCES",
        if combined { "cpu,gpu" } else { "gpu" },
    );
    std::env::set_var("MARS_PROFILE_SYMBOLIZATION", "functions");
    std::env::set_var("MARS_PROFILE_TIMEOUT_MS", "2000");
    std::env::set_var("MARS_SIGNALS", "traces,profiles");
    let (stop, stopping) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut profiles = Vec::new();
        let mut cpu_samples = 0;
        while stopping.try_recv().is_err() {
            let Some(mut request) = server.recv_timeout(Duration::from_millis(10)).unwrap() else {
                continue;
            };
            let gpu = request.url().contains("PusherService");
            if gpu {
                let mut compressed = Vec::new();
                request.as_reader().read_to_end(&mut compressed).unwrap();
                let mut bytes = Vec::new();
                libflate::gzip::Decoder::new(compressed.as_slice())
                    .unwrap()
                    .read_to_end(&mut bytes)
                    .unwrap();
                for series in PushRequest::decode(bytes.as_slice()).unwrap().series {
                    let is_gpu = series
                        .labels
                        .iter()
                        .any(|label| label.name == "__name__" && label.value == "gpu_kernel");
                    assert!(is_gpu || combined);
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
    let _tracer = glib::Object::new::<gstoteltracer::InstrumentTracer>();
    gstoteltracer::stats().expect("tracer initialization failed");
    let path = std::ffi::CString::new(std::env::var("MARS_GPU_ADAPTER").unwrap()).unwrap();
    let launch: unsafe extern "C" fn() = unsafe {
        let handle = libc::dlopen(path.as_ptr(), libc::RTLD_NOW);
        assert!(!handle.is_null());
        let symbol = libc::dlsym(handle, c"mars_gpu_mock_launch_v1".as_ptr());
        assert!(!symbol.is_null());
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
                let span = gstoteltracer::current_span_context().unwrap();
                expected
                    .lock()
                    .unwrap()
                    .insert((name.to_owned(), span.span_id().to_string()));
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
    assert_eq!(gpu.correlated_kernels, 4);
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
                .find(|l| profile.string_table[l.key as usize] == "span_id")
                .unwrap();
            actual.insert((
                element.to_owned(),
                profile.string_table[span.str as usize].clone(),
            ));
            nanos += sample.value[0];
        }
    }
    assert_eq!(actual, *expected.lock().unwrap());
    assert_eq!(nanos, 40_000_000);
}
