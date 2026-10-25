//! Small four-signal demo for bench/correlation/check.py. Deliberately long
//! CPU work gives each selected push enough samples for a useful span profile.
use gstreamer::{self as gst, prelude::*};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, LazyLock,
    },
    time::{Duration, Instant},
};

static LOG: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new("mars-correlation", gst::DebugColorFlags::empty(), None)
});

fn thread_cpu() -> u64 {
    let mut value = std::mem::MaybeUninit::<libc::timespec>::uninit();
    // SAFETY: clock_gettime initializes value on success, checked below.
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, value.as_mut_ptr()) },
        0
    );
    let value = unsafe { value.assume_init() };
    value.tv_sec as u64 * 1_000_000_000 + value.tv_nsec as u64
}

#[inline(never)]
fn simulated_cpu_work(milliseconds: u64) {
    let until = thread_cpu() + milliseconds * 1_000_000;
    let mut value = 1u64;
    while thread_cpu() < until {
        for _ in 0..10000 {
            value = std::hint::black_box(value.wrapping_mul(6364136223846793005).wrapping_add(1));
        }
    }
}

fn main() {
    gst::init().unwrap();
    let showcase = std::env::var("MARS_DEMO_SHOWCASE").as_deref() == Ok("1");
    let buffers = if showcase { 48 } else { 4 };
    let _tracer = glib::Object::new::<gstoteltracer::InstrumentTracer>();
    gst::info!(LOG, "demo started outside a sampled span");
    let pipeline = gst::parse::launch(&format!(
        "fakesrc num-buffers={buffers} ! identity name=light ! queue name=handoff ! \
         identity name=heavy ! fakesink name=output sync=false"
    ))
    .unwrap()
    .downcast::<gst::Pipeline>()
    .unwrap();
    let slow_finished = Arc::new(AtomicBool::new(false));
    for (name, milliseconds) in [("light", 100), ("heavy", 250)] {
        let sequence = AtomicU64::new(0);
        let slow_finished = slow_finished.clone();
        pipeline
            .by_name(name)
            .unwrap()
            .connect("handoff", false, move |_| {
                let span = gstoteltracer::current_span_context().expect("sampled push context");
                println!(
                    "SPAN element={name} trace_id={} span_id={}",
                    span.trace_id(),
                    span.span_id()
                );
                let frame = sequence.fetch_add(1, Ordering::Relaxed);
                let milliseconds = if showcase {
                    match name {
                        "light" => 20,
                        _ if frame == 24 => 500,
                        _ => 100,
                    }
                } else {
                    milliseconds
                };
                simulated_cpu_work(milliseconds);
                if showcase && name == "heavy" && frame == 24 {
                    gst::warning!(
                        LOG,
                        "slow frame: element={name} frame={} cpu_work_ms={milliseconds}",
                        frame + 1
                    );
                    slow_finished.store(true, Ordering::Release);
                } else {
                    gst::info!(
                        LOG,
                        "processed frame: element={name} frame={} cpu_work_ms={milliseconds}",
                        frame + 1
                    );
                }
                if showcase && name == "light" {
                    if frame == 25 {
                        // Keep the slow measurement in the bounded recent-exemplar
                        // slot for an export interval before sending another frame.
                        let deadline = Instant::now() + Duration::from_secs(5);
                        while !slow_finished.load(Ordering::Acquire) {
                            assert!(Instant::now() < deadline, "slow demo frame stalled");
                            std::thread::sleep(Duration::from_millis(5));
                        }
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
                None
            });
    }
    pipeline.set_state(gst::State::Playing).unwrap();
    let message = pipeline
        .bus()
        .unwrap()
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(30),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .expect("pipeline timed out");
    if let gst::MessageView::Error(error) = message.view() {
        panic!("{} {:?}", error.error(), error.debug());
    }
    pipeline.set_state(gst::State::Null).unwrap();
    gst::info!(LOG, "demo completed outside a sampled span");
    let traces = gstoteltracer::shutdown(Duration::from_secs(20)).unwrap();
    let profiles = gstoteltracer::profile_stats();
    assert_eq!(profiles.accepted_samples, profiles.reported_samples);
    assert_eq!(
        profiles.failed_samples
            + profiles.failed_exports
            + profiles.pending_samples
            + profiles.pending_profiles,
        0
    );
    assert_eq!(traces.started_spans, buffers * 4);
    assert_eq!(traces.exported_spans, traces.started_spans);
    assert_eq!(
        traces.pending_spans + traces.dropped_queue_full + traces.failed_export_spans,
        0
    );
    assert!(profiles.correlated_samples > 0);
    assert_eq!(profiles.samples, profiles.reported_samples);
    assert_eq!(
        profiles.dropped_full + profiles.dropped_processing + profiles.dropped_upload,
        0
    );
    assert_eq!(profiles.active_threads + profiles.registration_failures, 0);
    let metrics = gstoteltracer::metric_stats().unwrap();
    assert_eq!(metrics.recorded_pushes, buffers * 4);
    assert_eq!(metrics.failed_exports + metrics.dropped_capacity, 0);
    let logs = gstoteltracer::log_stats();
    assert_eq!(logs.exported, buffers * 2 + 2);
    assert_eq!(
        logs.pending + logs.failed + logs.dropped_queue_full + logs.dropped_closed,
        0
    );
    println!("TRACE {traces:?}");
    println!("PROFILE {profiles:?}");
    println!("METRICS {metrics:?}");
    println!("LOGS {logs:?}");
}
