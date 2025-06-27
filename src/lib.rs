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
    use super::*;
    use glib::translate::ToGlibPtr;

    #[derive(Default)]
    pub struct TelemetyTracer;

    #[glib::object_subclass]
    impl ObjectSubclass for TelemetyTracer {
        const NAME: &'static str = "telemetytracer";
        type Type = super::TelemetyTracer;
        type ParentType = gst::Tracer;
    }

    impl ObjectImpl for TelemetyTracer {
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
                // Send a custom downstream event with timestamp
                let pad = gst::Pad::from_glib_ptr_borrow(&pad);
                if let Some(parent) = get_real_pad_parent(pad) {
                    if !parent.is::<gst::Bin>() && pad.direction() == gst::PadDirection::Src {
                        if let Some(sink_pad) = pad.peer() {
                            sink_pad.set_qdata::<u64>(*LATENCY_QUARK, ts);
                        }
                    }
                }
            }

            unsafe extern "C" fn do_pull_range_pre(
                _tracer: *mut gst::Tracer,
                ts: u64,
                pad: *mut gst::ffi::GstPad,
            ) {
                // Calculate latency when buffer arrives at sink
                let pad = gst::Pad::from_glib_ptr_borrow(&pad);
                if let Some(peer) = pad.peer() {
                    if let Some(parent) = get_real_pad_parent(&peer) {
                        if !parent.is::<gst::Bin>() && pad.direction() == gst::PadDirection::Src {
                            if let Some(sink_pad) = pad.peer() {
                                sink_pad.set_qdata::<u64>(*LATENCY_QUARK, ts);
                            }
                        }
                    }
                }
            }

            unsafe extern "C" fn do_push_buffer_post(
                _tracer: *mut gst::Tracer,
                ts: u64,
                pad: *mut gst::ffi::GstPad,
            ) {
                // Calculate latency when buffer arrives at sink
                let pad = gst::Pad::from_glib_ptr_borrow(&pad);
                if let Some(peer) = pad.peer() {
                    if let Some(parent) = get_real_pad_parent(&peer) {
                        if !parent.is::<gst::Bin>() && peer.direction() == gst::PadDirection::Sink {
                            if let Some(src_ts) = peer.steal_qdata::<u64>(*LATENCY_QUARK) {
                                log_latency(src_ts, &peer, ts, &parent);
                            }
                        }
                    }
                }
            }

            unsafe extern "C" fn do_pull_range_post(
                _tracer: *mut gst::Tracer,
                ts: u64,
                pad: *mut gst::ffi::GstPad,
            ) {
                // Calculate latency when buffer arrives at sink
                let pad = gst::Pad::from_glib_ptr_borrow(&pad);
                if let Some(parent) = get_real_pad_parent(&pad) {
                    if !parent.is::<gst::Bin>() && pad.direction() == gst::PadDirection::Sink {
                        if let Some(src_ts) = pad.steal_qdata::<u64>("latency_probe.ts".into()) {
                            log_latency(src_ts, &pad, ts, &parent);
                        };
                    }
                }
            }

            // We are not using events at the moment to measure latency
            //
            // unsafe extern "C" fn do_push_event_pre(
            //     _tracer: *mut gst::Tracer,
            //     _ts: u64,
            //     pad: *mut gst::ffi::GstPad,
            //     ev: *mut gst::ffi::GstEvent,
            // ) {
            //     // Store the custom event on the pad for later
            //     let peer = gst::Pad::from_glib_ptr_borrow(&pad).peer();
            //     if let Some(peer) = peer {
            //         let parent = get_real_pad_parent(&peer);
            //         if let Some(_parent) = parent {
            //             let ev = gst::Event::from_glib_borrow(ev);
            //             if ev.type_() == gst::EventType::CustomDownstream {
            //                 if let Some(structure) = ev.structure() {
            //                     if structure.name() == "latency_probe.id" {
            //                         peer.set_qdata::<gst::Event>(
            //                             *LATENCY_QUARK,
            //                             ev.clone(),
            //                         );
            //                     }
            //                 }
            //             }
            //         }
            //     }
            // }
            // Register hooks for tracing
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
                // Not using the event method at the moment
                // ffi::gst_tracing_register_hook(
                //     tracer_obj.to_glib_none().0,
                //     b"pad-push-event-pre\0".as_ptr() as *const _,
                //     std::mem::transmute::<_, GCallback>(do_push_event_pre as *const ()),
                // );
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

    impl GstObjectImpl for TelemetyTracer {}
    impl TracerImpl for TelemetyTracer {}

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
    fn get_real_pad_parent(pad: &gst::Pad) -> Option<gst::Element> {
        // 1. Grab its parent as a generic `Object`.
        let parent_obj = pad.parent()?;

        // 2. If that parent is actually a `GhostPad`, unwrap one level further.
        let real_parent_obj = if parent_obj.is::<gst::GhostPad>() {
            // If it's a GhostPad, get the real pad and then its parent
            // Just in case its a GhostPad targetting another GhostPad, we keep unwrapping.
            // This is fairly atypical but can happen 2 or 3 levels deep occasionally.
            parent_obj
                .downcast::<gst::GhostPad>()
                .ok()?
                .target()
                .and_then(|p| p.parent())?
        } else {
            // Otherwise, just use the parent directly
            parent_obj
        };

        // 3. Finally, cast the resulting object to an Element.
        real_parent_obj.downcast::<gst::Element>().ok()
    }

    // Helper for sending latency probes. useful for tracing across entire bins.
    //
    // fn send_latency_probe(parent: &gst::Element, pad: &gst::Pad, ts: u64) {
    //     if !parent.is::<gst::Bin>() && pad.direction() == gst::PadDirection::Src {
    //         let ev = gst::event::CustomDownstream::builder(LATENCY_STRUCT_TEMPLATE.clone())
    //             .other_field("pad", pad)
    //             .other_field("ts", ts)
    //             .build();
    //         let _ = pad.push_event(ev);
    //     }
    // }

    // Log and update Prometheus metrics
    fn log_latency(src_ts: u64, sink_pad: &gst::Pad, sink_ts: u64, _parent: &gst::Element) {
        // Extract source pad and timestamp
        let src_pad = sink_pad.peer().expect("Sink pad must have a peer");
        let diff = sink_ts.saturating_sub(src_ts);

        // Create a unique key for the metric cache
        // This may not be safe in highly dynamic pipelines, as pads may be added/removed frequently resulting in the same key being reused.
        // However, this should still return the correct metrics for the same pad pair.
        // I guess this does eventually leak memory though if this continues on for too long.
        // Would be nice to use a better identity that's tied to the pad pair (element name + pad name + pipeline name)
        let key = src_pad.as_ptr() as usize + sink_pad.as_ptr() as usize;

        // Insert if absent, then get a reference
        let metrics = METRIC_CACHE.entry(key).or_insert_with(|| {
            let element_latency = sink_pad
                .parent()
                .map(|p| p.name())
                .unwrap_or_else(|| sink_pad.name());

            // format as  "element_name.src_pad_name, if we have a parent, otherwise just "pad_name"
            let src_pad_name = src_pad
                .parent()
                .map(|p| format!("{}.{}", p.name(), src_pad.name()).into())
                .unwrap_or_else(|| src_pad.name());
            let sink_pad_name = sink_pad
                .parent()
                .map(|p| format!("{}.{}", p.name(), sink_pad.name()).into())
                .unwrap_or_else(|| sink_pad.name());

            let labels = &[&element_latency, &src_pad_name, &sink_pad_name];
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
                        println!("Prometheus metrics server listening on 0.0.0.0:{}", port);

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
                    eprintln!(
                        "GST_PROMETHEUS_TRACER_PORT is not a valid port number (`{}`): {}",
                        port_str, e
                    );
                }
            }
        }
    }
}

glib::wrapper! {
    pub struct TelemetyTracer(ObjectSubclass<imp::TelemetyTracer>)
        @extends gst::Tracer, gst::Object;
}

// Register the plugin with GStreamer
pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    // Register the tracer factory
    gst::Tracer::register(Some(plugin), "prom-latency", TelemetyTracer::static_type())?;

    // Initialize the plugin
    plugin_init(plugin)?;

    Ok(())
}

// ───────────────── plugin boilerplate ──────────────────
fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    // Register the tracer factory
    register(plugin)?;
    Ok(())
}

gst::plugin_define!(
    telemetytracer, // → libgsttelemetytracer.so
    "GStreamer telemetry latency tracer",
    plugin_init,
    env!("CARGO_PKG_VERSION"),
    "MPL-2.0",
    "gst_telemetry_latency_tracer",
    "gst_telemetry_latency_tracer",
    "https://example.com"
);
