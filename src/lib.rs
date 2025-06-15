//! gst-otel-tracer – glib 0.20 | gstreamer 0.23 | OTLP 0.15

use std::sync::Arc;

use dashmap::DashMap;
use gobject_sys::GCallback;
use gstreamer as gst;
use gst::prelude::*;
use gst::subclass::prelude::*;
use glib::translate::{FromGlibPtrBorrow, ToGlibPtr};
use once_cell::sync::Lazy;
use opentelemetry::{
    metrics::{Histogram, Meter, MeterProvider, Unit},
    trace::{Tracer, Span},
    KeyValue,
};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{runtime::Tokio, trace as sdktrace};
use tokio::runtime::Runtime;

// ───────────── global Tokio runtime ─────────────
static TOKIO_RT: Lazy<Runtime> = Lazy::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime")
});

// ───────────── OpenTelemetry pipelines ───────────
fn get_sampling_ratio() -> f64 {
    std::env::var("OTEL_TRACES_SAMPLING_RATIO")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|&r| (0.0..=1.0).contains(&r))
        .unwrap_or(0.001) // default 0.1 %
}

static OTEL: Lazy<(sdktrace::Tracer, Meter, Histogram<f64>)> = Lazy::new(|| {
    TOKIO_RT.block_on(async {
        let sampling_ratio = get_sampling_ratio();

        let exp_traces = opentelemetry_otlp::new_exporter()
            .tonic()
            .with_export_config(Default::default());

        let exp_metrics = opentelemetry_otlp::new_exporter()
            .tonic()
            .with_export_config(Default::default());

        let trace_config = sdktrace::config()
            .with_sampler(sdktrace::Sampler::TraceIdRatioBased(sampling_ratio));

        let tracer = opentelemetry_otlp::new_pipeline()
            .tracing()
            .with_trace_config(trace_config)
            .with_exporter(exp_traces)
            .install_batch(Tokio)
            .unwrap();

        let meter_provider = opentelemetry_otlp::new_pipeline()
            .metrics(Tokio)
            .with_exporter(exp_metrics)
            .build()
            .unwrap();

        let meter = meter_provider.meter("gst-tracer");

        let hist = meter
            .f64_histogram("gstreamer.element.latency.ns")
            .with_unit(Unit::new("ns"))
            .init();

        (tracer, meter, hist)
    })
});

// ───────────── per-pad attribute cache ─────────────
static PAD_ATTRS: Lazy<DashMap<usize, Arc<[KeyValue]>>> = Lazy::new(DashMap::new);

fn get_pad_attrs(pad: &gst::Pad) -> Arc<[KeyValue]> {
    let key = pad.as_ptr() as usize;

    PAD_ATTRS
        .entry(key)
        .or_insert_with(|| {
            let element_name = pad
                .parent_element()
                .map(|e| e.name().to_string())
                .unwrap_or_default();

            Arc::from([
                KeyValue::new("element", element_name),
                KeyValue::new("direction", format!("{:?}", pad.direction())),
            ])
        })
        .clone()
}

// ───────────── tracer subclass ─────────────
mod imp {
    use super::*;
    use gst::ffi;

    #[derive(Default)]
    pub struct OtelTracer;

    #[glib::object_subclass]
    impl ObjectSubclass for OtelTracer {
        const NAME: &'static str = "OtelTracer";
        type Type = super::OtelTracer;
        type ParentType = gst::Tracer;
    }

    impl ObjectImpl for OtelTracer {
        fn constructed(&self) {
            self.parent_constructed();

            let obj_handle = self.obj();
            let tracer_obj: &gst::Tracer = obj_handle.upcast_ref();

            // ─── C callbacks ────────────────────────────────────────────────
            unsafe extern "C" fn elem_latency(
                _tr: *mut ffi::GstTracer,
                element: *mut ffi::GstElement,
                time: ffi::GstClockTime,
                _ud: glib::ffi::gpointer,
            ) {
                if time == ffi::GST_CLOCK_TIME_NONE {
                    return;
                }

                let elem = gst::Element::from_glib_borrow(element);
                let (_, _, hist) = &*super::OTEL;

                hist.record(
                    time as f64,
                    &[KeyValue::new("element", elem.name().to_string())],
                );
            }

            unsafe extern "C" fn pad_push(
                _tr: *mut ffi::GstTracer,
                pad: *mut ffi::GstPad,
                _buf: *mut ffi::GstBuffer,
                _ud: glib::ffi::gpointer,
            ) {
                let p = gst::Pad::from_glib_borrow(pad);
                let (tracer, _, _) = &*super::OTEL;

                let attrs = super::get_pad_attrs(&p);

                let mut span = tracer
                    .span_builder("PadPush")
                    .with_attributes(attrs.as_ref())
                    .start(tracer);

                span.end();
            }

            // ─── Hook registration (specific quarks) ───────────────────────
            unsafe {
                // Element latency hook
                ffi::gst_tracing_register_hook(
                    tracer_obj.to_glib_none().0,
                    b"element-latency\0".as_ptr() as *const _,
                    std::mem::transmute::<_, GCallback>(elem_latency as *const ()),
                );

                // Pad push (single buffer)
                ffi::gst_tracing_register_hook(
                    tracer_obj.to_glib_none().0,
                    b"pad-push-post\0".as_ptr() as *const _,
                    std::mem::transmute::<_, GCallback>(pad_push as *const ()),
                );

                // Pad push (buffer-list) — optional, same callback signature
                ffi::gst_tracing_register_hook(
                    tracer_obj.to_glib_none().0,
                    b"pad-push-list-post\0".as_ptr() as *const _,
                    std::mem::transmute::<_, GCallback>(pad_push as *const ()),
                );
            }
        }
    }

    impl GstObjectImpl for OtelTracer {}
    impl TracerImpl for OtelTracer {}
}

glib::wrapper! {
    pub struct OtelTracer(ObjectSubclass<imp::OtelTracer>)
        @extends gst::Tracer, gst::Object;
}

// ───────────── plugin boilerplate ─────────────
fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Tracer::register(Some(plugin), "otel-tracer", OtelTracer::static_type())?;
    Ok(())
}

gst::plugin_define!(
    oteltracer,                          // → libgstoteltracer.so
    "GStreamer → OpenTelemetry tracer",
    plugin_init,
    env!("CARGO_PKG_VERSION"),
    "MIT",
    "gst_otel_tracer",
    "gst_otel_tracer",
    "https://example.com"
);
