/* Derived from gstlatency.c: tracing module that logs processing latency stats
 *
 * This library is free software; you can redistribute it and/or
 * modify it under the terms of the GNU Library General Public
 * License as published by the Free Software Foundation; either
 * version 2 of the License, or (at your option) any later version.
 *
 * This library is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
 * Library General Public License for more details.
 *
 * You should have received a copy of the GNU Library General Public
 * License along with this library; if not, write to the
 * Free Software Foundation, Inc., 51 Franklin St, Fifth Floor,
 * Boston, MA 02110-1301, USA.
 */
use std::env;
use std::sync::LazyLock;
use std::sync::OnceLock;
use std::thread;

use glib::subclass::prelude::*;
use glib::translate::IntoGlib;
use glib::Quark;
use gst::ffi;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gstreamer as gst;
use lazy_static::lazy_static;
use once_cell::sync::Lazy;
use prometheus::{gather, Encoder, TextEncoder};
use tiny_http::{Header, Response, Server};

mod imp {
    use std::{cell::Cell, os::raw::c_void};

    use super::*;
    use glib::{
        ffi::{gboolean, GTRUE},
        translate::ToGlibPtr,
    };
    use prometheus::{
        register_int_counter_vec, register_int_gauge_vec, IntCounter, IntCounterVec, IntGauge,
        IntGaugeVec,
    };

    // Define Prometheus metrics, all in nanoseconds
    lazy_static! {
        static ref LATENCY_LAST: IntGaugeVec = register_int_gauge_vec!(
            "gst_element_latency_last_gauge",
            "Last latency in nanoseconds per element",
            &["element", "src_pad", "sink_pad"]
        )
        .unwrap();
        static ref LATENCY_SUM: IntCounterVec = register_int_counter_vec!(
            "gst_element_latency_sum_count",
            "Sum of latencies in nanoseconds per element",
            &["element", "src_pad", "sink_pad"]
        )
        .unwrap();
        static ref LATENCY_COUNT: IntCounterVec = register_int_counter_vec!(
            "gst_element_latency_count_count",
            "Count of latency measurements per element",
            &["element", "src_pad", "sink_pad"]
        )
        .unwrap();
    }

    thread_local! {
        /// Experimental approach to seeing if we set the span latency if
        /// we can use it to measure cross element latency.
        pub static SPAN_LATENCY: Cell<u64> = const { Cell::new(0) };
    }

    static PAD_CACHE_QUARK: Lazy<glib::ffi::GQuark> =
        Lazy::new(|| Quark::from_str("promlatency.pad_cache").into_glib());

    static METRICS_SERVER_ONCE: OnceLock<()> = OnceLock::new();
    static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
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
    pub struct PromLatencyTracer;

    #[glib::object_subclass]
    impl ObjectSubclass for PromLatencyTracer {
        const NAME: &'static str = "promlatencytracer";
        type Type = super::PromLatencyTracer;
        type ParentType = gst::Tracer;
    }

    impl ObjectImpl for PromLatencyTracer {
        // Called once when the class is initialized
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            let tracer_obj: &gst::Tracer = obj.upcast_ref();

            // Start the metrics server if not already started
            METRICS_SERVER_ONCE.get_or_init(maybe_start_metrics_server);

            // Hook callbacks
            unsafe extern "C" fn do_push_buffer_pre(
                _tracer: *mut gst::Tracer,
                ts: u64,
                pad: *mut gst::ffi::GstPad,
                _buf_ptr: *mut gst::ffi::GstBuffer,
            ) {
                do_send_latency_ts(ts, pad);
            }

            unsafe extern "C" fn do_push_buffer_post(
                _tracer: *mut gst::Tracer,
                ts: u64,
                pad: *mut gst::ffi::GstPad,
            ) {
                do_receive_and_record_latency_ts(ts, pad);
            }

            unsafe extern "C" fn do_pull_range_pre(
                _tracer: *mut gst::Tracer,
                _ts: u64,
                _pad: *mut gst::ffi::GstPad,
            ) {
                // TODO - revisit pull, which requires us to be careful about how we traverse proxy and ghost pads.
                // For pull, we treat sink as src, src as sink as we're going the other way
                // let peer = ffi::gst_pad_get_peer(pad);
                // do_send_latency_ts(ts, peer);
            }
            unsafe extern "C" fn do_pull_range_post(
                _tracer: *mut gst::Tracer,
                _ts: u64,
                _pad: *mut gst::ffi::GstPad,
            ) {
                // TODO - revisit pull, which requires us to be careful about how we traverse proxy and ghost pads.
                // For pull, we treat sink as src, src as sink as we're going the other way
                // let peer = ffi::gst_pad_get_peer(pad);
                // do_receive_and_record_latency_ts(ts, peer, pad);
            }

            unsafe extern "C" fn do_pad_link_post(
                _tracer: *mut gst::Tracer,
                _ts: u64,
                src_pad: *mut gst::ffi::GstPad,
                sink_pad: *mut gst::ffi::GstPad,
                res: gst::ffi::GstPadLinkReturn,
            ) {
                if res == ffi::GST_PAD_LINK_OK {
                    let pad_latency_cache = do_create_latency_cache_for_pad_pair(src_pad, sink_pad);
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
                        Some(drop_value::<PadCacheData>),
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

        fn signals() -> &'static [glib::subclass::Signal] {
            static SIGNALS: OnceLock<Vec<glib::subclass::Signal>> = OnceLock::new();
            // Allow the application layer to request metrics via a signal
            SIGNALS.get_or_init(|| {
                vec![glib::subclass::Signal::builder("request-metrics")
                    // The ACTION flag is key for signals meant to be called by apps
                    .flags(glib::SignalFlags::ACTION)
                    // The signal will return a string (gchar* in C)
                    .return_type::<Option<String>>()
                    .class_handler(|_, _args| {
                        // Call the request_metrics function when the signal is emitted
                        let ret = request_metrics();
                        gst::info!(
                            CAT,
                            "Prometheus metrics requested via signal, returning {} bytes",
                            ret.len()
                        );
                        Some(ret.to_value())
                    })
                    .accumulator(|_hint, ret, value| {
                        *ret = value.clone();
                        true
                    })
                    .build()]
            })
        }
    }

    impl GstObjectImpl for PromLatencyTracer {}
    impl TracerImpl for PromLatencyTracer {}

    // Add this function, which is the handler for the "request-metrics" signal
    fn request_metrics() -> String {
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
        let real_pad = get_real_pad_ffi(pad);

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
                get_real_pad_ffi(maybe_real_pad)
            }
        } else {
            None
        };

        if o_pad.is_some() {
            return o_pad;
        }

        let is_a_proxy_pad = unsafe { is_proxy_pad(pad) };
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
                    get_real_pad_ffi(maybe_real_pad)
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
    ///
    /// Background required to understand this function:
    ///
    /// Recall that pads can be arranged like any of the following patterns in gstreamer:
    ///
    /// Simple case:
    ///
    /// ```text
    /// Elem_A → Elem_A.src → Elem_B.sink → Elem_B
    /// ```
    ///
    /// Ghost pad case:
    ///
    /// ```text
    /// Bin A { child: Elem_A.src → GhostPad { child: ProxyPad.src → } } → Elem_B (sink)
    /// ```
    ///
    /// Nested case with multiple bins, leading to multiple ghost pads:
    ///
    /// ```text
    /// Bin B { child: Bin A { child: Elem_A.src → GhostPad { child: ProxyPad.src → } → GhostPad { child: ProxyPad.src → } } } → Elem_B (sink)
    /// ```
    ///
    /// While in common cases ghost pads are used to link elements across bins, they can also be used to wrap pads arbitrarily.
    /// The examples above focus on the bin case as it is the most common in practice.
    ///
    /// We've been measuring latency spanning operations for given Element B by recording ts at prepush of the source pad of upstream
    /// Element A, and then at the post push of Element A.
    ///
    /// However, when GhostPads and ProxyPads are involved, we need to ensure that we are measuring the latency across
    /// the real pads, and not accidentally including time spent performing operations in the parent bin, which may be
    /// the case if we measure the time between last src element of a bin to the first sink element outside the bin.
    ///
    /// For now this method omits the proxy pad case; however, later on we will measure latency spanning elements which
    /// will better account for proxy pads and ghost pads, and capture the true latency across elements.
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
        if !unsafe { is_pad(src_pad) } || !unsafe { is_pad(sink_pad) } {
            gst::trace!(
                CAT,
                "do_get_latency_cache_for_pad_pair called with non-pad objects: src: {:?}, sink: {:?}",
                src_pad,
                sink_pad
            );
            return PAD_SKIP_SENTINEL as *mut PadCacheData;
        }

        // Check if they are the direction we expect.
        if unsafe { ffi::gst_pad_get_direction(sink_pad) } != ffi::GST_PAD_SINK
            || unsafe { ffi::gst_pad_get_direction(src_pad) } != ffi::GST_PAD_SRC
        {
            gst::trace!(
                CAT,
                "do_get_latency_cache_for_pad_pair called with invalid pads: src: {:?}, sink: {:?}",
                src_pad,
                sink_pad
            );
            return PAD_SKIP_SENTINEL as *mut PadCacheData;
        }

        // Get the real parent of the sink pad, which is what we are measuring across.
        let parent = get_real_pad_parent_ffi(sink_pad);
        if parent.is_none() {
            unsafe {
                gst::trace!(
                    CAT,
                    "do_get_latency_cache_for_pad_pair called on {}.{} {}.{}, but no real parent found.",
                    gst::Pad::from_glib_ptr_borrow(&src_pad)
                        .parent()
                        .map(|p| p.name())
                        .unwrap_or("unknown".into()),
                    gst::Pad::from_glib_ptr_borrow(&src_pad).name(),
                    gst::Pad::from_glib_ptr_borrow(&sink_pad)
                        .parent()
                        .map(|p| p.name())
                        .unwrap_or("unknown".into()),
                    gst::Pad::from_glib_ptr_borrow(&sink_pad).name()
                );
            }
            // If we don't have a parent, we can't measure latency.
            return PAD_SKIP_SENTINEL as *mut PadCacheData;
        }

        // Ensure the parent is not a bin, as we currently do not support measuring latency across bins.
        if unsafe {
            glib::gobject_ffi::g_type_check_instance_is_a(
                parent.unwrap() as *mut gobject_sys::GTypeInstance,
                ffi::gst_bin_get_type(),
            ) == glib::ffi::GTRUE
        } {
            // If the parent is a bin, we cannot measure latency across it.
            return PAD_SKIP_SENTINEL as *mut PadCacheData;
        }

        // Here's where things get a bit weird. In the push case, if our src pad is a proxy pad,
        // we skip, because measuring latency across the proxy pad, while accurate, results in double counting;
        // once for the proxy pad to the sink, and once for the src pad to the ghost pad.
        //
        // While we would like to omit the ghost pad case, as measuring latency across the ghost pad may include parts
        // of the parent bins processing time, the ghost pad is likely not linked yet and thus we cannot retrieve it.

        if unsafe { is_proxy_pad(src_pad) } {
            // If the src pad is a proxy pad, we choose to measure latency from ghost pad to sink pad.
            return PAD_SKIP_SENTINEL as *mut PadCacheData;
        }

        // Create & return the cache.
        Box::<PadCacheData>::into_raw(Box::new(create_pad_cache_data(src_pad, sink_pad)))
    }

    fn create_pad_cache_data(
        src_pad: *mut gst::ffi::GstPad,
        sink_pad: *mut gst::ffi::GstPad,
    ) -> PadCacheData {
        // Our ts is 0, representing we have not had a valid push yet.
        let ts = 0;

        // Get the 'real' sink pad (non-ghost, non-proxy)
        let binding = get_real_pad_ffi(sink_pad).unwrap();
        let sink_pad_s = unsafe { gst::Pad::from_glib_ptr_borrow(&binding) };

        // Get the 'real' src pad (non-ghost, non-proxy)
        let binding = get_real_pad_ffi(src_pad).unwrap();
        let src_pad_s = unsafe { gst::Pad::from_glib_ptr_borrow(&binding) };

        // Get the parent of the sink, what we're measuring across.
        let sink_element_parent = get_real_pad_parent_ffi(sink_pad).unwrap();

        let sink_name = sink_pad_s.name().to_string();
        let element_latency_name = if !sink_element_parent.is_null() {
            let element_latency_s =
                unsafe { gst::Element::from_glib_ptr_borrow(&sink_element_parent) };
            // If we have a parent element, use its name
            element_latency_s.name().to_string()
        } else {
            // Otherwise, use the pad name directly
            sink_name.clone()
        };
        let sink_pad_name = if !sink_element_parent.is_null() {
            // el.name + "." + sink_pad.name()
            element_latency_name.clone() + "." + &sink_name
        } else {
            sink_name.clone()
        };

        // do the same for the source pad
        let src_pad_name = if !src_pad.is_null() {
            if let Some(parent) = get_real_pad_parent_ffi(src_pad) {
                if !parent.is_null() {
                    let element_src = unsafe { gst::Element::from_glib_ptr_borrow(&parent) };
                    // If we have a parent element, use its name
                    element_src.name().to_string() + "." + &src_pad_s.name()
                } else {
                    // Otherwise, just use the pad name
                    src_pad_s.name().to_string()
                }
            } else {
                // Otherwise, just use the pad name
                src_pad_s.name().to_string()
            }
        } else {
            "unknown_src_pad".into()
        };

        let labels = &[&element_latency_name, &src_pad_name, &sink_pad_name];
        gst::info!(
            CAT,
            "Registering latency for element: {}, src_pad: {} ({:?}), sink_pad: {} ({:?})",
            element_latency_name,
            src_pad_name,
            src_pad,
            sink_pad_name,
            sink_pad
        );

        let last_gauge = LATENCY_LAST.with_label_values(labels);
        let sum_counter = LATENCY_SUM.with_label_values(labels);
        let count_counter = LATENCY_COUNT.with_label_values(labels);

        PadCacheData {
            ts,
            peer: sink_pad as *mut c_void,
            last_gauge,
            sum_counter,
            count_counter,
        }
    }

    unsafe fn do_send_latency_ts(ts: u64, src_pad: *mut gst::ffi::GstPad) {
        let pad_cache = unsafe {
            glib::gobject_ffi::g_object_get_qdata(
                src_pad as *mut gobject_sys::GObject,
                *PAD_CACHE_QUARK,
            ) as *mut PadCacheData
        };
        if pad_cache.is_null() {
            return;
        }

        // If we have a valid cache, we can safely convert the pointer to a Box.
        let pad_cache: &mut PadCacheData = unsafe { &mut *pad_cache };

        // Set the ts
        pad_cache.ts = ts;

        // Zero out the span latency
        SPAN_LATENCY.with(|v| v.set(0));
    }

    unsafe fn do_receive_and_record_latency_ts(ts: u64, src_pad: *mut gst::ffi::GstPad) {
        let pad_cache = unsafe {
            glib::gobject_ffi::g_object_get_qdata(
                src_pad as *mut gobject_sys::GObject,
                *PAD_CACHE_QUARK,
            ) as *mut PadCacheData
        };
        if pad_cache.is_null() {
            return;
        }

        // If we have a valid cache, we can safely convert the pointer to a Box.
        let pad_cache: &mut PadCacheData = unsafe { &mut *pad_cache };

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
        let el_diff = span_diff.saturating_sub(ts_latency);

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

    /// If the env var is set and valid, spawn the HTTP server in a new thread.
    fn maybe_start_metrics_server() {
        if let Ok(port_str) = env::var("GST_PROMETHEUS_TRACER_PORT") {
            match port_str.parse::<u16>() {
                Ok(port) => {
                    // spawn the server
                    thread::spawn(move || {
                        let addr = ("0.0.0.0", port);
                        let server =
                            Server::http(addr).expect("Failed to bind Prometheus metrics server");
                        gst::info!(
                            CAT,
                            "Prometheus metrics server listening on 0.0.0.0:{}",
                            port
                        );

                        for request in server.incoming_requests() {
                            // Gather and encode all registered metrics
                            let metric_families = gather();
                            let mut buffer = Vec::new();
                            TextEncoder::new()
                                .encode(&metric_families, &mut buffer)
                                .expect("Failed to encode metrics");

                            // Build and send HTTP response
                            let response = Response::from_data(buffer).with_header(
                                Header::from_bytes(
                                    &b"Content-Type"[..],
                                    &b"text/plain; charset=utf-8"[..],
                                )
                                .unwrap(),
                            );
                            let _ = request.respond(response);
                        }
                    });
                }
                Err(e) => {
                    gst::error!(
                        CAT,
                        "GST_PROMETHEUS_TRACER_PORT is not a valid port number (`{}`): {}",
                        port_str,
                        e
                    );
                }
            }
        }
    }
}

glib::wrapper! {
    pub struct PromLatencyTracer(ObjectSubclass<imp::PromLatencyTracer>)
        @extends gst::Tracer, gst::Object;
}

// Register the plugin with GStreamer
pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    // Register the tracer factory
    gst::Tracer::register(
        Some(plugin),
        "prom-latency",
        PromLatencyTracer::static_type(),
    )?;

    Ok(())
}
