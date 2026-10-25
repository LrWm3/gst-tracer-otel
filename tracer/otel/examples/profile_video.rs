//! Linux benchmark driver for bench/profile_video.py. All modes use one binary
//! and instantiate the same tracer types as the plugin factories. Explicit
//! shutdown includes final profile processing instead of relying on a sleep.
use gstreamer::{self as gst, prelude::*};
use std::sync::{
    atomic::{AtomicU64, Ordering::Relaxed},
    Arc, LazyLock,
};
use std::time::{Duration, Instant};

static LOG: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new("mars-profile-bench", gst::DebugColorFlags::empty(), None)
});

fn cpu(clock: libc::clockid_t) -> u64 {
    let mut value = std::mem::MaybeUninit::<libc::timespec>::uninit();
    // SAFETY: clock_gettime writes a timespec on success, checked before reading.
    assert_eq!(unsafe { libc::clock_gettime(clock, value.as_mut_ptr()) }, 0);
    let value = unsafe { value.assume_init() };
    value.tv_sec as u64 * 1_000_000_000 + value.tv_nsec as u64
}

fn main() {
    let total_start = cpu(libc::CLOCK_PROCESS_CPUTIME_ID);
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(
        args.len(),
        3,
        "profile_video none|profiles|otel|instrument FRAMES"
    );
    let frames: u64 = args[2].parse().unwrap();
    assert!(frames > 0);
    gst::init().unwrap();
    let tracer: Option<gst::Tracer> = match args[1].as_str() {
        "none" => None,
        "profiles" => Some(glib::Object::new::<gstoteltracer::ProfilesTracer>().upcast()),
        "otel" => Some(glib::Object::new::<gstoteltracer::TelemetryTracer>().upcast()),
        "instrument" => Some(glib::Object::new::<gstoteltracer::InstrumentTracer>().upcast()),
        _ => panic!("unknown mode"),
    };
    let pipeline = gst::parse::launch(&format!(
        "videotestsrc name=source0 num-buffers={frames} pattern=snow ! \
         video/x-raw,format=RGBA,width=1920,height=1080 ! videoconvert ! \
         video/x-raw,format=I420 ! fakesink name=sink0 sync=false async=false"
    ))
    .unwrap()
    .downcast::<gst::Pipeline>()
    .unwrap();
    let first_cpu = Arc::new(AtomicU64::new(0));
    let source_cpu = Arc::new(AtomicU64::new(0));
    let received = Arc::new(AtomicU64::new(0));
    let first = first_cpu.clone();
    let measured = source_cpu.clone();
    pipeline
        .by_name("source0")
        .unwrap()
        .static_pad("src")
        .unwrap()
        .add_probe(
            gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM,
            move |_, info| {
                if info.buffer().is_some() && first.load(Relaxed) == 0 {
                    first.store(cpu(libc::CLOCK_THREAD_CPUTIME_ID), Relaxed);
                }
                if info
                    .event()
                    .is_some_and(|e| e.type_() == gst::EventType::Eos)
                {
                    measured.store(
                        cpu(libc::CLOCK_THREAD_CPUTIME_ID) - first.load(Relaxed),
                        Relaxed,
                    );
                }
                gst::PadProbeReturn::Ok
            },
        );
    let count = received.clone();
    pipeline
        .by_name("sink0")
        .unwrap()
        .static_pad("sink")
        .unwrap()
        .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            let frame = count.fetch_add(1, Relaxed) + 1;
            // All modes execute this call; GST_DEBUG enables it only in log
            // modes. Sink probes run inside the traced downstream push.
            gst::info!(LOG, "processed frame {frame}");
            gst::PadProbeReturn::Ok
        });
    let process_start = cpu(libc::CLOCK_PROCESS_CPUTIME_ID);
    let wall_start = Instant::now();
    pipeline.set_state(gst::State::Playing).unwrap();
    let message = pipeline
        .bus()
        .unwrap()
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(120),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .expect("pipeline timed out");
    let elapsed = wall_start.elapsed();
    if let gst::MessageView::Error(error) = message.view() {
        panic!("{} {:?}", error.error(), error.debug());
    }
    assert_eq!(received.load(Relaxed), frames);
    let before_cleanup = std::fs::read_to_string("/proc/self/status").unwrap();
    let steady_rss = before_cleanup
        .lines()
        .find(|l| l.starts_with("VmRSS:"))
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap();
    if std::env::var_os("MARS_BENCH_SYNC_EOS").is_some() {
        // Let the benchmark scrape final live aggregates before NULL retires
        // metric series. The harness acknowledges over stdin, with a timeout.
        use std::io::{Read, Write};
        println!("EOS");
        std::io::stdout().flush().unwrap();
        std::io::stdin().read_exact(&mut [0]).unwrap();
    }
    let shutdown_start = Instant::now();
    pipeline.set_state(gst::State::Null).unwrap();
    let stats = if tracer.is_some() {
        gstoteltracer::shutdown(Duration::from_secs(20)).unwrap()
    } else {
        gstoteltracer::ExportStats::default()
    };
    let shutdown = shutdown_start.elapsed();
    let process_end = cpu(libc::CLOCK_PROCESS_CPUTIME_ID);
    let profile = gstoteltracer::profile_stats();
    let metrics = if tracer.is_some() {
        gstoteltracer::metric_stats().unwrap()
    } else {
        gstoteltracer::MetricStats::default()
    };
    assert_eq!(metrics.failed_exports, 0);
    assert_eq!(
        metrics.dropped_capacity + metrics.dropped_invalid + metrics.skipped_closed,
        0
    );
    assert_eq!(stats.pending_spans, 0);
    assert_eq!(stats.failed_export_spans, 0);
    assert_eq!(stats.dropped_queue_full, 0);
    assert_eq!(profile.active_threads, 0);
    assert_eq!(profile.samples, profile.reported_samples);
    assert_eq!(
        profile.dropped_full + profile.dropped_processing + profile.dropped_upload,
        0
    );
    assert_eq!(profile.registration_failures, 0);
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let peak = status
        .lines()
        .find(|l| l.starts_with("VmHWM:"))
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap();
    // Linux's per-CPU RSS accounting can briefly make VmHWM lag a prior RSS
    // read. Keep the highest observation instead of reporting peak < current.
    let peak = peak
        .parse::<u64>()
        .unwrap()
        .max(steady_rss.parse().unwrap());
    println!("RESULT frames={frames} elapsed_ns={} stream_cpu_ns={} process_cpu_ns={} total_cpu_ns={} shutdown_ns={} max_rss_kb={peak} steady_rss_kb={steady_rss}",
        elapsed.as_nanos(), source_cpu.load(Relaxed), process_end - process_start,
        process_end - total_start, shutdown.as_nanos());
    println!("TRACE {stats:?}");
    println!("PROFILE {profile:?}");
    println!("METRIC {metrics:?}");
    println!("LOG {:?}", gstoteltracer::log_stats());
}
