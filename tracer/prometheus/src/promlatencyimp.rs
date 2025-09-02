use std::{
    cell::Cell,
    os::raw::c_void,
    sync::{LazyLock, OnceLock},
    thread,
};

use glib::{
    ffi::{gboolean, GTRUE},
    translate::{FromGlibPtrNone, IntoGlib, ToGlibPtr},
    Quark,
};
use gst::{ffi, prelude::*};
use gstreamer as gst;
use prometheus::{
    gather, register_int_counter_vec, register_int_gauge_vec, Encoder, IntCounter, IntCounterVec,
    IntGauge, IntGaugeVec, TextEncoder,
};
use tiny_http::{Header, Response, Server};

// Define Prometheus metrics, all in nanoseconds
static LATENCY_LAST: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    register_int_gauge_vec!(
        "gst_element_latency_last_gauge",
        "Last latency in nanoseconds per element",
        &["element", "src_pad", "sink_pad"]
    )
    .unwrap()
});
static LATENCY_SUM: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "gst_element_latency_sum_count",
        "Sum of latencies in nanoseconds per element",
        &["element", "src_pad", "sink_pad"]
    )
    .unwrap()
});
static LATENCY_COUNT: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "gst_element_latency_count_count",
        "Count of latency measurements per element",
        &["element", "src_pad", "sink_pad"]
    )
    .unwrap()
});

thread_local! {
    /// Experimental approach to seeing if we set the span latency if
    /// we can use it to measure cross element latency.
    pub static SPAN_LATENCY: Cell<u64> = const { Cell::new(0) };
}

static PAD_CACHE_QUARK: LazyLock<glib::ffi::GQuark> =
    LazyLock::new(|| Quark::from_str("promlatency.pad_cache").into_glib());

static METRICS_SERVER_ONCE: OnceLock<()> = OnceLock::new();
pub(crate) static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "prom-latency",
        gst::DebugColorFlags::empty(),
        Some("Prometheus tracer"),
    )
});

/// This is used to cache the decision for a pad, so we don't have to
/// repeatedly check the pad's state.
/// If the value is null, it means we should skip measuring latency for this pad.
/// If the value is a valid pointer, we fetch the PadCacheData from it.
const PAD_SKIP_SENTINEL: *mut c_void = std::ptr::null_mut();

/// Data structure to hold cached pad information used for latency measurement.
struct PadCacheData {
    /// The verdict tag indicating whether to skip or measure latency.
    ts: u64, // timestamp of the last push/pull

    /// Pointer to the peer pad, used during unlink to verify the pad pair.
    peer: *mut c_void,

    last_gauge: IntGauge,
    sum_counter: IntCounter,
    // TODO - at the moment we don't differentiate between buffers into the element vs buffers out, will require
    //          a change to what we are doing here to make that work.
    count_counter: IntCounter,
}

#[derive(Default)]
pub struct PromLatencyTracerImp;

impl PromLatencyTracerImp {
    /// Register all tracing hooks on construction
    pub fn constructed(&self, tracer_obj: &gst::Tracer) {
        // Hook callbacks
        unsafe extern "C" fn do_push_buffer_pre(
            _tracer: *mut gst::Tracer,
            ts: u64,
            pad: *mut gst::ffi::GstPad,
            _buf_ptr: *mut gst::ffi::GstBuffer,
        ) {
            PromLatencyTracerImp::do_send_latency_ts(ts, pad);
        }

        unsafe extern "C" fn do_push_buffer_post(
            _tracer: *mut gst::Tracer,
            ts: u64,
            pad: *mut gst::ffi::GstPad,
        ) {
            PromLatencyTracerImp::do_receive_and_record_latency_ts(ts, pad);
        }

        unsafe extern "C" fn do_push_list_pre(
            _tracer: *mut gst::Tracer,
            ts: u64,
            pad: *mut gst::ffi::GstPad,
            _list_ptr: *mut gst::ffi::GstBufferList,
        ) {
            PromLatencyTracerImp::do_send_latency_ts(ts, pad);
        }

        unsafe extern "C" fn do_push_list_post(
            _tracer: *mut gst::Tracer,
            ts: u64,
            pad: *mut gst::ffi::GstPad,
        ) {
            PromLatencyTracerImp::do_receive_and_record_latency_ts(ts, pad);
        }

        unsafe extern "C" fn do_pull_range_pre(
            _tracer: *mut gst::Tracer,
            _ts: u64,
            _pad: *mut gst::ffi::GstPad,
        ) {
            // TODO - revisit pull, which requires us to be careful about how we traverse proxy and ghost pads.
            // For pull, we treat sink as src, src as sink as we're going the other way
            // let peer = ffi::gst_pad_get_peer(pad);
            // PromLatencyTracerImp::do_send_latency_ts(ts, peer);
        }
        unsafe extern "C" fn do_pull_range_post(
            _tracer: *mut gst::Tracer,
            _ts: u64,
            _pad: *mut gst::ffi::GstPad,
        ) {
            // TODO - revisit pull, which requires us to be careful about how we traverse proxy and ghost pads.
            // For pull, we treat sink as src, src as sink as we're going the other way
            // let peer = ffi::gst_pad_get_peer(pad);
            // PromLatencyTracerImp::do_receive_and_record_latency_ts(ts, peer, pad);
        }

        unsafe extern "C" fn do_pad_link_post(
            _tracer: *mut gst::Tracer,
            _ts: u64,
            src_pad: *mut gst::ffi::GstPad,
            sink_pad: *mut gst::ffi::GstPad,
            res: gst::ffi::GstPadLinkReturn,
        ) {
            if res == ffi::GST_PAD_LINK_OK {
                let pad_latency_cache =
                    PromLatencyTracerImp::do_create_latency_cache_for_pad_pair(src_pad, sink_pad);
                if pad_latency_cache == PAD_SKIP_SENTINEL as *mut PadCacheData {
                    gst::trace!(
                        CAT,
                        "do_pad_link_post called for src_pad: {:?}, sink_pad: {:?}, but no cache found.",
                        src_pad,
                        sink_pad
                    );
                    return;
                }

                // If we have a valid cache, we store it in the src_pad's quark data.
                glib::gobject_ffi::g_object_set_qdata_full(
                    src_pad as *mut gobject_sys::GObject,
                    *PAD_CACHE_QUARK,
                    pad_latency_cache as *mut c_void,
                    Some(PromLatencyTracerImp::drop_value::<PadCacheData>),
                );
            }
        }

        unsafe extern "C" fn do_pad_unlink_post(
            _tracer: *mut gst::Tracer,
            _ts: u64,
            src_pad: *mut gst::ffi::GstPad,
            sink_pad: *mut gst::ffi::GstPad,
            res: gboolean,
        ) {
            // For reasons unknown to me, this callback appears to be called a lot. Perhaps it is accidentally
            // registering for all events instead of just the pad unlink events.
            //
            // Anyways, we can tell by the sink_pad appearing as a small value, such as 0x11, 0x21, etc.
            if res == GTRUE && sink_pad as usize > 4096usize {
                // See if we have a cache for this pad pair. Sometimes unlink is called for the
                // src_pad, but the sink_pad is not a pad, its something else. I am not sure what.
                // Anyways, as a result, we confirm the sink_pad matches what we expect before
                // unlinking.
                let pad_cache = glib::gobject_ffi::g_object_get_qdata(
                    src_pad as *mut gobject_sys::GObject,
                    *PAD_CACHE_QUARK,
                ) as *mut PadCacheData;

                // If the peer matches the provided sink, we remove the cache.
                if !pad_cache.is_null() && sink_pad as *mut c_void == (*pad_cache).peer {
                    gst::trace!(
                        CAT,
                        "removing cache for src_pad: {:?}, sink_pad: {:?}",
                        src_pad,
                        sink_pad
                    );
                    glib::gobject_ffi::g_object_set_qdata_full(
                        src_pad as *mut gobject_sys::GObject,
                        *PAD_CACHE_QUARK,
                        std::ptr::null_mut(),
                        None,
                    );
                }
            }
        }

        unsafe {
            // Push hooks; majority of the time we are pushing.
            ffi::gst_tracing_register_hook(
                tracer_obj.to_glib_none().0,
                c"pad-push-pre".as_ptr(),
                std::mem::transmute::<*const (), Option<unsafe extern "C" fn()>>(
                    do_push_buffer_pre as *const (),
                ),
            );
            ffi::gst_tracing_register_hook(
                tracer_obj.to_glib_none().0,
                c"pad-push-post".as_ptr(),
                std::mem::transmute::<*const (), Option<unsafe extern "C" fn()>>(
                    do_push_buffer_post as *const (),
                ),
            );
            ffi::gst_tracing_register_hook(
                tracer_obj.to_glib_none().0,
                c"pad-push-list-pre".as_ptr(),
                std::mem::transmute::<*const (), Option<unsafe extern "C" fn()>>(
                    do_push_list_pre as *const (),
                ),
            );
            ffi::gst_tracing_register_hook(
                tracer_obj.to_glib_none().0,
                c"pad-push-list-post".as_ptr(),
                std::mem::transmute::<*const (), Option<unsafe extern "C" fn()>>(
                    do_push_list_post as *const (),
                ),
            );
            // Pull hooks; far less common, but still useful.
            ffi::gst_tracing_register_hook(
                tracer_obj.to_glib_none().0,
                c"pad-pull-range-pre".as_ptr(),
                std::mem::transmute::<*const (), Option<unsafe extern "C" fn()>>(
                    do_pull_range_pre as *const (),
                ),
            );
            ffi::gst_tracing_register_hook(
                tracer_obj.to_glib_none().0,
                c"pad-pull-range-post".as_ptr(),
                std::mem::transmute::<*const (), Option<unsafe extern "C" fn()>>(
                    do_pull_range_post as *const (),
                ),
            );
            // Link hooks; allow us to populate and clear the pads' quark cache.
            ffi::gst_tracing_register_hook(
                tracer_obj.to_glib_none().0,
                c"do_pad_link_post".as_ptr(),
                std::mem::transmute::<*const (), Option<unsafe extern "C" fn()>>(
                    do_pad_link_post as *const (),
                ),
            );
            ffi::gst_tracing_register_hook(
                tracer_obj.to_glib_none().0,
                c"do_pad_unlink_post".as_ptr(),
                std::mem::transmute::<*const (), Option<unsafe extern "C" fn()>>(
                    do_pad_unlink_post as *const (),
                ),
            );
        }
    }

    /// Handle the element-new hook
    pub fn element_new(&self, _ts: u64, element: &gst::Element, port: u16) {
        if element.is::<gst::Pipeline>() && port > 0 {
            METRICS_SERVER_ONCE.get_or_init(|| Self::maybe_start_metrics_server(port));
        }
    }

    // Add this function, which is the handler for the "metrics" signal
    pub fn request_metrics() -> String {
        let metric_families = gather();
        let mut buffer = Vec::new();
        let encoder = TextEncoder::new();
        encoder
            .encode(&metric_families, &mut buffer)
            .expect("Failed to encode metrics");

        String::from_utf8(buffer).expect("Metrics buffer is not valid UTF-8")
    }

    /// Given an optional `Pad`, returns the real parent `Element`, skipping over a `GhostPad` proxy.
    fn get_real_pad_parent_ffi(pad: *mut ffi::GstPad) -> Option<*mut ffi::GstElement> {
        // 1. Grab its parent as a generic `Object`.
        let parent_obj = unsafe { ffi::gst_object_get_parent(pad as *mut ffi::GstObject) };
        if parent_obj.is_null() {
            return None;
        }

        // 2. Get the real pad
        let real_pad = Self::get_real_pad_ffi(pad);

        // 3. Finally, cast the resulting object to an Element.
        real_pad.map(|p| unsafe {
            ffi::gst_object_get_parent(p as *mut ffi::GstObject) as *mut ffi::GstElement
        })
    }

    unsafe fn is_pad(pad: *mut ffi::GstPad) -> bool {
        let pad_type = ffi::gst_pad_get_type();
        glib::gobject_ffi::g_type_check_instance_is_a(
            pad as *mut glib::gobject_ffi::GTypeInstance,
            pad_type,
        ) == glib::ffi::GTRUE
    }

    unsafe fn is_proxy_pad(pad: *mut ffi::GstPad) -> bool {
        let proxy_pad_type = ffi::gst_proxy_pad_get_type();
        glib::gobject_ffi::g_type_check_instance_is_a(
            pad as *mut glib::gobject_ffi::GTypeInstance,
            proxy_pad_type,
        ) == glib::ffi::GTRUE
    }

    /// Given an optional `Pad`, returns the real parent `Element`, skipping over a `GhostPad` proxy.
    fn get_real_pad_ffi(pad: *mut ffi::GstPad) -> Option<*mut ffi::GstPad> {
        let ghost_pad_type = unsafe { ffi::gst_ghost_pad_get_type() };
        let is_ghost_pad = unsafe {
            glib::gobject_ffi::g_type_check_instance_is_a(
                pad as *mut glib::gobject_ffi::GTypeInstance,
                ghost_pad_type,
            )
        };
        let o_pad = if is_ghost_pad == glib::ffi::GTRUE {
            let maybe_real_pad =
                unsafe { ffi::gst_ghost_pad_get_target(pad as *mut ffi::GstGhostPad) };
            if maybe_real_pad.is_null() {
                None
            } else {
                Self::get_real_pad_ffi(maybe_real_pad)
            }
        } else {
            None
        };

        if o_pad.is_some() {
            return o_pad;
        }

        let is_a_proxy_pad = unsafe { Self::is_proxy_pad(pad) };
        if is_a_proxy_pad {
            let maybe_ghost_pad = unsafe {
                ffi::gst_object_get_parent(pad as *mut ffi::GstObject) as *mut ffi::GstPad
            };
            if maybe_ghost_pad.is_null() {
                None
            } else {
                // get the peer, that might be our real pad
                let maybe_real_pad = unsafe { ffi::gst_pad_get_peer(maybe_ghost_pad) };
                if maybe_real_pad.is_null() {
                    None
                } else {
                    Self::get_real_pad_ffi(maybe_real_pad)
                }
            }
        } else {
            Some(pad)
        }
    }

    /// Drop function for the `gobject` quark data.
    /// This is called when the `gobject` quark data is removed.
    /// It safely converts the pointer back to a Box and drops it.
    /// This is necessary to avoid memory leaks.
    /// Note that this function is unsafe because it assumes
    /// the pointer is valid and points to a `Box<QD>`.
    /// It is the caller's responsibility to ensure this is the case.
    unsafe extern "C" fn drop_value<QD>(ptr: *mut c_void) {
        debug_assert!(!ptr.is_null());
        let value: Box<QD> = Box::from_raw(ptr as *mut QD);

        // Explicitly drop the value as I am still new to rust and this reminds
        // me how it works.
        drop(value)
    }

    /// Given a source and sink pad, returns the PadCacheData for the pad pair.
    /// If the pads are not valid for any reason, returns a sentinel value indicating to skip this pair.
    fn do_create_latency_cache_for_pad_pair(
        src_pad: *mut gst::ffi::GstPad,
        sink_pad: *mut gst::ffi::GstPad,
    ) -> *mut PadCacheData {
        // Ensure pads are not null.
        if src_pad.is_null() || sink_pad.is_null() {
            gst::trace!(
                CAT,
                "do_get_latency_cache_for_pad_pair called with null pads: src: {:?}, sink: {:?}",
                src_pad,
                sink_pad
            );
            return PAD_SKIP_SENTINEL as *mut PadCacheData;
        }

        // Ensure what we were passed are actually pads
        if !unsafe { Self::is_pad(src_pad) } || !unsafe { Self::is_pad(sink_pad) } {
            gst::trace!(
                CAT,
                "do_get_latency_cache_for_pad_pair called with non-pad objects: src: {:?}, sink: {:?}",
                src_pad,
                sink_pad
            );
            return PAD_SKIP_SENTINEL as *mut PadCacheData;
        }

        // Ensure that the pads have a parent
        let src_parent_element = Self::get_real_pad_parent_ffi(src_pad);
        let sink_parent_element = Self::get_real_pad_parent_ffi(sink_pad);
        if src_parent_element.is_none() || sink_parent_element.is_none() {
            gst::trace!(
                CAT,
                "do_get_latency_cache_for_pad_pair called with pads without parents: src: {:?}, sink: {:?}",
                src_pad,
                sink_pad
            );
            return PAD_SKIP_SENTINEL as *mut PadCacheData;
        }

        // Prepare metrics
        let src_parent = unsafe { gst::Element::from_glib_none(src_parent_element.unwrap()) };
        let _sink_parent = unsafe { gst::Element::from_glib_none(sink_parent_element.unwrap()) };
        let src_name = src_parent.name().to_string();
        let src_pad_name = Self::pad_name(src_pad);
        let sink_pad_name = Self::pad_name(sink_pad);
        let labels = [&src_name, &src_pad_name, &sink_pad_name];
        let last_gauge = LATENCY_LAST.with_label_values(&labels);
        let sum_counter = LATENCY_SUM.with_label_values(&labels);
        let count_counter = LATENCY_COUNT.with_label_values(&labels);

        // Create cache
        Box::into_raw(Box::new(PadCacheData {
            ts: 0,
            peer: sink_pad as *mut c_void,
            last_gauge,
            sum_counter,
            count_counter,
        }))
    }

    fn pad_name(pad: *mut gst::ffi::GstPad) -> String {
        unsafe { gst::Pad::from_glib_none(pad).name().to_string() }
    }

    unsafe fn do_send_latency_ts(ts: u64, src_pad: *mut gst::ffi::GstPad) {
        let pad_cache = glib::gobject_ffi::g_object_get_qdata(
            src_pad as *mut gobject_sys::GObject,
            *PAD_CACHE_QUARK,
        ) as *mut PadCacheData;
        if pad_cache.is_null() {
            return;
        }

        // If we have a valid cache, we can safely convert the pointer to a Box.
        let pad_cache: &mut PadCacheData = &mut *pad_cache;

        // Set the ts
        pad_cache.ts = ts;

        // Zero out the span latency
        SPAN_LATENCY.with(|v| v.set(0));
    }

    unsafe fn do_receive_and_record_latency_ts(ts: u64, src_pad: *mut gst::ffi::GstPad) {
        let pad_cache = glib::gobject_ffi::g_object_get_qdata(
            src_pad as *mut gobject_sys::GObject,
            *PAD_CACHE_QUARK,
        ) as *mut PadCacheData;
        if pad_cache.is_null() {
            return;
        }

        // If we have a valid cache, we can safely convert the pointer to a Box.
        let pad_cache: &mut PadCacheData = &mut *pad_cache;

        // If the ts is 0, we skip, as we have not had a valid push yet.
        if pad_cache.ts == 0 {
            return;
        }

        // Calculate the difference
        let span_diff = ts.saturating_sub(pad_cache.ts);

        // Get cached latency if needed
        let ts_latency = SPAN_LATENCY.with(|v| v.get());
        // gst::info!(CAT, "Current span latency: {}", ts_latency);

        // Calculate the per element difference
        let el_diff = Self::compute_element_latency(span_diff, ts_latency);

        // Log the latency
        pad_cache
            .last_gauge
            .set(el_diff.try_into().unwrap_or(i64::MAX));
        pad_cache.sum_counter.inc_by(el_diff);
        pad_cache.count_counter.inc();

        // Reset the timestamp for the next push
        pad_cache.ts = 0;

        // Set the SPAN_LATENCY to span_diff so upstream elements know how much
        // latency to subtract from their own latency.
        SPAN_LATENCY.with(|v| v.set(span_diff));
    }

    /// Spawn the HTTP server in a new thread on the provided port.
    fn maybe_start_metrics_server(port: u16) {
        thread::spawn(move || {
            let addr = ("0.0.0.0", port);
            let server_r = Server::http(addr);
            if server_r.is_err() {
                gst::warning!(
                    CAT,
                    "Failed to start Prometheus metrics server on 0.0.0.0:{}",
                    port
                );
                return;
            };
            let server = server_r.unwrap();

            gst::info!(CAT, "Prometheus metrics server listening on {}", port);

            for request in server.incoming_requests() {
                // Gather and encode all registered metrics
                let metric_families = gather();
                let mut buffer = Vec::new();
                TextEncoder::new()
                    .encode(&metric_families, &mut buffer)
                    .expect("Failed to encode metrics");

                // Build and send HTTP response
                let response = Response::from_data(buffer).with_header(
                    Header::from_bytes(&b"Content-Type"[..], &b"text/plain; charset=utf-8"[..])
                        .unwrap(),
                );
                let _ = request.respond(response);
            }
        });
    }

    pub(crate) fn compute_element_latency(span_diff: u64, ts_latency: u64) -> u64 {
        span_diff.saturating_sub(ts_latency)
    }
}

#[cfg(test)]
mod tests {
    use super::PromLatencyTracerImp;

    #[test]
    fn compute_element_latency_subtracts_and_saturates() {
        assert_eq!(PromLatencyTracerImp::compute_element_latency(100, 30), 70);
        assert_eq!(PromLatencyTracerImp::compute_element_latency(30, 50), 0);
    }
}
