//! Temporary native lifetime/RSS stress driver; see companion runner.
use gstreamer::{self as gst, prelude::*};
use std::sync::LazyLock;
use std::time::{Duration, Instant};
static LOG: LazyLock<gst::DebugCategory> = LazyLock::new(|| gst::DebugCategory::new("mars-profile-soak", gst::DebugColorFlags::empty(), None));
fn memory(phase: &str, elapsed: f64, cycles: usize) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let fields: Vec<_> = status.lines().filter(|l| l.starts_with("VmRSS:") || l.starts_with("VmHWM:") || l.starts_with("RssAnon:") || l.starts_with("Threads:")).collect();
    println!("MEM phase={phase} elapsed={elapsed:.3} cycles={cycles} {}", fields.join(" "));
}
fn pipeline(unique: usize, churn: bool) {
    let description = if churn {
        format!("pipeline name=churn_{unique} fakesrc num-buffers=1 ! identity ! fakesink sync=false async=false")
    } else {
        "pipeline name=steady videotestsrc name=source num-buffers=30 pattern=snow ! video/x-raw,format=RGBA,width=1920,height=1080 ! videoconvert name=convert ! video/x-raw,format=I420 ! fakesink name=sink sync=false async=false".into()
    };
    let pipeline = gst::parse::launch(&description).unwrap().downcast::<gst::Pipeline>().unwrap();
    if !churn {
        pipeline.by_name("sink").unwrap().static_pad("sink").unwrap().add_probe(gst::PadProbeType::BUFFER, |_, _| {
            gst::info!(LOG, "processed frame");
            gst::PadProbeReturn::Ok
        });
    }
    let weak = pipeline.downgrade();
    pipeline.set_state(gst::State::Playing).unwrap();
    let message = pipeline.bus().unwrap().timed_pop_filtered(gst::ClockTime::from_seconds(15), &[gst::MessageType::Eos, gst::MessageType::Error]).expect("no pipeline progress for 15s");
    if let gst::MessageView::Error(error) = message.view() { panic!("{} {:?}", error.error(), error.debug()); }
    pipeline.set_state(gst::State::Null).unwrap();
    drop(message);
    drop(pipeline);
    let deadline = Instant::now() + Duration::from_secs(1);
    while weak.upgrade().is_some() {
        assert!(Instant::now() < deadline, "pipeline retained for 1s after destruction");
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn main() {
    let args: Vec<_> = std::env::args().collect();
    let seconds: u64 = args[2].parse().unwrap();
    gst::init().unwrap();
    let tracer: Option<gst::Tracer> = match args[1].as_str() {
        "none" => None,
        "profiles" => Some(glib::Object::new::<gstoteltracer::ProfilesTracer>().upcast()),
        "otel" => Some(glib::Object::new::<gstoteltracer::TelemetryTracer>().upcast()),
        _ => panic!("invalid mode"),
    };
    let start = Instant::now();
    let mut cycles = 0;
    memory("start", 0., cycles);
    for phase in ["warm", "steady"] {
        let phase_start = Instant::now();
        while phase_start.elapsed() < Duration::from_secs(seconds) {
            pipeline(cycles, false);
            cycles += 1;
            memory(phase, start.elapsed().as_secs_f64(), cycles);
        }
        if phase == "warm" {
            for i in 0..1500 { pipeline(i, true); }
            memory("churn-1500", start.elapsed().as_secs_f64(), cycles);
        }
    }
    let shutdown = Instant::now();
    if tracer.is_some() { println!("TRACE {:?}", gstoteltracer::shutdown(Duration::from_secs(20)).unwrap()); }
    println!("SHUTDOWN elapsed={:.6}", shutdown.elapsed().as_secs_f64());
    let profile = gstoteltracer::profile_stats();
    println!("PROFILE {profile:?}");
    assert_eq!(profile.active_threads, 0);
    assert_eq!(profile.samples, profile.reported_samples);
    assert_eq!(profile.dropped_full + profile.dropped_processing + profile.dropped_upload + profile.registration_failures, 0);
    memory("shutdown", start.elapsed().as_secs_f64(), cycles);
    std::thread::sleep(Duration::from_secs(10));
    memory("idle", start.elapsed().as_secs_f64(), cycles);
    // Diagnostic only: distinguish reusable libc arenas from live allocations.
    unsafe { libc::malloc_trim(0); }
    memory("trim", start.elapsed().as_secs_f64(), cycles);
}
