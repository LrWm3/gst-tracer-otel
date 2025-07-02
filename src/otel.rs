// Derived from gstlatency.c: tracing module that logs processing latency stats
// Now uses OTLP exporter for both traces and metrics, removing Prometheus-specific HTTP server

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
use opentelemetry::global::BoxedSpan;
use opentelemetry::TraceId;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;
use std::sync::OnceLock;
use std::thread;

use glib::subclass::prelude::*;
use gst::prelude::*;
use gst::subclass::prelude::*;
// OpenTelemetry and OTLP exporter
use opentelemetry::trace::TracerProvider;
use opentelemetry::trace::{Span, SpanContext, Tracer};
use opentelemetry::{global, Context, KeyValue};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::trace::Sampler::{ParentBased, TraceIdRatioBased};
use opentelemetry_sdk::Resource;

/// Quark key for QData propagation
static TRACE_EVENT_QUARK: LazyLock<Quark> = LazyLock::new(|| Quark::from_str("trace_event"));
static TRACE_END_PARENT_QUARK: LazyLock<Quark> =
    LazyLock::new(|| Quark::from_str("trace_parent_end"));
static LATENCY_QUARK: LazyLock<Quark> = LazyLock::new(|| Quark::from_str("latency"));

static INIT_ONCE: OnceLock<global::BoxedTracer> = OnceLock::new();

// Threadlocal which wraps INIT_ONCE to avoid needing to lock it every time
// thread_local! {
//     static L_INIT_ONCE: &'static global::BoxedTracer = &init_otlp();
// }

/// GStreamer debug category for logs
static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "oteltracer",
        gst::DebugColorFlags::empty(),
        Some("OTLP tracer with metrics"),
    )
});

lazy_static! {
    static ref METER: opentelemetry::metrics::Meter = global::meter("oteltracer");
    static ref LATENCY_LAST: opentelemetry::metrics::ObservableGauge<f64> = METER
        .f64_observable_gauge("gstreamer_element_latency_last_gauge")
        .with_description("Last latency in nanoseconds per element")
        .build();
    static ref LATENCY_SUM: opentelemetry::metrics::Counter<f64> = METER
        .f64_counter("gstreamer_element_latency_sum_count")
        .with_description("Sum of latencies in nanoseconds per element")
        .build();
    static ref LATENCY_COUNT: opentelemetry::metrics::Counter<u64> = METER
        .u64_counter("gstreamer_element_latency_count_count")
        .with_description("Count of latency measurements per element")
        .build();
}

/// Cache for pad-specific labels
static ATTR_CACHE: Lazy<DashMap<usize, Vec<KeyValue>>> = Lazy::new(DashMap::new);

static SIDE_CHANNEL_SPANS: Lazy<DashMap<TraceId, BoxedSpan>> = Lazy::new(DashMap::new);

/// Counter to assign buffer IDs
static BUFFER_ID_COUNTER: AtomicI64 = AtomicI64::new(1);

/// Initialize both OTLP trace and metric exporters once
fn init_otlp() -> global::BoxedTracer {
    INIT_ONCE.get_or_init(|| {
        // First, create a OTLP exporter builder. Configure it as you need.
        let otlp_exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .build()
            .expect("Failed to create OTLP exporter");

        // Tracing pipeline
        let tracer_provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
            .with_sampler(opentelemetry_sdk::trace::Sampler::ParentBased(Box::new(
                opentelemetry_sdk::trace::Sampler::TraceIdRatioBased(0.001),
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

        global::tracer("oteltracer")
    });
    global::tracer("oteltracer")
}

/// Data stored in QData when catching the custom event
struct EventData {
    ctx: SpanContext,
    buffer_id: u64,
}

/// GStreamer Tracer subclass
mod imp {
    use glib::{
        bitflags::parser::from_str,
        translate::{FromGlibPtrNone, IntoGlib, ToGlibPtr},
    };
    use opentelemetry::{
        trace::{FutureExt, TraceContextExt},
        Key,
    };

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
            let tracer = init_otlp();

            unsafe extern "C" fn do_push_buffer_pre(
                _tracer: *mut gst::Tracer,
                ts: u64,
                pad: *mut gst::ffi::GstPad,
            ) {
                // not a source pad, ignore
                if ffi::gst_pad_get_direction(pad) != ffi::GST_PAD_SRC {
                    return;
                }
                let peer = ffi::gst_pad_get_peer(pad);
                if peer.is_null() {
                    return;
                }

                if let Some(parent) = get_real_pad_parent(peer) {
                    // we must get the global tracer
                    let tracer = INIT_ONCE.get().expect("OTLP tracer not initialized");

                    // if source element, we start a new span (hopefully parent span?)
                    if !parent.is_null() {
                        if glib::gobject_ffi::g_type_check_instance_is_a(
                            parent as *mut gobject_sys::GTypeInstance,
                            ffi::gst_bin_get_type(),
                        ) == glib::ffi::GFALSE
                        {
                            if element_type(parent, ffi::GST_ELEMENT_FLAG_SOURCE) {
                                let buf_id = BUFFER_ID_COUNTER.fetch_add(1, Ordering::Relaxed);

                                // Create a new span within the initial context
                                let mut span = tracer.start("gst.src");
                                if span.is_recording() {
                                    span.set_attributes(vec![
                                        KeyValue::new(
                                            "element_src",
                                            CStr::from_ptr(ffi::gst_object_get_name(
                                                parent as *mut _,
                                            ))
                                            .to_string_lossy()
                                            .into_owned(),
                                        ),
                                        KeyValue::new(
                                            "src_pad",
                                            CStr::from_ptr(ffi::gst_object_get_name(pad as *mut _))
                                                .to_string_lossy()
                                                .into_owned(),
                                        ),
                                        KeyValue::new("buffer_id", buf_id),
                                    ]);
                                    let sc = span.span_context().clone();
                                    let ev = gst::event::CustomDownstream::builder(
                                        <gst::Structure as std::str::FromStr>::from_str(
                                            "trace_probe.id",
                                        )
                                        .unwrap(),
                                    )
                                    .other_field("trace_id", sc.trace_id().to_string())
                                    .other_field("span_id", sc.span_id().to_string())
                                    .other_field("buffer_id", buf_id)
                                    .build();

                                    // Put into side channel
                                    SIDE_CHANNEL_SPANS.insert(sc.trace_id(), span);

                                    gst::Pad::from_glib_none(peer).push_event(ev);
                                    gst::info!(CAT, "Emitted trace-buffer event id={}", buf_id);
                                }
                            }

                            // FIXME - or if we have qdata with the parent span, restart the span?
                            if Context::current().has_active_span() {
                                let mut span = tracer.start("gst.element");
                                span.set_attributes(vec![
                                    KeyValue::new(
                                        "element",
                                        CStr::from_ptr(ffi::gst_object_get_name(parent as *mut _))
                                            .to_string_lossy()
                                            .into_owned(),
                                    ),
                                    KeyValue::new(
                                        "src_pad",
                                        CStr::from_ptr(ffi::gst_object_get_name(pad as *mut _))
                                            .to_string_lossy()
                                            .into_owned(),
                                    ),
                                    KeyValue::new("pre_push_timestamp_ns", ts as i64),
                                ]);

                                let trace_id = span.span_context().trace_id();
                                SIDE_CHANNEL_SPANS.insert(trace_id, span);

                                unsafe extern "C" fn drop_value<QD>(ptr: *mut c_void) {
                                    debug_assert!(!ptr.is_null());
                                    let value: Box<TraceId> = Box::from_raw(ptr as *mut TraceId);
                                    drop(value)
                                }
                                // Store the timestamp in QData for later use
                                glib::gobject_ffi::g_object_set_qdata_full(
                                    peer as *mut glib::gobject_ffi::GObject,
                                    TRACE_EVENT_QUARK.into_glib(),
                                    Box::into_raw(Box::new(trace_id)) as *mut c_void,
                                    Some(drop_value::<TraceId>),
                                );
                            }

                            // Finally we store the timestamp on LATENCY_QUARK
                            unsafe extern "C" fn drop_value<QD>(ptr: *mut c_void) {
                                debug_assert!(!ptr.is_null());
                                let value: Box<u64> = Box::from_raw(ptr as *mut u64);
                                drop(value)
                            }

                            let ptr = Box::into_raw(Box::new(ts)) as *mut c_void;
                            // Store the timestamp on the pad for later
                            glib::gobject_ffi::g_object_set_qdata_full(
                                peer as *mut gobject_sys::GObject,
                                (*LATENCY_QUARK).into_glib(),
                                ptr as *mut std::ffi::c_void,
                                Some(drop_value::<u64>),
                            );
                        }
                    }
                }
            }

            unsafe extern "C" fn do_push_event_pre(
                _tracer: *mut gst::Tracer,
                event_ptr: *mut gst::ffi::GstEvent,
                pad: *mut gst::ffi::GstPad,
            ) {
                let gst_event = gst::EventRef::from_ptr(event_ptr);
                if gst_event.is_downstream() {
                    if let Some(s) = gst_event.structure() {
                        if s.name() == "trace_probe.id" {
                            if !Context::current().has_active_span() {
                                let trace_id = s.get::<String>("trace_id").unwrap();
                                let span_id = s.get::<String>("span_id").unwrap();
                                let buf_id = s.get::<u64>("buffer_id").unwrap();
                                SpanContext::new(
                                    opentelemetry::trace::TraceId::from_hex(&trace_id).unwrap(),
                                    opentelemetry::trace::SpanId::from_hex(&span_id).unwrap(),
                                    opentelemetry::trace::TraceFlags::default(),
                                    false,
                                    Default::default(),
                                );
                                gst::info!(CAT, "Caught trace-buffer event id={}", buf_id);
                            }
                            // if this is the sink pad of a sink element, we store on quark 'TRACE_END_PARENT_QUARK'
                            if ffi::gst_pad_get_direction(pad) == ffi::GST_PAD_SINK {
                                if let Some(parent) = get_real_pad_parent(pad) {
                                    if glib::gobject_ffi::g_type_check_instance_is_a(
                                        parent as *mut gobject_sys::GTypeInstance,
                                        ffi::gst_bin_get_type(),
                                    ) == glib::ffi::GFALSE
                                    {
                                        if element_type(parent, ffi::GST_ELEMENT_FLAG_SINK) {
                                            // Store the parent span id on TRACE_END_PARENT_QUARK
                                            unsafe extern "C" fn drop_value<QD>(ptr: *mut c_void) {
                                                debug_assert!(!ptr.is_null());
                                                let value: Box<TraceId> =
                                                    Box::from_raw(ptr as *mut TraceId);
                                                drop(value)
                                            }
                                            let trace_id = opentelemetry::trace::TraceId::from_hex(
                                                s.get::<String>("trace_id").unwrap().as_str(),
                                            )
                                            .unwrap();
                                            glib::gobject_ffi::g_object_set_qdata_full(
                                                parent as *mut glib::gobject_ffi::GObject,
                                                TRACE_END_PARENT_QUARK.into_glib(),
                                                Box::into_raw(Box::new(trace_id)) as *mut c_void,
                                                Some(drop_value::<TraceId>),
                                            );
                                        }
                                    }
                                }
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
                let peer = ffi::gst_pad_get_peer(pad);
                if peer.is_null() || ffi::gst_pad_get_direction(peer) != ffi::GST_PAD_SINK {
                    return;
                }

                // Compute latency metrics
                let src_ts_ptr = glib::gobject_ffi::g_object_steal_qdata(
                    peer as *mut _,
                    LATENCY_QUARK.into_glib(),
                ) as *const u64;
                if src_ts_ptr.is_null() {
                    return;
                }
                // TODO - basically a full rewrite to get this part to work properly
                let src_ts = *src_ts_ptr;
                let diff = ts.saturating_sub(src_ts);

                // let key = (peer as usize) ^ (pad as usize);
                // let attrs = ATTR_CACHE
                //     .entry(key)
                //     .or_insert_with(|| build_attrs(peer, pad));
                // METER.record_batch(
                //     &Context::current(),
                //     attrs.as_slice(),
                //     &[
                //         opentelemetry::metrics::Measurement::new(&*LATENCY_LAST, diff as f64),
                //         opentelemetry::metrics::Measurement::new(&*LATENCY_SUM, diff as f64),
                //         opentelemetry::metrics::Measurement::new(&*LATENCY_COUNT, 1u64),
                //     ],
                // );

                // See if we have a trace span to end
                let evt_ptr = glib::gobject_ffi::g_object_steal_qdata(
                    peer as *mut _,
                    TRACE_EVENT_QUARK.into_glib(),
                ) as *mut TraceId;
                if !evt_ptr.is_null() {
                    let trace_id = Box::from_raw(evt_ptr);
                    let (_, mut span) = SIDE_CHANNEL_SPANS
                        .remove(&trace_id)
                        .expect("No span found for trace id");
                    if span.is_recording() {
                        span.set_attributes(vec![
                            KeyValue::new("post_push_timestamp_ns", ts as i64),
                            KeyValue::new("latency_ns", diff as i64),
                        ]);
                        span.end();
                    }
                }
                // If we are a sink pad and a sink element, check to see if our qdata TRACE_END_PARENT_QUARK
                // has a parent span to end
                if ffi::gst_pad_get_direction(pad) == ffi::GST_PAD_SINK {
                    if let Some(parent) = get_real_pad_parent(pad) {
                        if glib::gobject_ffi::g_type_check_instance_is_a(
                            parent as *mut gobject_sys::GTypeInstance,
                            ffi::gst_bin_get_type(),
                        ) == glib::ffi::GFALSE
                        {
                            if element_type(parent, ffi::GST_ELEMENT_FLAG_SINK) {
                                // Check if we have a parent span to end
                                let parent_trace_id_ptr = glib::gobject_ffi::g_object_steal_qdata(
                                    parent as *mut _,
                                    TRACE_END_PARENT_QUARK.into_glib(),
                                )
                                    as *mut TraceId;
                                if !parent_trace_id_ptr.is_null() {
                                    let parent_trace_id = Box::from_raw(parent_trace_id_ptr);
                                    if let Some((_, mut parent_span)) =
                                        SIDE_CHANNEL_SPANS.remove(&parent_trace_id)
                                    {
                                        if parent_span.is_recording() {
                                            parent_span.set_attributes(vec![
                                                KeyValue::new(
                                                    "element_sink",
                                                    CStr::from_ptr(ffi::gst_object_get_name(
                                                        parent as *mut _,
                                                    ))
                                                    .to_string_lossy()
                                                    // FIXME- This doesn't really make a ton of sense as we would probably want
                                                    // latency across the entire pipeline, not just the parent element
                                                    .into_owned(),
                                                ),
                                                KeyValue::new("post_push_timestamp_ns", ts as i64),
                                                KeyValue::new("latency_ns", diff as i64),
                                            ]);
                                            parent_span.end();
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            unsafe {
                let obj = tracer_obj.to_glib_none().0;
                ffi::gst_tracing_register_hook(
                    obj,
                    b"pad-push-pre\0".as_ptr() as *const _,
                    std::mem::transmute::<_, GCallback>(do_push_buffer_pre as *const ()),
                );
                ffi::gst_tracing_register_hook(
                    obj,
                    b"pad-event-push-pre\0".as_ptr() as *const _,
                    std::mem::transmute::<_, GCallback>(do_push_event_pre as *const ()),
                );
                ffi::gst_tracing_register_hook(
                    obj,
                    b"pad-push-post\0".as_ptr() as *const _,
                    std::mem::transmute::<_, GCallback>(do_push_buffer_post as *const ()),
                );
            }
        }

        fn signals() -> &'static [glib::subclass::Signal] {
            &[]
        }
    }

    impl GstObjectImpl for OtelTracerImpl {}
    impl TracerImpl for OtelTracerImpl {}

    fn get_real_pad_parent(pad: *mut ffi::GstPad) -> Option<*mut ffi::GstElement> {
        unsafe { Some(ffi::gst_object_get_parent(pad as *mut _).cast()) }
    }

    fn build_attrs(src: *mut ffi::GstPad, sink: *mut ffi::GstPad) -> Vec<KeyValue> {
        unsafe {
            let elem = ffi::gst_pad_get_parent_element(sink);
            let e_name = CStr::from_ptr(ffi::gst_object_get_name(elem as *mut _))
                .to_string_lossy()
                .into_owned();
            let s_name = CStr::from_ptr(ffi::gst_object_get_name(src as *mut _))
                .to_string_lossy()
                .into_owned();
            let t_name = CStr::from_ptr(ffi::gst_object_get_name(sink as *mut _))
                .to_string_lossy()
                .into_owned();
            vec![
                KeyValue::new("element", e_name),
                KeyValue::new("src_pad", s_name),
                KeyValue::new("sink_pad", t_name),
            ]
        }
    }

    fn element_type(elem: *mut ffi::GstElement, flag: u32) -> bool {
        unsafe {
            let ptr: *mut ffi::GstObject = elem as *mut _;
            (*ptr).flags & flag == flag
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
