//! gst-otel-tracer - glib 0.20, gstreamer 0.23, OTLP 0.15

use gstreamer as gst;
use gst::prelude::*;
use gst::subclass::prelude::*;
use glib::translate::{FromGlibPtrBorrow, ToGlibPtr};
use once_cell::sync::Lazy;

use opentelemetry::{
    metrics::{Histogram, Meter, MeterProvider, Unit},   // ← added Unit + MeterProvider
    trace::{Span, Tracer},                              // ← Span for .end()
    KeyValue,
};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{runtime::Tokio, trace as sdktrace};

use gobject_sys::GCallback;   // ← correct GLib C-callback alias

// ───────────────────────── OTel bootstrap ─────────────────────────
static OTEL: Lazy<(sdktrace::Tracer, Meter, Histogram<f64>)> = Lazy::new(|| {
    // tiny Tokio runtime for batch exporters
    let _rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()
        .unwrap();

    // NOTE: Tonic exporter builder is NOT Clone → make one per signal
    let exp_traces  = opentelemetry_otlp::new_exporter().tonic()
        .with_export_config(Default::default());
    let exp_metrics = opentelemetry_otlp::new_exporter().tonic()
        .with_export_config(Default::default());

    // ── Traces ────────────────────────────────────────────────────
    let tracer = opentelemetry_otlp::new_pipeline()
        .tracing()                       // param-less in 0.15
        .with_exporter(exp_traces)
        .install_batch(Tokio)
        .unwrap();

    // ── Metrics ───────────────────────────────────────────────────
    let meter_provider = opentelemetry_otlp::new_pipeline()
        .metrics(Tokio)                  // runtime FIRST in 0.15
        .with_exporter(exp_metrics)
        .build()
        .unwrap();

    let meter = meter_provider.meter("gst-tracer");
    let hist  = meter
        .f64_histogram("gstreamer.element.latency.ns")
        .with_unit(Unit::new("ns"))      // ← Unit, NOT &str
        .init();                         // explicit-bucket API not in 0.22

    (tracer, meter, hist)
});

// ────────────────────── Tracer subclass ───────────────────────────
mod imp {
    use super::*;
    use gstreamer as gst;
    use gst::ffi;

    #[derive(Default)]
    pub struct OtelTracer;

    #[glib::object_subclass]
    impl ObjectSubclass for OtelTracer {
        const NAME: &'static str = "OtelTracer";
        type Type       = super::OtelTracer;
        type ParentType = gst::Tracer;
    }

    // glib-0.20: constructed() lives on ObjectImpl and has **no args**
    impl ObjectImpl for OtelTracer {
        fn constructed(&self) {
            self.parent_constructed();

            let _ = &*super::OTEL;   // ensure exporter starts

            // get &gst::Tracer for hook registration
            let obj_handle = self.obj();
            let tracer_obj: &gst::Tracer = obj_handle.upcast_ref();

            // -------- C callbacks ---------------------------------
            unsafe extern "C" fn elem_latency(
                _tr: *mut ffi::GstTracer,
                element: *mut ffi::GstElement,
                time: ffi::GstClockTime,
                _ud: glib::ffi::gpointer,
            ) {
                if time == ffi::GST_CLOCK_TIME_NONE { return; }
                let elem = gst::Element::from_glib_borrow(element);
                let (_, _, hist) = &*super::OTEL;
                hist.record(
                    time as f64,
                    &[KeyValue::new("element", elem.name().to_string())], // GString→String
                );
            }

            unsafe extern "C" fn pad_push(
                _tr: *mut ffi::GstTracer,
                pad: *mut ffi::GstPad,
                _buf: *mut ffi::GstBuffer,
                _ud: glib::ffi::gpointer,
            ) {
                if fastrand::u32(..1000) != 0 { return; }  // 0.1 % sample
                let p = gst::Pad::from_glib_borrow(pad);
                let (tracer, _, _) = &*super::OTEL;

                let mut span = tracer                 // ← mutable so we can .end()
                    .span_builder("PadPush")
                    .with_attributes(vec![
                        KeyValue::new(
                            "element",
                            p.parent_element()
                                .map(|e| e.name().to_string())
                                .unwrap_or_default(),
                        ),
                        KeyValue::new("direction", format!("{:?}", p.direction())),
                    ])
                    .start(tracer);
                span.end();
            }

            unsafe {
                ffi::gst_tracing_register_hook(
                    tracer_obj.to_glib_none().0,
                    std::ptr::null(),
                    std::mem::transmute::<_, GCallback>(elem_latency as *const ()),
                );
                ffi::gst_tracing_register_hook(
                    tracer_obj.to_glib_none().0,
                    std::ptr::null(),
                    std::mem::transmute::<_, GCallback>(pad_push as *const ()),
                );
            }
        }
    }
    impl GstObjectImpl for OtelTracer {}
    impl TracerImpl   for OtelTracer {}
}

glib::wrapper! {
    pub struct OtelTracer(ObjectSubclass<imp::OtelTracer>)
        @extends gst::Tracer, gst::Object;
}

// ───────────────────── plugin boilerplate ────────────────────────
fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Tracer::register(Some(plugin), "otel-tracer", OtelTracer::static_type())?;
    Ok(())
}

gst::plugin_define!(
    oteltracer,                              // name
    "GStreamer → OpenTelemetry tracer",      // description
    plugin_init,                             // init fn ident
    env!("CARGO_PKG_VERSION"),               // version
    "MIT", "gst_otel_tracer", "gst_otel_tracer", "https://example.com"
);
