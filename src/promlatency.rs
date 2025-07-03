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
use glib;
use glib::subclass::prelude::*;
use glib::Quark;
use gobject_sys::GCallback;
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
static METRIC_CACHE: Lazy<DashMap<usize, (Gauge, Counter, Counter)>> = Lazy::new(|| DashMap::new());
static LATENCY_QUARK: Lazy<Quark> = Lazy::new(|| Quark::from_str("latency_probe.ts"));

// Define Prometheus metrics, all in nanoseconds
lazy_static! {
    static ref LATENCY_LAST: GaugeVec = register_gauge_vec!(
        "gstreamer_element_latency_last_gauge",
        "Last latency in nanoseconds per element",
        &["element", "src_pad", "sink_pad"]
    )
    .unwrap();
    static ref LATENCY_SUM: CounterVec = register_counter_vec!(
        "gstreamer_element_latency_sum_count",
        "Sum of latencies in nanoseconds per element",
        &["element", "src_pad", "sink_pad"]
    )
    .unwrap();
    static ref LATENCY_COUNT: CounterVec = register_counter_vec!(
        "gstreamer_element_latency_count_count",
        "Count of latency measurements per element",
        &["element", "src_pad", "sink_pad"]
    )
    .unwrap();
}

// Our Tracer subclass
mod imp {
    use std::{ffi::CStr, os::raw::c_void};

    use super::*;
    use glib::translate::{IntoGlib, ToGlibPtr};

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
            METRICS_SERVER_ONCE.get_or_init(|| maybe_start_metrics_server());

            // Hook callbacks
            unsafe extern "C" fn do_push_buffer_pre(
                _tracer: *mut gst::Tracer,
                ts: u64,
                pad: *mut gst::ffi::GstPad,
            ) {
                let peer = ffi::gst_pad_get_peer(pad);
                do_send_latency_ts(ts, peer);
            }

            unsafe extern "C" fn do_pull_range_pre(
                _tracer: *mut gst::Tracer,
                ts: u64,
                pad: *mut gst::ffi::GstPad,
            ) {
                // TODO - do I send for pad? or for peer?
                do_send_latency_ts(ts, pad);
            }

            unsafe extern "C" fn do_push_buffer_post(
                _tracer: *mut gst::Tracer,
                ts: u64,
                pad: *mut gst::ffi::GstPad,
            ) {
                // Calculate latency when buffer arrives at sink
                let peer = ffi::gst_pad_get_peer(pad);
                do_receive_and_record_latency_ts(ts, peer);
            }

            unsafe extern "C" fn do_pull_range_post(
                _tracer: *mut gst::Tracer,
                ts: u64,
                pad: *mut gst::ffi::GstPad,
            ) {
                // Calculate latency when buffer arrives at sink
                do_receive_and_record_latency_ts(ts, pad);
            }

            unsafe {
                ffi::gst_tracing_register_hook(
                    tracer_obj.to_glib_none().0,
                    b"pad-push-pre\0".as_ptr() as *const _,
                    std::mem::transmute::<_, GCallback>(do_push_buffer_pre as *const ()),
                );
                ffi::gst_tracing_register_hook(
                    tracer_obj.to_glib_none().0,
                    b"pad-push-post\0".as_ptr() as *const _,
                    std::mem::transmute::<_, GCallback>(do_push_buffer_post as *const ()),
                );
                ffi::gst_tracing_register_hook(
                    tracer_obj.to_glib_none().0,
                    b"pad-pull-range-pre\0".as_ptr() as *const _,
                    std::mem::transmute::<_, GCallback>(do_pull_range_pre as *const ()),
                );
                ffi::gst_tracing_register_hook(
                    tracer_obj.to_glib_none().0,
                    b"pad-pull-range-post\0".as_ptr() as *const _,
                    std::mem::transmute::<_, GCallback>(do_pull_range_post as *const ()),
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
        // TODO - if we already know the pad is a GhostPad, we could return immediately.
        // however, this would require us hook into pad lifecycle events to clean up caching when necessary.
        let ghost_pad_type = unsafe { ffi::gst_ghost_pad_get_type() };
        let is_ghost_pad = unsafe {
            glib::gobject_ffi::g_type_check_instance_is_a(
                parent_obj as *mut glib::gobject_ffi::GTypeInstance,
                ghost_pad_type,
            )
        };
        let real_parent_obj = if is_ghost_pad == glib::ffi::GTRUE {
            // If it's a GhostPad, get the real pad and then its parent
            // Just in case its a GhostPad targetting another GhostPad, we keep unwrapping.
            // This is fairly atypical but can happen 2 or 3 levels deep occasionally.
            let real_pad =
                unsafe { ffi::gst_ghost_pad_get_target(parent_obj as *mut ffi::GstGhostPad) };
            if real_pad.is_null() {
                return None;
            }
            unsafe { ffi::gst_object_get_parent(real_pad as *mut ffi::GstObject) }
        } else {
            parent_obj
        };

        // 3. Finally, cast the resulting object to an Element.
        Some(real_parent_obj as *mut ffi::GstElement)
    }

    unsafe fn do_send_latency_ts(ts: u64, pad: *mut gst::ffi::GstPad) {
        if !pad.is_null() && ffi::gst_pad_get_direction(pad) == ffi::GST_PAD_SINK {
            if let Some(parent) = get_real_pad_parent_ffi(pad) {
                if !parent.is_null() {
                    if glib::gobject_ffi::g_type_check_instance_is_a(
                        parent as *mut gobject_sys::GTypeInstance,
                        ffi::gst_bin_get_type(),
                    ) == glib::ffi::GFALSE
                    {
                        unsafe extern "C" fn drop_value<QD>(ptr: *mut c_void) {
                            debug_assert!(!ptr.is_null());
                            let value: Box<u64> = Box::from_raw(ptr as *mut u64);
                            drop(value)
                        }

                        let ptr = Box::into_raw(Box::new(ts)) as *mut c_void;
                        // Store the timestamp on the pad for later
                        glib::gobject_ffi::g_object_set_qdata_full(
                            pad as *mut gobject_sys::GObject,
                            (*LATENCY_QUARK).into_glib(),
                            ptr as *mut std::ffi::c_void,
                            Some(drop_value::<u64>),
                        );
                    }
                }
            }
        }
    }

    unsafe fn do_receive_and_record_latency_ts(ts: u64, pad: *mut gst::ffi::GstPad) {
        if !pad.is_null() && ffi::gst_pad_get_direction(pad) == ffi::GST_PAD_SINK {
            if let Some(parent) = get_real_pad_parent_ffi(pad) {
                if !parent.is_null() {
                    if glib::gobject_ffi::g_type_check_instance_is_a(
                        parent as *mut gobject_sys::GTypeInstance,
                        ffi::gst_bin_get_type(),
                    ) == glib::ffi::GFALSE
                    {
                        let src_ts = glib::gobject_ffi::g_object_steal_qdata(
                            pad as *mut gobject_sys::GObject,
                            (*LATENCY_QUARK).into_glib(),
                        ) as *const u64;
                        if !src_ts.is_null() {
                            log_latency_ffi(*src_ts, pad, ts, parent);
                        }
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
            let element_latency = ffi::gst_pad_get_parent_element(sink_pad);
            let element_latency_name = if !element_latency.is_null() {
                // If we have a parent element, use its name
                CStr::from_ptr(ffi::gst_object_get_name(
                    element_latency as *mut gst::ffi::GstObject,
                ))
                .to_str()
                .unwrap_or("unknown_element")
                .to_string()
            } else {
                // Otherwise, use the pad name directly
                CStr::from_ptr(ffi::gst_object_get_name(
                    sink_pad as *mut gst::ffi::GstObject,
                ))
                .to_str()
                .unwrap_or("unknown_pad")
                .to_string()
            };

            // format as  "element_name.sink_pad_name, if we have a parent, otherwise just "pad_name"
            let sink_name = CStr::from_ptr(ffi::gst_object_get_name(
                sink_pad as *mut gst::ffi::GstObject,
            ));

            // back to string for now
            let sink_pad_name = if !element_latency.is_null() {
                element_latency_name.clone()
                    + "."
                    + sink_name.to_str().unwrap_or("unknown_sink_pad")
            } else {
                sink_name.to_str().unwrap_or("unknown_sink_pad").to_string()
            };

            // do the same for the source pad
            let src_pad_name = if !src_pad.is_null() {
                let parent = ffi::gst_pad_get_parent_element(src_pad);
                if !parent.is_null() {
                    CStr::from_ptr(ffi::gst_object_get_name(parent as *mut gst::ffi::GstObject))
                        .to_str()
                        .unwrap_or("unknown_src_pad")
                        .to_string()
                        + "."
                        + CStr::from_ptr(ffi::gst_object_get_name(
                            src_pad as *mut gst::ffi::GstObject,
                        ))
                        .to_str()
                        .unwrap_or("unknown_src_pad")
                } else {
                    CStr::from_ptr(ffi::gst_object_get_name(
                        src_pad as *mut gst::ffi::GstObject,
                    ))
                    .to_str()
                    .unwrap_or("unknown_src_pad")
                    .to_string()
                }
            } else {
                "unknown_src_pad".into()
            };

            let labels = &[&element_latency_name, &src_pad_name, &sink_pad_name];
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
