//! gst-otel-tracer for glib 0.20 / gstreamer 0.23 / OTLP 0.15

use gstreamer as gst;
use gst::prelude::*;
use gst::subclass::prelude::*;
use glib::translate::{FromGlibPtrBorrow, ToGlibPtr};
use once_cell::sync::Lazy;

use opentelemetry::{
    metrics::{Histogram, Meter},
    trace::Tracer as _,
    KeyValue,
};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{runtime::Tokio, trace as sdktrace};

// ─────────── OpenTelemetry bootstrap ───────────
static OTEL: Lazy<(sdktrace::Tracer, Meter, Histogram<f64>)> = Lazy::new(|| {
    // tiny Tokio RT for the batch exporters
    let _rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();

    // OTLP exporter honours OTEL_EXPORTER_OTLP_* env vars
    let exporter = opentelemetry_otlp::new_exporter()
        .tonic()
        .with_export_config(Default::default());

    // tracing pipeline (param-less in 0.15)
    let tracer = opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_exporter(exporter.clone())
        .install_batch(Tokio)
        .unwrap();

    // metrics pipeline: runtime → exporter → build
    let meter_provider = opentelemetry_otlp::new_pipeline()
        .metrics(Tokio)
        .with_exporter(exporter)
        .build()
        .unwrap();

    let hist = meter_provider
        .meter("gst-tracer")
        .f64_histogram("gstreamer.element.latency.ns")
        .with_unit("ns")
        .with_explicit_bucket_boundaries(vec![
            100., 200., 500., 1_000., 2_000., 5_000.,
            10_000., 20_000., 50_000.,  // … up to 1 s
            100_000., 200_000., 500_000.,
            1_000_000., 2_000_000., 5_000_000.,
            10_000_000., 20_000_000., 50_000_000.,
            100_000_000., 200_000_000., 500_000_000.,
            1_000_000_000.,
        ])
        .init();

    (tracer, meter_provider.meter("gst-tracer"), hist)
});

// ─────────── Tracer subclass ───────────
mod imp {
    use super::*;
    use glib::ffi::GCallback;
    use gst::ffi;

    #[derive(Default)]
    pub struct OtelTracer;

    #[glib::object_subclass]
    impl ObjectSubclass for OtelTracer {
        const NAME: &'static str = "OtelTracer";
        type Type        = super::OtelTracer;
        type ParentType  = gst::Tracer;
    }

    // life-cycle hook lives on ObjectImpl in glib-0.20
    impl ObjectImpl for OtelTracer {
        fn constructed(&self, obj: &Self::Type) {
            self.parent_constructed();          // no arg

            // make sure the OTEL static is initialised
            let _ = &*super::OTEL;

            // C hooks
            unsafe extern "C" fn elem_lat(
                _tr: *mut ffi::GstTracer,
                element: *mut ffi::GstElement,
                time: ffi::GstClockTime,
                _ud: glib::ffi::gpointer,
            ) {
                if time == ffi::GST_CLOCK_TIME_NONE { return; }
                let elem = gst::Element::from_glib_borrow(element);
                let (_, _, hist) = &*super::OTEL;
                hist.record(time as f64, &[KeyValue::new("element", elem.name().as_str())]);
            }

            unsafe extern "C" fn pad_push(
                _tr: *mut ffi::GstTracer,
                pad: *mut ffi::GstPad,
                _buf: *mut ffi::GstBuffer,
                _ud: glib::ffi::gpointer,
            ) {
                if fastrand::u32(..1000) != 0 { return; } // 0.1 %

                let p = gst::Pad::from_glib_borrow(pad);
                let (tracer, _, _) = &*super::OTEL;

                let span = tracer
                    .span_builder("PadPush")
                    .with_attributes(vec![
                        KeyValue::new("element",
                                      p.parent_element().map(|e| e.name()).unwrap_or_default().as_str()),
                        KeyValue::new("dir", format!("{:?}", p.direction())),
                    ])
                    .start(tracer);
                span.end();
            }

            unsafe {
                ffi::gst_tracing_register_hook(
                    obj.to_glib_none().0,
                    std::ptr::null(),
                    Some(std::mem::transmute::<_, GCallback>(elem_lat as *const ())),
                );
                ffi::gst_tracing_register_hook(
                    obj.to_glib_none().0,
                    std::ptr::null(),
                    Some(std::mem::transmute::<_, GCallback>(pad_push as *const ())),
                );
            }
        }
    }
    impl GstObjectImpl for OtelTracer {}
    impl TracerImpl   for OtelTracer {}   // nothing extra in 0.23
}

glib::wrapper! {
    pub struct OtelTracer(ObjectSubclass<imp::OtelTracer>)
        @extends gst::Tracer, gst::Object;
}

// ─────────── plugin boilerplate (0.23 style) ───────────
fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Tracer::register(Some(plugin), "otel-tracer", OtelTracer::static_type())?;
    Ok(())
}

gst::plugin_define!(
    oteltracer,                            // plugin name
    "GStreamer → OpenTelemetry tracer",    // description
    plugin_init,                           // init function ident
    env!("CARGO_PKG_VERSION"),             // version
    "MIT",                                 // license
    "gst_otel_tracer",                     // package
    "gst_otel_tracer",                     // origin
    "https://example.com"                  // URL
);
