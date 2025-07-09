// Derived from gstlatency.c: tracing module that logs processing latency stats
// Now uses OTLP exporter for both traces and metrics, removing Prometheus-specific HTTP server

use glib;
use glib::subclass::prelude::*;
use glib::translate::IntoGlib;
use glib::Quark;
use gstreamer as gst;
use once_cell::sync::Lazy;
use opentelemetry::global::BoxedSpan;
use std::sync::LazyLock;
use std::sync::OnceLock;

use gst::prelude::*;
use gst::subclass::prelude::*;
// OpenTelemetry and OTLP exporter
use opentelemetry::trace::{Span, SpanContext, Tracer};
use opentelemetry::{global, KeyValue};
use opentelemetry_sdk::Resource;

/// GStreamer debug category for logs
static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "otel-tracer",
        gst::DebugColorFlags::empty(),
        Some("OTLP tracer with metrics"),
    )
});

static INIT_ONCE: OnceLock<global::BoxedTracer> = OnceLock::new();
static QUARK_SINK_SPAN: Lazy<u32> = Lazy::new(|| Quark::from_str("otel-trace").into_glib());
static QUARK_SRC_SPAN_REF: Lazy<u32> = Lazy::new(|| Quark::from_str("otel-trace-ref").into_glib());

#[derive(Debug, Clone)]
struct GstSpanSrc<'a> {
    // The Src has a reference to the span
    span: &'a BoxedSpan,
}

#[derive(Debug)]
struct GstSpanSink<'a> {
    guard: opentelemetry::ContextGuard,
    span: opentelemetry::trace::SpanRef<'a>,
}

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

        global::tracer("otel-tracer")
    });
    global::tracer("otel-tracer")
}

/// GStreamer Tracer subclass
mod imp {
    use super::*;
    use core::hash;
    use glib::translate::{FromGlibPtrBorrow, IntoGlib, ToGlibPtr};
    use gobject_sys::GCallback;
    use opentelemetry::trace::TraceContextExt;
    use std::os::raw::c_void;

    #[derive(Default)]
    pub struct OtelTracerImpl;

    #[glib::object_subclass]
    impl ObjectSubclass for OtelTracerImpl {
        const NAME: &'static str = "otel-tracer";
        type Type = TelemetryTracer;
        type ParentType = gst::Tracer;
    }

    impl ObjectImpl for OtelTracerImpl {
        fn constructed(&self) {
            self.parent_constructed();
            let binding = self.obj();
            let tracer_obj: &gst::Tracer = binding.upcast_ref();
            init_otlp();
            gst::info!(CAT, "OtelTracerImpl constructed");

            // Omit ffi hooks for now, we will use safe Rust API to start with
            //   as its easier to implement & we can use the unsafe API for performance-critical parts later.
            unsafe extern "C" fn do_push_buffer_pre(
                _tracer: *mut gst::Tracer,
                ts: u64,
                pad: *mut gst::ffi::GstPad,
                buffer: *mut gst::ffi::GstBuffer,
            ) {
                // gst::info!(
                //     CAT,
                //     "do_push_buffer_pre called for pad {} at ts {} with buffer {:?}",
                //     gst::Pad::from_glib_borrow(pad).name(),
                //     ts,
                //     gst::Buffer::from_glib_borrow(buffer)
                // );
                // This function is called before a buffer is pushed to a pad.
                // We will use it to start a span for the pad.
                let pad = gst::Pad::from_glib_borrow(pad);
                let buffer = gst::Buffer::from_glib_borrow(buffer);
                pad_push_pre(ts, &pad, &buffer);
            }

            // unsafe extern "C" fn do_push_event_pre(
            //     _tracer: *mut gst::Tracer,
            //     event_ptr: *mut gst::ffi::GstEvent,
            //     pad: *mut gst::ffi::GstPad,
            // ) {
            // }

            unsafe extern "C" fn do_push_buffer_post(
                _tracer: *mut gst::Tracer,
                ts: u64,
                pad: *mut gst::ffi::GstPad,
            ) {
                // gst::info!(
                //     CAT,
                //     "do_push_buffer_post called for pad {} at ts {}",
                //     gst::Pad::from_glib_borrow(pad).name(),
                //     ts
                // );
                let peer = gst::ffi::gst_pad_get_peer(pad);
                let peer_pad = gst::Pad::from_glib_borrow(peer);
                let self_pad = gst::Pad::from_glib_borrow(pad);
                pad_push_post(ts, &peer_pad, &self_pad);
            }

            unsafe {
                let obj = tracer_obj.to_glib_none().0;
                gst::ffi::gst_tracing_register_hook(
                    obj,
                    b"pad-push-pre\0".as_ptr() as *const _,
                    std::mem::transmute::<_, GCallback>(do_push_buffer_pre as *const ()),
                );
                // gst::ffi::gst_tracing_register_hook(
                //     obj,
                //     b"pad-push-event-pre\0".as_ptr() as *const _,
                //     std::mem::transmute::<_, GCallback>(do_push_event_pre as *const ()),
                // );
                gst::ffi::gst_tracing_register_hook(
                    obj,
                    b"pad-push-post\0".as_ptr() as *const _,
                    std::mem::transmute::<_, GCallback>(do_push_buffer_post as *const ()),
                );
            }
        }
    }

    impl GstObjectImpl for OtelTracerImpl {}
    impl TracerImpl for OtelTracerImpl {}

    unsafe extern "C" fn drop_value<QD>(ptr: *mut c_void) {
        debug_assert!(!ptr.is_null());
        let value: Box<QD> = Box::from_raw(ptr as *mut QD);
        drop(value)
    }

    fn pad_push_pre(ts: u64, pad: &gstreamer::Pad, buffer: &gstreamer::Buffer) {
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
        // gst::trace!(
        //     CAT,
        //     "pad_push_pre called for pad {} with buffer {:?}",
        //     pad.name(),
        //     buffer
        // );
        if pad.direction() == gstreamer::PadDirection::Sink {
            return;
        }

        // TODO - separate change - if child span present on 'this pads' qdata, end it here

        if let Some(peer) = pad.peer() {
            // Check if we already have a span for this pad by checking the qdata
            let pad_ffi: *mut gstreamer_sys::GstPad = peer.to_glib_none().0;

            let has_no_existing_span = unsafe {
                // Get the BoxedSpan from the pad's qdata, and rebox it
                let existing_span = glib::gobject_ffi::g_object_get_qdata(
                    pad_ffi as *mut gobject_sys::GObject,
                    *QUARK_SINK_SPAN,
                ) as *mut BoxedSpan;
                existing_span.is_null()
            };

            // If no existing span, create a new one
            if has_no_existing_span {
                gst::trace!(
                    CAT,
                    "Starting new span for pad {} with peer {}",
                    pad.name(),
                    peer.name()
                );
                let tracer = init_otlp();
                let span_name = format!(
                    "pad-push-{}-{}-{}",
                    pad.name(),
                    peer.parent()
                        .map(|p| p.name().to_string())
                        .unwrap_or("unknown".to_string()),
                    peer.name(),
                );

                // if our context isn't set yet, we check to see if there is a span attached to the src pad (not peer)
                // and use that as the parent context, if not, we use the current context.
                //
                // TODO - this is the 'cross-threads' span propagation logic. too much to test at once, revisit later.
                //
                // if !opentelemetry::Context::current().has_active_span() {
                //     if let Some(src_pad) = pad.parent() {
                //         // Get the src pad's qdata
                //         let src_pad_ffi: *mut gstreamer_sys::GstObject = src_pad.to_glib_none().0;
                //         let ctx_ptr = unsafe {
                //             glib::gobject_ffi::g_object_get_qdata(
                //                 src_pad_ffi as *mut gobject_sys::GObject,
                //                 *QUARK_SRC_SPAN_REF,
                //             )
                //         } as *mut SpanContext;

                //         if !ctx_ptr.is_null() {
                //             // If we have a span, use it as the parent context
                //             gst::trace!(
                //                 CAT,
                //                 "Using span from src pad {} as parent context",
                //                 src_pad.name()
                //             );
                //             let ctx = unsafe { (*ctx_ptr).clone() };
                //             gst::trace!(CAT, "Span is recording, using it as parent context");
                //             // Use the span's context
                //             opentelemetry::Context::current().with_remote_span_context(ctx);
                //         } else {
                //             gst::trace!(CAT, "No span found on src pad {}", src_pad.name());
                //         }
                //     } else {
                //         gst::trace!(CAT, "No parent src pad found for pad {}", pad.name());
                //     }
                // }

                let mut span = tracer.start(span_name);
                if span.is_recording() {
                    // Set the spans attributes
                    let pad_c = pad.clone();
                    let src_pad_element_v = pad_c
                        .parent()
                        .map(|p| p.name().to_string())
                        .unwrap_or("unknown".to_string());
                    let src_pad_name_v = pad_c.name().to_owned().to_string();
                    let sink_pad_element_v = peer
                        .parent()
                        .map(|p| p.name().to_string())
                        .unwrap_or("unknown".to_string());

                    gst::trace!(
                        CAT,
                        "Span is recording for element {} pad {}",
                        src_pad_element_v,
                        sink_pad_element_v
                    );
                    span.set_attributes(vec![
                        KeyValue::new("src_pad_element", src_pad_element_v),
                        KeyValue::new("src_pad_name", src_pad_name_v),
                        KeyValue::new("ts_start", ts as i64),
                        // i64 is not ideal but its all KeyValue supports
                        KeyValue::new("buffer_id", buffer.as_ptr() as i64),
                        KeyValue::new("buffer_size", buffer.size() as i64),
                        KeyValue::new("sink_pad_element", sink_pad_element_v),
                        KeyValue::new("sink_pad_name", peer.name().to_string()),
                    ]);

                    // Box the span and store it in the pad's qdata
                    // TODO - this is messy, not sure if there's a better way to set the span and then send the span ref.
                    let ctx_ref = span.span_context().clone();
                    let guard = opentelemetry::Context::current_with_span(span).attach();
                    let ctx_t_s = opentelemetry::Context::current();
                    let span_to_send = ctx_t_s.span();
                    let boxed_span = Box::new(GstSpanSink {
                        guard,
                        span: span_to_send,
                    });

                    // Store the span in the pad's qdata
                    unsafe {
                        glib::gobject_ffi::g_object_set_qdata_full(
                            pad_ffi as *mut gobject_sys::GObject,
                            *QUARK_SINK_SPAN,
                            Box::into_raw(boxed_span) as *mut c_void,
                            Some(drop_value::<GstSpanSink>),
                        );
                    }

                    // Get the peer's parents' src pads if any and attach the span to them as as refs without
                    // the deallocation callback.
                    let parent = peer
                        .parent()
                        .map(gst::Object::downcast::<gst::Element>)
                        .map(Result::ok)
                        .flatten();
                    if let Some(parent) = parent {
                        parent.src_pads().iter().for_each(|src_pad| {
                            // Make sure there's no existing span reference on the src pad
                            let src_pad_ffi: *mut gstreamer_sys::GstPad = src_pad.to_glib_none().0;
                            let existing_ctx_ptr = unsafe {
                                glib::gobject_ffi::g_object_get_qdata(
                                    src_pad_ffi as *mut gobject_sys::GObject,
                                    *QUARK_SRC_SPAN_REF,
                                )
                            }
                                as *mut SpanContext;
                            if !existing_ctx_ptr.is_null() {
                                gst::trace!(
                                    CAT,
                                    "Src pad {} already has a span reference, skipping",
                                    src_pad.name()
                                );
                                return;
                            }

                            // Attach the span to the src pad as a reference
                            let src_pad_ffi: *mut gstreamer_sys::GstPad = src_pad.to_glib_none().0;
                            unsafe {
                                glib::gobject_ffi::g_object_set_qdata_full(
                                    src_pad_ffi as *mut gobject_sys::GObject,
                                    *QUARK_SRC_SPAN_REF,
                                    Box::into_raw(Box::new(ctx_ref.clone())) as *mut c_void,
                                    Some(drop_value::<SpanContext>),
                                );
                            }
                        });
                    }
                }
            }
        }
    }
    fn pad_push_post(ts: u64, peer_pad: &gstreamer::Pad, self_pad: &gstreamer::Pad) {
        // To start with simple logic:
        // First, we check if conditions are met to start a span.
        // Currently, those conditions are:
        //
        // 1. This is a sink pad.
        // 2. This sink pad has qdata with a span.
        //
        // Then we end the span with the following attributes:
        //
        // - ts_end
        // - duration (calculated from ts_start to ts_end)
        // - result (success or error)
        //
        // Then we remove the span from the qdata of the pad, so it can be garbage collected.

        // gst::trace!(
        //     CAT,
        //     "pad_push_post called for pad {}, element {:?}",
        //     pad.name(),
        //     pad.parent()
        // );
        if peer_pad.direction() == gstreamer::PadDirection::Src {
            return;
        }

        // Get the pad's qdata
        let sink_pad_ffi: *mut gstreamer_sys::GstPad = peer_pad.to_glib_none().0;
        let span_ptr = unsafe {
            glib::gobject_ffi::g_object_get_qdata(
                sink_pad_ffi as *mut gobject_sys::GObject,
                *QUARK_SINK_SPAN,
            )
        } as *mut GstSpanSink;
        gst::trace!(CAT, "Ending span for pad {} at ts {}", peer_pad.name(), ts);

        // If we have a span pointer, we can end the span
        // and remove it from the pad's qdata.
        if !span_ptr.is_null() {
            // TODO - this is a really big unsafe block.
            unsafe {
                if (*span_ptr).span.is_recording() {
                    // To check this we check if the source pads of this pads' element
                    // have any attached active spans.
                    let has_no_downstream_active_spans = peer_pad
                        .parent()
                        .map(gst::Object::downcast::<gst::Element>)
                        .map(Result::ok)
                        .flatten()
                        .and_then(|p| {
                            Some(p.src_pads().iter().all(|src_pad| {
                                // Get the src pad's qdata
                                let src_pad_ffi: *mut gstreamer_sys::GstPad =
                                    src_pad.to_glib_none().0;
                                let ctx_ptr = glib::gobject_ffi::g_object_get_qdata(
                                    src_pad_ffi as *mut gobject_sys::GObject,
                                    *QUARK_SRC_SPAN_REF,
                                ) as *mut SpanContext;

                                // If the context is not null, we have an active span
                                ctx_ptr.is_null()
                            }))
                        })
                        // By default we assume there are downstream active spans
                        .unwrap_or(false);

                    if has_no_downstream_active_spans {
                        gst::trace!(CAT, "Ending span for pad {}", peer_pad.name());

                        // Set the end time
                        (*span_ptr)
                            .span
                            .set_attributes(vec![KeyValue::new("ts_end", ts as i64)]);
                        (*span_ptr).span.end();

                        // Last chance to log the span
                        gst::trace!(
                            CAT,
                            "Span ended for pad {}: {:?}",
                            peer_pad.name(),
                            (*span_ptr)
                        );

                        // Deallocate the span through glib.
                        // This will also drop the guard.
                        glib::gobject_ffi::g_object_set_qdata(
                            sink_pad_ffi as *mut gobject_sys::GObject,
                            *QUARK_SINK_SPAN,
                            std::ptr::null_mut(),
                        );

                        // Remove the src pad's qdata reference to the span so it can be garbage collected.
                        // And upstream sink pads will see this src pad as not having an active span.
                        // Allowing them to end their own attached spans.
                        let src_pad_ffi: *mut gstreamer_sys::GstPad = self_pad.to_glib_none().0;
                        glib::gobject_ffi::g_object_set_qdata(
                            src_pad_ffi as *mut gobject_sys::GObject,
                            *QUARK_SRC_SPAN_REF,
                            std::ptr::null_mut(),
                        );
                    }
                }
            }
        }
    }
}

glib::wrapper! {
    pub struct TelemetryTracer(ObjectSubclass<imp::OtelTracerImpl>)
        @extends gst::Tracer, gst::Object;
}

/// Register plugin
pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Tracer::register(Some(plugin), "otel-tracer", TelemetryTracer::static_type())?;
    Ok(())
}
