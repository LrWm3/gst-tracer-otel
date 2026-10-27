//! Experimental GPU-only attribution demo. Build native/gpu_workload.c on the
//! CUDA host, and set MARS_GPU_WORKLOAD to that shared library's absolute path.
#[cfg(target_os = "linux")]
fn main() {
    use gstreamer::{self as gst, prelude::*};
    use std::{
        ffi::CString,
        time::{Duration, Instant},
    };
    let path =
        CString::new(std::env::var("MARS_GPU_WORKLOAD").expect("MARS_GPU_WORKLOAD")).unwrap();
    // SAFETY: the explicitly selected demo library implements these three C ABI
    // entry points. It remains loaded until process exit, including callbacks.
    let (initialize, launch, finish) = unsafe {
        let library = libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        assert!(
            !library.is_null(),
            "could not load GPU workload: {:?}",
            std::ffi::CStr::from_ptr(libc::dlerror())
        );
        let init = libc::dlsym(library, c"mars_workload_initialize".as_ptr());
        let launch = libc::dlsym(library, c"mars_workload_launch".as_ptr());
        let finish = libc::dlsym(library, c"mars_workload_finish".as_ptr());
        assert!(!init.is_null() && !launch.is_null() && !finish.is_null());
        (
            std::mem::transmute::<*mut libc::c_void, unsafe extern "C" fn() -> i32>(init),
            std::mem::transmute::<*mut libc::c_void, unsafe extern "C" fn(u32) -> i32>(launch),
            std::mem::transmute::<*mut libc::c_void, unsafe extern "C" fn() -> i32>(finish),
        )
    };
    gst::init().unwrap();
    // The demo takes the same source selector as the tracer's profile-sources property.
    let sources = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "gpu".into())
        .parse::<gstoteltracer::ProfileSources>()
        .expect("usage: gpu_correlation [gpu|cpu,gpu]");
    assert_ne!(
        sources,
        gstoteltracer::ProfileSources::Cpu,
        "this demo requires GPU profiles"
    );
    let _tracer = glib::Object::builder::<gstoteltracer::InstrumentTracer>()
        .property("signals", "traces,profiles")
        .property("profile-sources", sources)
        .build();
    gstoteltracer::stats()
        .expect("tracer initialization failed; enable GST_DEBUG=mars-otel:5 for details");
    let combined = sources == gstoteltracer::ProfileSources::CpuAndGpu;
    assert_eq!(unsafe { initialize() }, 0, "CUDA workload initialization");
    let buffers = std::env::var("MARS_GPU_DEMO_BUFFERS")
        .unwrap_or_else(|_| "8".into())
        .parse::<u64>()
        .unwrap();
    assert!((1..=1000).contains(&buffers));
    let pipeline = gst::parse::launch(&format!(
        "fakesrc num-buffers={buffers} ! identity name=preprocess ! queue ! identity name=inference ! fakesink sync=false"
    )).unwrap().downcast::<gst::Pipeline>().unwrap();
    for (index, name) in ["preprocess", "inference"].into_iter().enumerate() {
        pipeline
            .by_name(name)
            .unwrap()
            .connect("handoff", false, move |_| {
                let span = gstoteltracer::current_span_context().expect("sampled span");
                let start = Instant::now();
                assert_eq!(unsafe { launch(index as u32) }, 0, "CUDA launch");
                if combined {
                    let cpu_start = Instant::now();
                    while cpu_start.elapsed() < Duration::from_millis(40) {
                        std::hint::black_box((0u64..1000).sum::<u64>());
                    }
                }
                println!(
                    "SPAN element={name} trace_id={} span_id={} submit_us={}",
                    span.trace_id(),
                    span.span_id(),
                    start.elapsed().as_micros()
                );
                None
            });
    }
    let start = Instant::now();
    pipeline.set_state(gst::State::Playing).unwrap();
    let message = pipeline
        .bus()
        .unwrap()
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(60),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .expect("pipeline timeout");
    if let gst::MessageView::Error(error) = message.view() {
        panic!("{}", error.error());
    }
    drop(message);
    pipeline.set_state(gst::State::Null).unwrap();
    drop(pipeline);
    println!("SUBMISSION_DONE elapsed_ms={}", start.elapsed().as_millis());
    // Application-owned synchronization after spans and elements have ended.
    // The profiler itself never waits for or synchronizes GPU execution.
    assert_eq!(unsafe { finish() }, 0, "CUDA completion");
    println!("DEVICE_DONE elapsed_ms={}", start.elapsed().as_millis());
    let start = Instant::now();
    let report = gstoteltracer::shutdown_report(Duration::from_secs(20));
    println!(
        "DRAIN {report:?} elapsed_ms={}",
        start.elapsed().as_millis()
    );
    let gpu = gstoteltracer::gpu_profile_stats();
    let cpu = gstoteltracer::profile_stats();
    println!("GPU {gpu:?}");
    println!("CPU {cpu:?}");
    report.result().unwrap();
    assert_eq!(gpu.kernels, buffers * 6);
    assert_eq!(gpu.attributed_kernels, gpu.kernels);
    assert_eq!(gpu.correlated_kernels, gpu.kernels);
    assert_eq!(gpu.accepted_kernels, gpu.kernels);
    assert_eq!(
        gpu.failed_kernels
            + gpu.dropped_events
            + gpu.dropped_records
            + gpu.context_capacity_rejections
            + gpu.unmatched_kernels
            + gpu.invalid_timestamps
            + gpu.dropped_processing
            + gpu.dropped_upload
            + gpu.pending_kernels
            + gpu.pending_uploads
            + gpu.pending_contexts,
        0
    );
    assert_eq!(cpu.samples > 0, combined);
    assert_eq!(cpu.samples, cpu.accepted_samples);
    assert_eq!(cpu.active_threads + cpu.context_descriptions, 0);
    assert!(cpu.context_retired >= 5);
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("The experimental CUPTI adapter requires Linux");
    std::process::exit(1);
}
