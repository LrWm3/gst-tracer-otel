// Derived from gstlatency.c: tracing module that logs processing latency stats
// Now uses OTLP exporter for both traces and metrics, removing Prometheus-specific HTTP server

use glib::subclass::prelude::*;
use glib::translate::IntoGlib;
use glib::Quark;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gstreamer as gst;
use once_cell::sync::Lazy;
use opentelemetry::global::BoxedSpan;
use std::sync::LazyLock;
use std::sync::OnceLock;
// OpenTelemetry and OTLP exporter
use opentelemetry::trace::{Span, SpanContext, Tracer};
use opentelemetry::{global, KeyValue};
use opentelemetry_sdk::Resource;

use opentelemetry::logs::LoggerProvider;

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

#[derive(Debug)]
struct GstSpanSink<'a> {
    // guard deallocation ends span
    #[allow(dead_code)]
    guard: opentelemetry::ContextGuard,
    span: opentelemetry::trace::SpanRef<'a>,
}

/// Initialize both OTLP trace and metric exporters once
fn init_otlp() -> global::BoxedTracer {
    INIT_ONCE.get_or_init(|| {
        // First, create a OTLP exporter builder. Configure it as you need.
        // TODO - will try and wire this up later
        let otlp_exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .build()
            .expect("Failed to create OTLP exporter");

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
            .with_simple_exporter(otlp_exporter)
            .build();
        global::set_tracer_provider(tracer_provider);

        gst::info!(CAT, "OTLP exporters initialized");

        global::tracer("otel-tracer")
    });
    global::tracer("otel-tracer")
}

/// GStreamer Tracer subclass
mod imp {
    use crate::otellogbridge::{init_logs_otlp, LogBridge, StructuredBridge};

    use super::*;
    use glib::{
        ffi::{gpointer, GFALSE, GTRUE},
        translate::{FromGlib, FromGlibPtrBorrow, IntoGlib, ToGlibPtr},
    };
    use gobject_sys::GCallback;

    use gstreamer_sys::{GstBuffer, GstMeta};
    use opentelemetry::trace::TraceContextExt;
    use std::{os::raw::c_void, ptr};

    #[repr(C)]
    pub struct GstOtelSpanBuf {
        parent: gst::ffi::GstMeta,
        // The Buf has a reference to the span
        span: *const SpanContext,
    }

    unsafe impl Send for GstOtelSpanBuf {}
    unsafe impl Sync for GstOtelSpanBuf {}

    impl GstOtelSpanBuf {
        /// Attach a new meta with the given label to `buffer`.
        #[allow(dead_code)]
        pub fn add(
            buffer: &mut gst::BufferRef,
            span: SpanContext,
        ) -> gst::MetaRefMut<'_, Self, gst::meta::Standalone> {
            unsafe {
                // Prepare params for the init func
                let params = Box::into_raw(Box::new(span));
                let meta = gst::ffi::gst_buffer_add_meta(
                    buffer.as_mut_ptr(),
                    imp::gst_span_buf_get_info(),
                    &mut *params as *mut _ as *mut _,
                ) as *mut imp::GstOtelSpanBuf;

                // Ensure params is dropped before returning
                Self::from_mut_ptr(buffer, meta)
            }
        }
        /// Attach a new meta with the given label to `buffer`.
        pub fn add_ptr(buffer: *mut gst::ffi::GstBuffer, span: SpanContext) {
            unsafe {
                // Prepare params for the init func
                let params = Box::into_raw(Box::new(span));
                gst::ffi::gst_buffer_add_meta(
                    buffer,
                    imp::gst_span_buf_get_info(),
                    &mut *params as *mut _ as *mut _,
                );
            }
        }

        /// Retrieve the stored span.
        pub fn span(&self) -> &SpanContext {
            unsafe { &*self.span }
        }
    }

    unsafe extern "C" fn gst_spanbuf_init(
        meta: *mut GstMeta,
        params: gpointer,
        _buffer: *mut GstBuffer,
    ) -> glib::ffi::gboolean {
        // Cast meta to your struct
        let span_meta = meta as *mut GstOtelSpanBuf;
        // Cast params to your params struct
        let p = params as *mut SpanContext;
        gst::trace!(
            CAT,
            "gst_spanbuf_init called with meta: {:?}, params: {:?}",
            span_meta,
            *p
        );
        // Copy the span pointer into the meta
        (*span_meta).span = p;
        // Return TRUE to indicate success
        GTRUE
    }

    unsafe extern "C" fn gst_spanbuf_free(_meta: *mut GstMeta, _buffer: *mut GstBuffer) {
        // we drop the reference to the span
        let src = _meta as *mut GstOtelSpanBuf;
        drop(Box::from_raw((*src).span as *mut SpanContext));
    }

    unsafe extern "C" fn gst_spanbuf_transform(
        dest_buffer: *mut GstBuffer,
        src_meta: *mut GstMeta,
        _src_buffer: *mut GstBuffer,
        _type: glib::ffi::GQuark,
        _data: gpointer,
    ) -> glib::ffi::gboolean {
        // Registering your meta returns a GstMetaInfo pointer:
        let info = gst_span_buf_get_info(); // your function returning *const GstMetaInfo

        // Allocate a new instance on `dest_buffer`
        let new_meta = gst::ffi::gst_buffer_add_meta(dest_buffer, info, std::ptr::null_mut())
            as *mut GstOtelSpanBuf;

        if new_meta.is_null() {
            // failed to attach
            gst::error!(CAT, "Failed to attach span metadata");
            return GFALSE;
        }

        // Copy the span pointer from the source meta
        let src = src_meta as *mut GstOtelSpanBuf;
        (*new_meta).span = (*src).span;

        gst::trace!(CAT, "Span metadata transformed successfully");
        GTRUE
    }
    pub fn gst_span_buf_get_info() -> *const gst::ffi::GstMetaInfo {
        struct MetaInfo(ptr::NonNull<gst::ffi::GstMetaInfo>);
        unsafe impl Send for MetaInfo {}
        unsafe impl Sync for MetaInfo {}

        // this closure runs exactly once, even in the face of threads
        static META_INFO: Lazy<MetaInfo> = Lazy::new(|| unsafe {
            MetaInfo(
                ptr::NonNull::new(gst::ffi::gst_meta_register(
                    gst_span_buf_api_get_type().into_glib(),
                    c"GstOtelSpanBufAPI".as_ptr() as *const _,
                    std::mem::size_of::<GstOtelSpanBuf>(),
                    Some(gst_spanbuf_init),
                    Some(gst_spanbuf_free),
                    Some(gst_spanbuf_transform),
                ) as *mut gst::ffi::GstMetaInfo)
                .expect("Failed to register meta API"),
            )
        });
        META_INFO.0.as_ptr() as *const gst::ffi::GstMetaInfo
    }

    // Called once per program to register the API type
    #[allow(static_mut_refs)]
    pub fn gst_span_buf_api_get_type() -> glib::Type {
        static ONCE: std::sync::OnceLock<glib::Type> = std::sync::OnceLock::new();
        static mut TAG: [u8; 12] = [0; 12]; // mutable to allow setting the tag
        *ONCE.get_or_init(|| unsafe {
            let t = glib::Type::from_glib(gst::ffi::gst_meta_api_type_register(
                c"GstOtelSpanBuf".as_ptr() as *const _,
                TAG.as_mut_ptr() as *mut *const i8,
            ));
            assert_ne!(t, glib::Type::INVALID);
            println!("t: {t:?}");
            println!("t.into_glib(): {:?}", t.into_glib());
            t
        })
    }

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

            // this registers the API type
            // gst_span_buf_api_get_type();
            // this registers the actual GstMetaInfo (size + init/free/transform)
            // gst_span_buf_get_info();

            init_otlp();
            gst::info!(CAT, "OtelTracerImpl constructed");

            // Install the bridge into GStreamer
            let log_provider = init_logs_otlp();
            let logger = log_provider.logger("otel-tracer");

            // Create a bridge to handle GStreamer logs
            let bridge_clone = Box::new(StructuredBridge::new(logger));

            gst::log::remove_default_log_function();
            gst::log::add_log_function(move |cat, lvl, file, func, line, obj, msg| {
                // Extract trace/span from current context:

                let trace_id = opentelemetry::Context::current()
                    .span()
                    .span_context()
                    .trace_id()
                    .to_string();
                let span_id = opentelemetry::Context::current()
                    .span()
                    .span_context()
                    .span_id()
                    .to_string();

                bridge_clone
                    .log_message(&cat, lvl, file, func, line, msg, obj, &trace_id, &span_id);
            });

            // Omit ffi hooks for now, we will use safe Rust API to start with
            //   as its easier to implement & we can use the unsafe API for performance-critical parts later.
            unsafe extern "C" fn do_push_buffer_pre(
                _tracer: *mut gst::Tracer,
                ts: u64,
                pad: *mut gst::ffi::GstPad,
                buf_ptr: *mut gst::ffi::GstBuffer,
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
                let buffer = gst::Buffer::from_glib_borrow(buf_ptr);
                pad_push_pre(ts, &pad, &buffer, buf_ptr);
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
                    c"pad-push-pre".as_ptr() as *const _,
                    std::mem::transmute::<*const (), GCallback>(do_push_buffer_pre as *const ()),
                );
                // gst::ffi::gst_tracing_register_hook(
                //     obj,
                //     c"pad-push-event-pre".as_ptr() as *const _,
                //     std::mem::transmute::<_, GCallback>(do_push_event_pre as *const ()),
                // );
                gst::ffi::gst_tracing_register_hook(
                    obj,
                    c"pad-push-post".as_ptr() as *const _,
                    std::mem::transmute::<*const (), GCallback>(do_push_buffer_post as *const ()),
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

    fn pad_push_pre(
        ts: u64,
        pad: &gstreamer::Pad,
        buffer: &gst::Buffer,
        buf_ptr: *mut gst::ffi::GstBuffer,
    ) {
        // To start with simple logic:
        // First, we check if conditions are met to start a span.
        // Currently, those conditions are:
        //
        // 1. This is a source pad.
        // 2. The peer sink pad is a sink pad on a sink element.
        // 3. There is no existing span for this pad already created.
        // 4. If there is a span attached to the buffer, we use that as the parent span.
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
        if pad.direction() != gstreamer::PadDirection::Src {
            return;
        }

        // TODO - separate change - if child span present on 'this pads' qdata, end it here

        if let Some(peer) = pad.peer() {
            //
            // Just a reminder to myself on how to do this so I can use it later
            //
            if let Some(el_o) = pad.parent() {
                match el_o.downcast::<gst::Element>() {
                    Ok(elem) => {
                        // now `elem` is a gst::Element
                        // e.g. check flags:
                        if elem.element_flags().contains(gst::ElementFlags::SOURCE) {
                            // source element!
                        }

                        // unsafe version
                        unsafe {
                            let ptr: *mut gst::ffi::GstObject = elem.as_ptr() as *mut _;
                            if (*ptr).flags & gst::ffi::GST_ELEMENT_FLAG_SOURCE != 0 {
                                gst::trace!(CAT, "Element {} is a source element", elem.name());
                            } else {
                                gst::trace!(CAT, "Element {} is not a source element", elem.name());
                            }
                        }
                    }
                    Err(_) => {
                        // parent wasn’t an Element
                    }
                }
            }
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
                    "Trying to start new span for {} {} {} {}",
                    pad.parent().map(|p| p.name()).unwrap_or("unknown".into()),
                    pad.name(),
                    peer.name(),
                    peer.parent().map(|p| p.name()).unwrap_or("unknown".into()),
                );
                let tracer = init_otlp();
                let span_name = format!(
                    "pad-push-{}-{}-{}-{}",
                    pad.parent()
                        .map(|p| p.name().to_string())
                        .unwrap_or("unknown".to_string()),
                    pad.name(),
                    peer.name(),
                    peer.parent()
                        .map(|p| p.name().to_string())
                        .unwrap_or("unknown".to_string()),
                );

                // if our context isn't set yet, we check to see if there is a span attached to the src pad (not peer)
                // and use that as the parent context, if not, we use the current context.
                //
                // TODO - this is the 'cross-threads' span propagation logic. too much to test at once, revisit later.
                //
                let o_ctx = if !opentelemetry::Context::current().has_active_span() {
                    // let meta = ffi::gst_buffer_get_meta(self.as_mut_ptr(), T::meta_api().into_glib());
                    // See if we have a span on the buffer
                    let buffer_span = buffer
                        .meta::<GstOtelSpanBuf>()
                        .map(|meta| meta.span().clone());

                    // TODO - if we have a span in the buffer, use that, if not, we can only start a span if this is a
                    //        source pad.

                    buffer_span
                        .map(|span| {
                            // Use the span's context
                            opentelemetry::Context::current().with_remote_span_context(span.clone())
                        })
                        .or_else(|| {
                            gst::trace!(
                                CAT,
                                "No span found in buffer {:?}, using current context",
                                buffer
                            );
                            // If this is a source pad, we return the current context
                            if pad.direction() == gstreamer::PadDirection::Src {
                                gst::trace!(
                                    CAT,
                                    "Using current context for source pad {}",
                                    pad.name()
                                );
                                Some(opentelemetry::Context::current())
                            } else {
                                gst::trace!(
                                    CAT,
                                    "Not a source pad, cannot start span {}",
                                    pad.name()
                                );
                                None
                            }
                        })
                } else {
                    gst::trace!(
                        CAT,
                        "Current context already has an active span {} {} {} {}",
                        pad.parent().map(|p| p.name()).unwrap_or("unknown".into()),
                        pad.name(),
                        peer.name(),
                        peer.parent().map(|p| p.name()).unwrap_or("unknown".into())
                    );
                    Some(opentelemetry::Context::current())
                };

                if o_ctx.is_none() {
                    gst::trace!(
                        CAT,
                        "No context available for pad {} {} {} {}, not starting span",
                        pad.parent().map(|p| p.name()).unwrap_or("unknown".into()),
                        pad.name(),
                        peer.name(),
                        peer.parent().map(|p| p.name()).unwrap_or("unknown".into())
                    );
                    return;
                }
                let ctx = o_ctx.unwrap();

                let mut span = tracer.start_with_context(span_name, &ctx);
                let _guard = ctx.attach();
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
                        "Span is recording for {} {} {} {} - trace parent {:?}",
                        pad.parent().map(|p| p.name()).unwrap_or("unknown".into()),
                        pad.name(),
                        peer.name(),
                        peer.parent().map(|p| p.name()).unwrap_or("unknown".into()),
                        opentelemetry::Context::current()
                            .span()
                            .span_context()
                            .trace_id(),
                    );
                    let current = std::thread::current();
                    let thread_name = current
                        .name()
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| "unnamed".into());
                    let thread_id = format!("{:?}", current.id());

                    span.set_attributes(vec![
                        KeyValue::new("src_pad.element", src_pad_element_v),
                        KeyValue::new("src_pad.name", src_pad_name_v),
                        KeyValue::new("ts.start", ts as i64),
                        // i64 is not ideal but its all KeyValue supports
                        KeyValue::new("buffer.id", buffer.as_ptr() as i64),
                        KeyValue::new("buffer.size", buffer.size() as i64),
                        KeyValue::new("sink_pad.element", sink_pad_element_v),
                        KeyValue::new("sink_pad.name", peer.name().to_string()),
                        KeyValue::new("src_pad.thread.name", thread_name),
                        KeyValue::new("src_pad.thread.id", thread_id),
                    ]);

                    // Box the span and store it in the pad's qdata
                    // TODO - this is messy, not sure if there's a better way to set the span and then send the span ref.
                    let guard = opentelemetry::Context::current_with_span(span).attach();
                    let ctx_t_s = opentelemetry::Context::current();
                    let span_to_send = ctx_t_s.span();
                    let boxed_span = Box::new(GstSpanSink {
                        guard,
                        span: span_to_send,
                    });

                    gst::trace!(
                        CAT,
                        "Attached span; storing reference for {} {} {} {}",
                        pad.parent().map(|p| p.name()).unwrap_or("unknown".into()),
                        pad.name(),
                        peer.name(),
                        peer.parent().map(|p| p.name()).unwrap_or("unknown".into()),
                    );
                    // Store the span in the pad's qdata
                    unsafe {
                        glib::gobject_ffi::g_object_set_qdata_full(
                            pad_ffi as *mut gobject_sys::GObject,
                            *QUARK_SINK_SPAN,
                            Box::into_raw(boxed_span) as *mut c_void,
                            Some(drop_value::<GstSpanSink>),
                        );
                    }

                    // Store the span in the buffers Meta, if the buffer has no span already
                    if buffer.meta::<GstOtelSpanBuf>().is_none() {
                        let ctx_t_s = opentelemetry::Context::current();
                        let span_to_send = ctx_t_s.span();
                        gst::trace!(
                            CAT,
                            "Storing span in buffer {:?} for {} {} {} {}",
                            buffer,
                            pad.parent().map(|p| p.name()).unwrap_or("unknown".into()),
                            pad.name(),
                            peer.name(),
                            peer.parent().map(|p| p.name()).unwrap_or("unknown".into()),
                        );
                        GstOtelSpanBuf::add_ptr(buf_ptr, span_to_send.span_context().to_owned());
                        gst::trace!(
                            CAT,
                            "Stored span in buffer {:?} for {} {} {} {}",
                            buffer,
                            pad.parent().map(|p| p.name()).unwrap_or("unknown".into()),
                            pad.name(),
                            peer.name(),
                            peer.parent().map(|p| p.name()).unwrap_or("unknown".into()),
                        );
                    }
                }
            }
        }
    }
    fn pad_push_post(ts: u64, peer_pad: &gstreamer::Pad, _self_pad: &gstreamer::Pad) {
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
        gst::trace!(
            CAT,
            "Entering ending span for pad {} {} at ts {}",
            peer_pad.name(),
            peer_pad
                .parent()
                .map(|p| p.name())
                .unwrap_or("unknown".into()),
            ts
        );

        // If we have a span pointer, we can end the span
        // and remove it from the pad's qdata.
        if !span_ptr.is_null() {
            // TODO - this is a really big unsafe block.
            unsafe {
                if (*span_ptr).span.is_recording() {
                    gst::trace!(
                        CAT,
                        "Ending span for pad {}, {}",
                        peer_pad.name(),
                        peer_pad
                            .parent()
                            .map(|p| p.name())
                            .unwrap_or("unknown".into())
                    );

                    let current = std::thread::current();
                    let thread_name = current
                        .name()
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| "unnamed".into());
                    let thread_id = format!("{:?}", current.id());
                    // Set the end time
                    (*span_ptr).span.set_attributes(vec![
                        KeyValue::new("ts.end", ts as i64),
                        KeyValue::new("sink_pad.thread.name", thread_name),
                        KeyValue::new("sink_pad.thread.id", thread_id),
                    ]);
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
                } else {
                    gst::trace!(
                        CAT,
                        "Span for pad {}, {} is not recording, skipping end",
                        peer_pad.name(),
                        peer_pad
                            .parent()
                            .map(|p| p.name())
                            .unwrap_or("unknown".into())
                    );
                }
            }
        } else {
            gst::trace!(
                CAT,
                "No span found for pad {}, {} at ts {}",
                peer_pad.name(),
                peer_pad
                    .parent()
                    .map(|p| p.name())
                    .unwrap_or("unknown".into()),
                ts
            );
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

unsafe impl gst::MetaAPI for imp::GstOtelSpanBuf {
    type GstType = imp::GstOtelSpanBuf;
    fn meta_api() -> glib::Type {
        imp::gst_span_buf_api_get_type()
    }
}
