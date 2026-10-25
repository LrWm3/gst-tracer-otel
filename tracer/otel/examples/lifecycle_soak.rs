//! Long-running lifecycle exercise, driven by bench/lifecycle_soak.py.
//! No external processor or production backend is needed.
use gstreamer::{self as gst, prelude::*};
use std::{
    ffi::CString,
    sync::{
        atomic::{AtomicU64, Ordering::Relaxed},
        Arc, LazyLock,
    },
    time::{Duration, Instant},
};

static LOG: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new("mars-soak-work", gst::DebugColorFlags::empty(), None)
});

fn busy(duration: Duration) {
    let start = Instant::now();
    let mut value = 17u64;
    while start.elapsed() < duration {
        value = std::hint::black_box(value.wrapping_mul(6364136223846793005).wrapping_add(1));
    }
    std::hint::black_box(value);
}

fn cycle(index: u64, module: &CString) {
    let pipeline = gst::parse::launch(
        "queue name=queue ! identity name=work signal-handoffs=true ! fakesink sync=false async=false",
    ).unwrap().downcast::<gst::Pipeline>().unwrap();
    pipeline.set_property("name", format!("soak-{index}"));
    let count = Arc::new(AtomicU64::new(0));
    let counted = count.clone();
    pipeline
        .by_name("work")
        .unwrap()
        .connect("handoff", false, move |_| {
            busy(Duration::from_millis(2));
            counted.fetch_add(1, Relaxed);
            gst::info!(LOG, "processed soak buffer");
            None
        });
    // A fresh native producer thread pushes synchronously. Its sampler/TLS must
    // retire on return; GStreamer's own queue task can reuse a pool thread.
    let source = gst::Bin::with_name(&format!("producer-{index}"));
    source.set_element_flags(gst::ElementFlags::SOURCE);
    source.set_locked_state(true);
    pipeline.add(&source).unwrap();
    let pad = gst::Pad::builder(gst::PadDirection::Src)
        .name("src")
        .build();
    source.add_pad(&pad).unwrap();
    let sink = pipeline
        .by_name("queue")
        .unwrap()
        .static_pad("sink")
        .unwrap();
    pad.link(&sink).unwrap();
    pipeline.set_state(gst::State::Playing).unwrap();
    pad.set_active(true).unwrap();
    assert!(pad.push_event(gst::event::StreamStart::new(&format!("stream-{index}"))));
    assert!(pad.push_event(gst::event::Caps::new(
        &gst::Caps::builder("application/octet-stream").build()
    )));
    assert!(
        pad.push_event(gst::event::Segment::new(&gst::FormattedSegment::<
            gst::ClockTime,
        >::new()))
    );
    for item in 0..4 {
        let mut buffer = gst::Buffer::with_size(4).unwrap();
        buffer
            .get_mut()
            .unwrap()
            .set_pts(gst::ClockTime::from_mseconds(item));
        pad.push(buffer).unwrap();
        if item == 0 {
            // A constructor/destructor in this fixture does CPU work while the
            // loader lock is held. Sampling is already armed on this thread.
            unsafe {
                let handle = libc::dlopen(module.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
                assert!(!handle.is_null(), "fixture dlopen failed");
                assert_eq!(libc::dlclose(handle), 0);
            }
        }
    }
    assert!(pad.push_event(gst::event::Eos::new()));
    let message = pipeline
        .bus()
        .unwrap()
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(5),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .expect("pipeline stalled");
    assert_eq!(message.type_(), gst::MessageType::Eos, "{message:?}");
    assert_eq!(count.load(Relaxed), 4);
    pad.set_active(false).unwrap();
    pad.unlink(&sink).unwrap();
    pipeline.set_state(gst::State::Null).unwrap();
    let weak = pipeline.downgrade();
    let weak_source = source.downgrade();
    drop((message, pad, sink, pipeline, source));
    let until = Instant::now() + Duration::from_secs(1);
    while weak.upgrade().is_some() || weak_source.upgrade().is_some() {
        assert!(
            Instant::now() < until,
            "pipeline/source retained at cycle {index}"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn report(start: Instant, cycles: u64, phase: &str) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let value = |key: &str| {
        status
            .lines()
            .find(|line| line.starts_with(key))
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse::<u64>()
            .unwrap()
    };
    let rss = std::fs::read_to_string("/proc/self/smaps_rollup")
        .unwrap()
        .lines()
        .find(|line| line.starts_with("Rss:"))
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse::<u64>()
        .unwrap();
    let threads = std::fs::read_dir("/proc/self/task").unwrap().count();
    let fds = std::fs::read_dir("/proc/self/fd").unwrap().count();
    println!("STATE phase={phase} elapsed_ms={} cycles={cycles} rss_kb={rss} peak_kb={} threads={threads} fds={fds}", start.elapsed().as_millis(), value("VmHWM:").max(rss));
    println!("TRACE {:?}", gstoteltracer::stats().unwrap());
    println!("PROFILE {:?}", gstoteltracer::profile_stats());
    println!("METRIC {:?}", gstoteltracer::metric_stats().unwrap());
    println!("LOG {:?}", gstoteltracer::log_stats());
    use std::io::Write;
    std::io::stdout().flush().unwrap();
}

fn check_drain(report: &gstoteltracer::DrainReport) {
    let acceptable = |result: Result<(), gstoteltracer::FlushError>| {
        assert!(
            result.is_ok() || result == Err(gstoteltracer::FlushError::ExportFailed),
            "{result:?}"
        );
    };
    acceptable(report.traces.clone().map(|_| ()));
    acceptable(report.metrics.clone());
    acceptable(report.logs.clone());
    assert_eq!(report.profiles, Some(Ok(())));
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    let duration = Duration::from_secs(args[1].parse().unwrap());
    let module = CString::new(args[2].as_bytes()).unwrap();
    gst::init().unwrap();
    let _tracer: gstoteltracer::InstrumentTracer = glib::Object::new();
    let start = Instant::now();
    let mut cycles = 0;
    let mut next = Duration::ZERO;
    while start.elapsed() < duration {
        let module = module.clone();
        std::thread::spawn(move || cycle(cycles, &module))
            .join()
            .unwrap();
        cycles += 1;
        if start.elapsed() >= next {
            let drain = gstoteltracer::flush_report(Duration::from_secs(5));
            check_drain(&drain);
            let stats = gstoteltracer::stats().unwrap();
            let profile = gstoteltracer::profile_stats();
            assert_eq!(stats.pending_spans, 0);
            assert_eq!(
                stats.mismatched_hooks + stats.dropped_depth + stats.propagation_misses,
                0
            );
            assert_eq!(
                profile.context_capacity_rejections + profile.registration_failures,
                0
            );
            assert!(profile.context_descriptions < 64, "{profile:?}");
            report(start, cycles, "running");
            next = start.elapsed() + Duration::from_secs(10);
        }
        std::thread::sleep(Duration::from_millis(80));
    }
    // Allow a healthy recovery window and drain without creating more pipelines.
    for _ in 0..3 {
        std::thread::sleep(Duration::from_secs(2));
        check_drain(&gstoteltracer::flush_report(Duration::from_secs(5)));
        report(start, cycles, "settled");
    }
    let shutdown = Instant::now();
    let joins: Vec<_> = (0..4)
        .map(|_| std::thread::spawn(|| gstoteltracer::shutdown_report(Duration::from_secs(5))))
        .collect();
    for join in joins {
        check_drain(&join.join().unwrap());
    }
    assert!(shutdown.elapsed() < Duration::from_secs(6));
    for _ in 0..100 {
        check_drain(&gstoteltracer::shutdown_report(Duration::from_millis(100)));
    }
    let trace = gstoteltracer::stats().unwrap();
    let profile = gstoteltracer::profile_stats();
    assert_eq!(trace.pending_spans, 0);
    assert_eq!(profile.active_threads, 0);
    assert_eq!(profile.samples, profile.reported_samples);
    assert_eq!(profile.context_descriptions, 0);
    println!(
        "DONE cycles={cycles} shutdown_ms={}",
        shutdown.elapsed().as_millis()
    );
    report(start, cycles, "shutdown");
}
