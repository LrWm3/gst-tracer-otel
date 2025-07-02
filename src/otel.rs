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
    use glib::{bitflags::parser::from_str, translate::FromGlibPtrNone};

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
            let tracer_obj: &gst::Tracer = self.obj().upcast_ref();
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
                        if element_type(parent, ffi::GST_ELEMENT_FLAG_SOURCE) {
                            // Get the current context (e.g., from a request or other source)
                            let current_context = Context::current();
                            let buf_id = BUFFER_ID_COUNTER.fetch_add(1, Ordering::Relaxed);

                            // Create a new span within the initial context
                            let span = tracer.start_with_context("gst.src", &current_context);
                            if span.is_recording() {
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
                                    KeyValue::new("buffer_id", buf_id),
                                ]);
                                let sc = span.span_context().clone();
                                let ev = gst::event::CustomDownstream::builder(
                                    <gst::Structure as std::str::FromStr>::from_str(
                                        "latency_probe.id",
                                    )
                                    .unwrap(),
                                )
                                .other_field("trace_id", sc.trace_id().to_string())
                                .other_field("span_id", sc.span_id().to_string())
                                .other_field("buffer_id", buf_id)
                                .build();
                                gst::Pad::from_glib_none(peer).push_event(ev);
                                gst::info!(CAT, "Emitted trace-buffer event id={}", buf_id);
                            }
                        }

                        // Get the current context (e.g., from a request or other source)
                        let current_context = Context::current();

                        tracer
                            .span_builder("gst.element")
                            .with_parent_context(&current_context)
                            .start(&tracer);
                        // Store the timestamp in QData for later use
                        glib::gobject_ffi::g_object_set_qdata(
                            pad as *mut glib::gobject_ffi::GObject,
                            TRACE_EVENT_QUARK.into_glib(),
                            Box::into_raw(Box::new(ts)) as *mut c_void,
                        );
                        gst::info!(CAT, "Pushing buffer pre id={}", ts);
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
                        if s.name() == "trace-buffer" {
                            let trace_id = s.get::<String>("trace_id").unwrap();
                            let span_id = s.get::<String>("span_id").unwrap();
                            let buf_id = s.get::<u64>("buffer_id").unwrap();
                            let sc = SpanContext::new(
                                opentelemetry::trace::TraceId::from_hex(&trace_id).unwrap(),
                                opentelemetry::trace::SpanId::from_hex(&span_id).unwrap(),
                                opentelemetry::trace::TraceFlags::default(),
                                false,
                                Default::default(),
                            );
                            unsafe extern "C" fn drop_evt(ptr: *mut c_void) {
                                Box::from_raw(ptr as *mut EventData);
                            }
                            let data = Box::new(EventData {
                                ctx: sc,
                                buffer_id: buf_id,
                            });
                            glib::gobject_ffi::g_object_set_qdata_full(
                                pad as *mut glib::gobject_ffi::GObject,
                                TRACE_EVENT_QUARK.into_glib(),
                                Box::into_raw(data) as *mut c_void,
                                Some(drop_evt),
                            );
                            gst::info!(CAT, "Caught trace-buffer event id={}", buf_id);
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
                let src_ts_ptr = glib::gobject_ffi::g_object_steal_qdata(
                    peer as *mut _,
                    TRACE_EVENT_QUARK.into_glib(),
                ) as *const u64;
                if src_ts_ptr.is_null() {
                    return;
                }
                let src_ts = *src_ts_ptr;
                let diff = ts.saturating_sub(src_ts);

                let key = (peer as usize) ^ (pad as usize);
                let attrs = ATTR_CACHE
                    .entry(key)
                    .or_insert_with(|| build_attrs(peer, pad));
                METER.record_batch(
                    &Context::current(),
                    attrs.as_slice(),
                    &[
                        opentelemetry::metrics::Measurement::new(&*LATENCY_LAST, diff as f64),
                        opentelemetry::metrics::Measurement::new(&*LATENCY_SUM, diff as f64),
                        opentelemetry::metrics::Measurement::new(&*LATENCY_COUNT, 1u64),
                    ],
                );

                let evt_ptr = glib::gobject_ffi::g_object_steal_qdata(
                    peer as *mut _,
                    TRACE_EVENT_QUARK.into_glib(),
                ) as *mut EventData;
                if !evt_ptr.is_null() {
                    let ed = Box::from_raw(evt_ptr);
                    let span = tracer
                        .span_builder("gst.element_latency_subspan")
                        .with_parent_context(
                            &Context::new().with_remote_span_context(ed.ctx.clone()),
                        )
                        .start(&tracer);
                    span.add_event(
                        "latency.calculated",
                        vec![KeyValue::new("latency_ns", diff)],
                    );
                    span.end();
                    gst::info!(
                        CAT,
                        "Recorded subspan id={} latency={}ns",
                        ed.buffer_id,
                        diff
                    );
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
