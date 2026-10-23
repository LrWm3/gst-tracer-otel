//! Small trace/profile demo for bench/correlation/check.py. Deliberately long
//! CPU work gives each selected push enough samples for a useful span profile.
use gstreamer::{self as gst, prelude::*};
use std::time::Duration;

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
    let _tracer = glib::Object::new::<gstoteltracer::TelemetryTracer>();
    let pipeline = gst::parse::launch(
        "fakesrc num-buffers=4 ! identity name=light ! queue name=handoff ! \
         identity name=heavy ! fakesink name=output sync=false",
    )
    .unwrap()
    .downcast::<gst::Pipeline>()
    .unwrap();
    for (name, milliseconds) in [("light", 100), ("heavy", 250)] {
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
                simulated_cpu_work(milliseconds);
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
    let traces = gstoteltracer::shutdown(Duration::from_secs(20)).unwrap();
    let profiles = gstoteltracer::profile_stats();
    assert_eq!(traces.started_spans, 16);
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
    println!("TRACE {traces:?}");
    println!("PROFILE {profiles:?}");
}
