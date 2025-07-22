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

use dashmap::DashMap;
use glib::subclass::prelude::*;
use glib::translate::IntoGlib;
use glib::Quark;
use gst::ffi;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gstreamer as gst;
use lazy_static::lazy_static;
use once_cell::sync::Lazy;
use prometheus::{
    gather, register_counter_vec, register_gauge_vec, Counter, CounterVec, Encoder, Gauge,
    GaugeVec, TextEncoder,
};
use tiny_http::{Header, Response, Server};

/// Guarantee we only start the server once, even if `plugin_init`
/// gets called multiple times by GStreamer.
static METRICS_SERVER_ONCE: OnceLock<()> = OnceLock::new();
static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "prom-latency",
        gst::DebugColorFlags::empty(),
        Some("Prometheus tracer"),
    )
});

// A global, concurrent cache mapping pad‐ptrs → (last, sum, count)
static METRIC_CACHE: Lazy<DashMap<usize, (Gauge, Counter, Counter)>> = Lazy::new(DashMap::new);
static LATENCY_QUARK: Lazy<u32> = Lazy::new(|| Quark::from_str("latency_probe.ts").into_glib());

// Define Prometheus metrics, all in nanoseconds
lazy_static! {
    static ref LATENCY_LAST: GaugeVec = register_gauge_vec!(
        "gst_element_latency_last_gauge",
        "Last latency in nanoseconds per element",
        &["element", "src_pad", "sink_pad"]
    )
    .unwrap();
    static ref LATENCY_SUM: CounterVec = register_counter_vec!(
        "gst_element_latency_sum_count",
        "Sum of latencies in nanoseconds per element",
        &["element", "src_pad", "sink_pad"]
    )
    .unwrap();
    static ref LATENCY_COUNT: CounterVec = register_counter_vec!(
        "gst_element_latency_count_count",
        "Count of latency measurements per element",
        &["element", "src_pad", "sink_pad"]
    )
    .unwrap();
}

// Our Tracer subclass
mod imp {
    use std::os::raw::c_void;

    use super::*;
    use glib::translate::ToGlibPtr;

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
            ) {
                let peer = ffi::gst_pad_get_peer(pad);
                do_send_latency_ts(ts, pad, peer);
            }

            unsafe extern "C" fn do_pull_range_pre(
                _tracer: *mut gst::Tracer,
                ts: u64,
                pad: *mut gst::ffi::GstPad,
            ) {
                // For pull, we treat sink as src, src as sink as we're going the other way
                let peer = ffi::gst_pad_get_peer(pad);
                do_send_latency_ts(ts, peer, pad);
            }

            unsafe extern "C" fn do_push_buffer_post(
                _tracer: *mut gst::Tracer,
                ts: u64,
                pad: *mut gst::ffi::GstPad,
            ) {
                // Calculate latency when buffer arrives at sink
                let peer = ffi::gst_pad_get_peer(pad);
                do_receive_and_record_latency_ts(ts, pad, peer);
            }

            unsafe extern "C" fn do_pull_range_post(
                _tracer: *mut gst::Tracer,
                ts: u64,
                pad: *mut gst::ffi::GstPad,
            ) {
                // For pull, we treat sink as src, src as sink as we're going the other way
                let peer = ffi::gst_pad_get_peer(pad);
                do_receive_and_record_latency_ts(ts, peer, pad);
            }

            unsafe {
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
                        Some(request_metrics().to_value())
                    })
                    .accumulator(|_hint, _acc, _value| {
                        // First signal handler wins
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
        gst::info!(CAT, "Metrics requested via signal");
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

    fn is_proxy_pad(pad: *mut ffi::GstPad) -> bool {
        let proxy_pad_type = unsafe { ffi::gst_proxy_pad_get_type() };
        unsafe {
            glib::gobject_ffi::g_type_check_instance_is_a(
                pad as *mut glib::gobject_ffi::GTypeInstance,
                proxy_pad_type,
            ) == glib::ffi::GTRUE
        }
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

        let proxy_pad_type = unsafe { ffi::gst_proxy_pad_get_type() };
        let is_proxy_pad = unsafe {
            glib::gobject_ffi::g_type_check_instance_is_a(
                pad as *mut glib::gobject_ffi::GTypeInstance,
                proxy_pad_type,
            )
        };

        if is_proxy_pad == glib::ffi::GTRUE {
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
        drop(value)
    }
    unsafe fn do_send_latency_ts(
        ts: u64,
        src_pad: *mut gst::ffi::GstPad,
        sink_pad: *mut gst::ffi::GstPad,
    ) {
        if !sink_pad.is_null()
            && ffi::gst_pad_get_direction(sink_pad) == ffi::GST_PAD_SINK
            && !is_proxy_pad(src_pad)
        {
            if let Some(parent) = get_real_pad_parent_ffi(sink_pad) {
                if !parent.is_null()
                    && glib::gobject_ffi::g_type_check_instance_is_a(
                        parent as *mut gobject_sys::GTypeInstance,
                        ffi::gst_bin_get_type(),
                    ) == glib::ffi::GFALSE
                {
                    // To avoid many tiny allocations, if quark is present, we overwrite it
                    let existing = glib::gobject_ffi::g_object_get_qdata(
                        sink_pad as *mut gobject_sys::GObject,
                        *LATENCY_QUARK,
                    ) as *mut u64;
                    if !existing.is_null() {
                        // Overwrite in place:
                        *existing = ts;
                    } else {
                        // First time: allocate & install
                        let ptr = Box::into_raw(Box::new(ts)) as *mut c_void;
                        glib::gobject_ffi::g_object_set_qdata_full(
                            sink_pad as *mut gobject_sys::GObject,
                            *LATENCY_QUARK,
                            ptr,
                            Some(drop_value::<u64>),
                        );
                    }
                }
            }
        }
    }

    unsafe fn do_receive_and_record_latency_ts(
        ts: u64,
        src_pad: *mut gst::ffi::GstPad,
        sink_pad: *mut gst::ffi::GstPad,
    ) {
        if !sink_pad.is_null()
            && ffi::gst_pad_get_direction(sink_pad) == ffi::GST_PAD_SINK
            && !is_proxy_pad(src_pad)
        {
            if let Some(parent) = get_real_pad_parent_ffi(sink_pad) {
                if !parent.is_null()
                    && glib::gobject_ffi::g_type_check_instance_is_a(
                        parent as *mut gobject_sys::GTypeInstance,
                        ffi::gst_bin_get_type(),
                    ) == glib::ffi::GFALSE
                {
                    // Get the the qdata; this means drop_value will not be called
                    // and we can safely convert the pointer to a Box.
                    let src_ts = glib::gobject_ffi::g_object_get_qdata(
                        sink_pad as *mut gobject_sys::GObject,
                        *LATENCY_QUARK,
                    ) as *mut u64;
                    if !src_ts.is_null() && *src_ts != 0 {
                        log_latency_ffi(*src_ts, sink_pad, ts, parent);
                        // Reset the value to avoid reusing it
                        *src_ts = 0;
                    }
                }
            }
        }
    }

    unsafe fn log_latency_ffi(
        src_ts: u64,
        sink_pad: *mut gst::ffi::GstPad,
        sink_ts: u64,
        _parent: *mut gst::ffi::GstElement,
    ) {
        // ffi version which is intended to be faster
        let src_pad = ffi::gst_pad_get_peer(sink_pad);
        let diff = sink_ts.saturating_sub(src_ts);

        // I am not sure how unsafe this is, but we do it anyways
        let key = src_pad as usize + sink_pad as usize;

        // Insert if absent, then get a reference
        let metrics = METRIC_CACHE.entry(key).or_insert_with(|| {
            // Get the real sink pad
            let binding = get_real_pad_ffi(sink_pad).unwrap();
            let sink_pad_s = gst::Pad::from_glib_ptr_borrow(&binding);
            // Get the real src pad
            let binding = get_real_pad_ffi(src_pad).unwrap();
            let src_pad_s = gst::Pad::from_glib_ptr_borrow(&binding);
            let element_latency = get_real_pad_parent_ffi(sink_pad).unwrap();

            let sink_name = sink_pad_s.name().to_string();
            let element_latency_name = if !element_latency.is_null() {
                let element_latency_s = gst::Element::from_glib_ptr_borrow(&element_latency);
                // If we have a parent element, use its name
                element_latency_s.name().to_string()
            } else {
                // Otherwise, use the pad name directly
                sink_name.clone()
            };

            // back to string for now
            let sink_pad_name = if !element_latency.is_null() {
                // el.name + "." + sink_pad.name()
                element_latency_name.clone() + "." + &sink_name
            } else {
                sink_name.clone()
            };

            // do the same for the source pad
            let src_pad_name = if !src_pad.is_null() {
                let parent = get_real_pad_parent_ffi(src_pad).unwrap();
                if !parent.is_null() {
                    let element_src = gst::Element::from_glib_ptr_borrow(&parent);
                    // If we have a parent element, use its name
                    element_src.name().to_string() + "." + &src_pad_s.name()
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
                "Logging latency for element: {}, src_pad: {}, sink_pad: {}",
                element_latency_name,
                src_pad_name,
                sink_pad_name
            );
            (
                LATENCY_LAST.with_label_values(labels),
                LATENCY_SUM.with_label_values(labels),
                LATENCY_COUNT.with_label_values(labels),
            )
        });

        // metrics is a &mut (Gauge, Counter, Counter)
        let (last_g, sum_c, cnt_c) = metrics.value();
        last_g.set(diff as f64);
        sum_c.inc_by(diff as f64);
        cnt_c.inc();
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
