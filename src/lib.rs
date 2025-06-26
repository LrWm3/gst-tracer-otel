use glib;
use glib::subclass::prelude::*;
use gst::prelude::*;
use gst::subclass::prelude::*;
use lazy_static::lazy_static;
use prometheus::{register_counter_vec, register_gauge_vec, CounterVec, GaugeVec};

// Define Prometheus metrics, all in nanoseconds
lazy_static! {
    static ref LATENCY_LAST: GaugeVec = register_gauge_vec!(
        "gstreamer_element_latency_last_gauge",
        "Last latency in nanoseconds per element",
        &["el"]
    )
    .unwrap();
    static ref LATENCY_SUM: CounterVec = register_counter_vec!(
        "gstreamer_element_latency_sum_count",
        "Sum of latencies in nanoseconds per element",
        &["el"]
    )
    .unwrap();
    static ref LATENCY_COUNT: CounterVec = register_counter_vec!(
        "gstreamer_element_latency_count_count",
        "Count of latency measurements per element",
        &["el"]
    )
    .unwrap();
}

// Our Tracer subclass
mod imp {
    use super::*;
    use glib::translate::ToGlibPtr;
    use gst_tracing_sys;

    pub struct LatencyTracer;

    #[glib::object_subclass]
    impl ObjectSubclass for LatencyTracer {
        const NAME: &'static str = "GstLatencyTracerRust";
        type Type = super::LatencyTracer;
        type ParentType = gst_tracing::Tracer;

        // Called once when the class is initialized
        fn class_init(klass: &mut Self::Class) {
            // Register hooks for tracing
            unsafe {
                let klass_ptr = klass.to_glib_none().0;
                gst_tracing_sys::gst_tracer_register_hook(
                    klass_ptr,
                    b"pad-push-pre\0".as_ptr() as *const _,
                    Some(do_push_buffer_pre),
                );
                gst_tracing_sys::gst_tracer_register_hook(
                    klass_ptr,
                    b"pad-push-post\0".as_ptr() as *const _,
                    Some(do_push_buffer_post),
                );
                gst_tracing_sys::gst_tracer_register_hook(
                    klass_ptr,
                    b"pad-pull-range-pre\0".as_ptr() as *const _,
                    Some(do_pull_range_pre),
                );
                gst_tracing_sys::gst_tracer_register_hook(
                    klass_ptr,
                    b"pad-pull-range-post\0".as_ptr() as *const _,
                    Some(do_pull_range_post),
                );
                gst_tracing_sys::gst_tracer_register_hook(
                    klass_ptr,
                    b"pad-push-event-pre\0".as_ptr() as *const _,
                    Some(do_push_event_pre),
                );
            }
        }
    }

    impl ObjectImpl for LatencyTracer {}
    impl GstObjectImpl for LatencyTracer {}
    impl gst_tracing::TracerImpl for LatencyTracer {}

    // Hook callbacks
    unsafe extern "C" fn do_push_buffer_pre(
        _tracer: *mut gst_tracing_sys::GstTracer,
        ts: u64,
        pad: *mut gst::ffi::GstPad,
    ) {
        // Send a custom downstream event with timestamp
        let pad = gst::Pad::from_glib_borrow(pad);
        if let Some(parent) = pad.parent_element() {
            if !parent.is::<gst::Bin>() && parent.flags().contains(gst::ElementFlags::SOURCE) {
                let ev = gst::event::CustomDownstream::builder("latency_probe.id")
                    .field("pad", &pad)
                    .field("ts", &ts)
                    .build();
                let _ = pad.send_event(ev);
            }
        }
    }

    unsafe extern "C" fn do_push_buffer_post(
        _tracer: *mut gst_tracing_sys::GstTracer,
        ts: u64,
        pad: *mut gst::ffi::GstPad,
    ) {
        // Calculate latency when buffer arrives at sink
        let pad = gst::Pad::from_glib_borrow(pad);
        if let Some(peer) = pad.peer() {
            if let Some(parent) = peer.parent_element() {
                if !parent.is::<gst::Bin>() && parent.flags().contains(gst::ElementFlags::SINK) {
                    if let Some(ev) = peer.qdata::<gst::Event>("latency_probe.id") {
                        if let Some(structure) = ev.structure() {
                            super::log_latency(&structure, &peer, ts, &parent);
                        }
                        peer.set_qdata::<gst::Event>("latency_probe.id", None);
                    }
                }
            }
        }
    }

    unsafe extern "C" fn do_pull_range_pre(
        tracer: *mut gst_tracing_sys::GstTracer,
        ts: u64,
        pad: *mut gst::ffi::GstPad,
    ) {
        // Similar to push_pre but for pull ranges
        do_push_buffer_pre(tracer, ts, pad);
    }

    unsafe extern "C" fn do_pull_range_post(
        tracer: *mut gst_tracing_sys::GstTracer,
        ts: u64,
        pad: *mut gst::ffi::GstPad,
    ) {
        do_push_buffer_post(tracer, ts, pad);
    }

    unsafe extern "C" fn do_push_event_pre(
        _tracer: *mut gst_tracing_sys::GstTracer,
        ts: u64,
        pad: *mut gst::ffi::GstPad,
        ev: *mut gst::ffi::GstEvent,
    ) {
        // Store the custom event on the pad for later
        let ev = gst::Event::from_glib_borrow(ev);
        if ev.type_() == gst::EventType::CustomDownstream {
            if let Some(structure) = ev.structure() {
                if structure.name() == "latency_probe.id" {
                    let pad = gst::Pad::from_glib_borrow(pad);
                    pad.set_qdata::<gst::Event>("latency_probe.id", Some(ev.clone()));
                }
            }
        }
    }
}

// Log and update Prometheus metrics
fn log_latency(
    data: &gst::StructureRef,
    _sink_pad: &gst::Pad,
    sink_ts: u64,
    parent: &gst::Element,
) {
    // Extract source pad and timestamp
    let src_pad: gst::Pad = data.get_value("pad").unwrap().get().unwrap();
    let src_ts: u64 = data.get_value("ts").unwrap().get().unwrap();
    let diff = sink_ts.saturating_sub(src_ts);
    let el = parent.name();

    LATENCY_LAST.with_label_values(&[&el]).set(diff as f64);
    LATENCY_SUM.with_label_values(&[&el]).inc_by(diff as f64);
    LATENCY_COUNT.with_label_values(&[&el]).inc();
}

// Plugin registration
gst::plugin_define!(
    latency_tracer,
    "Latency tracer with Prometheus metrics",
    plugin_init,
    "0.1",
    "MIT/X11",
    "latency_tracer",
    "latency_tracer",
    "https://example.com",
    "2025-06-26"
);

fn plugin_init(plugin: &gst::Plugin) -> bool {
    gst::TracePlugin::register::<LatencyTracer>(plugin);
    gst::info!("Registered LatencyTracerRust with Prometheus metrics");
    true
}
