// Derived from gstlatency.c: tracing module that logs processing latency stats
// Now uses OTLP exporter for both traces and metrics, removing Prometheus-specific HTTP server

use dashmap::DashMap;
use glib;
use glib::subclass::prelude::*;
use glib::Quark;
use gobject_sys::GCallback;
use gst::ffi;
use gstreamer as gst;
use lazy_static::lazy_static;
use once_cell::sync::Lazy;
use opentelemetry::global::BoxedSpan;
use opentelemetry::TraceId;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use std::sync::LazyLock;
use std::sync::OnceLock;

use gst::prelude::*;
use gst::subclass::prelude::*;
// OpenTelemetry and OTLP exporter
use opentelemetry::trace::{Span, SpanContext, Tracer};
use opentelemetry::{global, Context, KeyValue};
use opentelemetry_sdk::Resource;

/// GStreamer debug category for logs
static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "oteltracer",
        gst::DebugColorFlags::empty(),
        Some("OTLP tracer with metrics"),
    )
});

static INIT_ONCE: OnceLock<global::BoxedTracer> = OnceLock::new();

/// Initialize both OTLP trace and metric exporters once
fn init_otlp() -> global::BoxedTracer {
    INIT_ONCE.get_or_init(|| {
        // First, create a OTLP exporter builder. Configure it as you need.
        // TODO - will try and wire this up later
        // let otlp_exporter = opentelemetry_otlp::SpanExporter::builder()
        //     .with_http()
        //     .build()
        //     .expect("Failed to create OTLP exporter");

        // Tracing pipeline
        let tracer_provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
            .with_sampler(opentelemetry_sdk::trace::Sampler::ParentBased(Box::new(
                opentelemetry_sdk::trace::Sampler::TraceIdRatioBased(1.0),
            )))
            .with_resource(
                Resource::builder()
                    .with_attributes(vec![KeyValue::new("service.name", "gst-prom-latency")])
                    .build(),
            )
            .with_simple_exporter(opentelemetry_stdout::SpanExporter::default())
            .build();
        global::set_tracer_provider(tracer_provider);

        gst::info!(CAT, "OTLP exporters initialized");

        global::tracer("oteltracer")
    });
    global::tracer("oteltracer")
}

/// Data stored in QData when catching the custom event
struct EventData {
    // FIXME - not in use but would allow avoiding side channels
    ctx: SpanContext,
    buffer_id: u64,
}

/// GStreamer Tracer subclass
mod imp {
    use glib::translate::{FromGlibPtrNone, IntoGlib, ToGlibPtr};
    use opentelemetry::trace::TraceContextExt;

    use super::*;
    use std::{ffi::CStr, os::raw::c_void};

    #[derive(Default)]
    pub struct OtelTracerImpl;

    #[glib::object_subclass]
    impl ObjectSubclass for OtelTracerImpl {
        const NAME: &'static str = "oteltracer";
        type Type = super::TelemetryTracer;
        type ParentType = gst::Tracer;
    }

    impl ObjectImpl for OtelTracerImpl {
        fn constructed(&self) {
            self.parent_constructed();
            let binding = self.obj();
            let tracer_obj: &gst::Tracer = binding.upcast_ref();
            init_otlp();

            // Omit ffi hooks for now, we will use safe Rust API to start with
            //   as its easier to implement & we can use the unsafe API for performance-critical parts later.
            // unsafe extern "C" fn do_push_buffer_pre(
            //     _tracer: *mut gst::Tracer,
            //     ts: u64,
            //     pad: *mut gst::ffi::GstPad,
            // ) {
            // }

            // unsafe extern "C" fn do_push_event_pre(
            //     _tracer: *mut gst::Tracer,
            //     event_ptr: *mut gst::ffi::GstEvent,
            //     pad: *mut gst::ffi::GstPad,
            // ) {
            // }

            // unsafe extern "C" fn do_push_buffer_post(
            //     _tracer: *mut gst::Tracer,
            //     ts: u64,
            //     pad: *mut gst::ffi::GstPad,
            // ) {
            // }

            // unsafe {
            //     let obj = tracer_obj.to_glib_none().0;
            //     ffi::gst_tracing_register_hook(
            //         obj,
            //         b"pad-push-pre\0".as_ptr() as *const _,
            //         std::mem::transmute::<_, GCallback>(do_push_buffer_pre as *const ()),
            //     );
            //     ffi::gst_tracing_register_hook(
            //         obj,
            //         b"pad-push-event-pre\0".as_ptr() as *const _,
            //         std::mem::transmute::<_, GCallback>(do_push_event_pre as *const ()),
            //     );
            //     ffi::gst_tracing_register_hook(
            //         obj,
            //         b"pad-push-post\0".as_ptr() as *const _,
            //         std::mem::transmute::<_, GCallback>(do_push_buffer_post as *const ()),
            //     );
            // }
        }
    }

    impl GstObjectImpl for OtelTracerImpl {}
    impl TracerImpl for OtelTracerImpl {
        fn pad_push_pre(&self, ts: u64, pad: &gstreamer::Pad, buffer: &gstreamer::Buffer) {
            // To start with simple logic:
            // First, we check if conditions are met to start a span.
            // Currently, those conditions are:
            //
            // 1. This is a source pad.
            // 2. The peer sink pad is a sink pad on a sink element.
            // 3. There is no existing span for this pad already created.
            //
            // Then we start a span with the following attributes:
            // - Src pad element name
            // - Src pad name
            // - ts_start
            // - Buffer ID
            // - Buffer size
            // - Sink pad element name
            // - Sink pad name
            //
            // Then we attach the span to the current context.
            //
            // Finally, we box & store the span in the qdata of the sink pad, so it can be retrieved later
            // when the buffer is pushed to the sink pad, have metadata added (ts_end, duration)
            if pad.direction() == gstreamer::PadDirection::Sink {
                return;
            }

            // TODO - separate change - if child span present on 'this pads' qdata, end it here

            if let Some(peer) = pad.peer() {
                // Check if we already have a span for this pad by checking the qdata
                let pad_ffi = pad.to_glib_none().0;

                let existing_span = unsafe {
                    // Get the BoxedSpan from the pad's qdata, and rebox it
                    let existing_span = glib::gobject_ffi::g_object_get_qdata(
                        pad_ffi as *mut gobject_sys::GObject,
                        Quark::from_str("otel-span").into_glib(),
                    ) as *mut BoxedSpan;
                    if existing_span.is_null() {
                        None
                    } else {
                        Some(Box::from_raw(existing_span))
                    }
                };

                // TODO - separate change - create child span of this span if present

                // If no existing span, create a new one
                if existing_span.is_none() {
                    // TODO - do i get it via 'init_otlp' or is there a more direct way?
                    let tracer = init_otlp();
                    let span_name = format!(
                        "{}-pad-push-{}-{}",
                        pad.name(),
                        peer.parent()
                            .map(|p| p.name().as_str())
                            .unwrap_or("unknown"),
                        peer.name(),
                    );
                    let span =
                        tracer.start_with_context(span_name, &opentelemetry::Context::current());
                    if span.is_recording() {
                        // Set the spans attributes
                        span.set_attributes(vec![
                            KeyValue::new(
                                "src_pad_element",
                                pad.parent().map(|p| p.name().as_str()).unwrap_or("unknown"),
                            ),
                            KeyValue::new("src_pad_name", pad.name().as_str()),
                            KeyValue::new("ts_start", ts as i64),
                            // i64 is not ideal but its all KeyValue supports
                            KeyValue::new("buffer_id", buffer.as_ptr() as i64),
                            KeyValue::new("buffer_size", buffer.size() as i64),
                            KeyValue::new(
                                "sink_pad_element",
                                peer.parent()
                                    .map(|p| p.name().as_str())
                                    .unwrap_or("unknown"),
                            ),
                            KeyValue::new("sink_pad_name", peer.name().as_str()),
                        ]);

                        unsafe extern "C" fn drop_value<QD>(ptr: *mut c_void) {
                            debug_assert!(!ptr.is_null());
                            let value: Box<u64> = Box::from_raw(ptr as *mut u64);
                            drop(value)
                        }

                        // Store the span in the pad's qdata
                        unsafe {
                            // Box the span and store it in the pad's qdata
                            let boxed_span = Box::new(span);
                            glib::gobject_ffi::g_object_set_qdata_full(
                                pad_ffi as *mut gobject_sys::GObject,
                                Quark::from_str("otel-span").into_glib(),
                                Box::into_raw(boxed_span) as *mut c_void,
                                Some(drop_value::<BoxedSpan>),
                            );
                        }
                    }
                }
            }
        }
        fn pad_push_post(
            &self,
            ts: u64,
            pad: &gstreamer::Pad,
            result: Result<gstreamer::FlowSuccess, gstreamer::FlowError>,
        ) {
        }
    }
}

glib::wrapper! {
    pub struct TelemetryTracer(ObjectSubclass<imp::OtelTracerImpl>)
        @extends gst::Tracer, gst::Object;
}

/// Register plugin
pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Tracer::register(Some(plugin), "oteltracer", TelemetryTracer::static_type())?;
    Ok(())
}
