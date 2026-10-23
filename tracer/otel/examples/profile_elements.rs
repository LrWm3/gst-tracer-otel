//! Visual attribution review: two identity instances on different threads,
//! intentionally consuming 2 ms and 6 ms CPU per frame. Default: profiles only.
use gstreamer::{self as gst, prelude::*};
use std::time::Duration;

fn thread_cpu() -> u64 {
    let mut value = std::mem::MaybeUninit::<libc::timespec>::uninit();
    // SAFETY: checked clock_gettime initializes the output.
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, value.as_mut_ptr()) },
        0
    );
    let value = unsafe { value.assume_init() };
    value.tv_sec as u64 * 1_000_000_000 + value.tv_nsec as u64
}

#[inline(never)]
fn simulated_element_work(milliseconds: u64) {
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
    let frames: u32 = std::env::args().nth(1).map_or(1200, |n| n.parse().unwrap());
    let _tracer = glib::Object::new::<gstoteltracer::ProfilesTracer>();
    let pipeline = gst::parse::launch(&format!(
        "videotestsrc name=source num-buffers={frames} pattern=snow ! \
         video/x-raw,format=RGBA,width=640,height=360 ! videoconvert name=convert ! \
         video/x-raw,format=I420 ! identity name=light ! queue name=handoff ! \
         identity name=heavy ! fakesink name=output sync=false"
    ))
    .unwrap()
    .downcast::<gst::Pipeline>()
    .unwrap();
    for (name, cpu_ms) in [("light", 2), ("heavy", 6)] {
        pipeline
            .by_name(name)
            .unwrap()
            .connect("handoff", false, move |_| {
                simulated_element_work(cpu_ms);
                None
            });
    }
    pipeline.set_state(gst::State::Playing).unwrap();
    let message = pipeline
        .bus()
        .unwrap()
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(120),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .expect("pipeline timed out");
    if let gst::MessageView::Error(error) = message.view() {
        panic!("{} {:?}", error.error(), error.debug());
    }
    pipeline.set_state(gst::State::Null).unwrap();
    gstoteltracer::shutdown(Duration::from_secs(20)).unwrap();
    let stats = gstoteltracer::profile_stats();
    assert!(stats.samples > 0);
    assert_eq!(stats.samples, stats.reported_samples);
    assert_eq!(
        stats.dropped_full + stats.dropped_processing + stats.dropped_upload,
        0
    );
    assert_eq!(
        stats.registration_failures + stats.context_capacity_rejections,
        0
    );
    println!("PROFILE {stats:?}");
}
