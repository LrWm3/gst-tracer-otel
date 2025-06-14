// Cargo.toml excerpts
// glib = "0.20"
// gstreamer = { version = "0.23", features=["v1_20","subclass"] }
// gstreamer-sys = "0.23.6"
// opentelemetry-sdk = { version="0.22", features=["metrics","rt-tokio"] }
// opentelemetry-otlp = { version="0.15", features=["tonic","metrics","trace"] }
// fastrand = "2"
// tokio = { version = "1", features=["rt-multi-thread","macros"] }

use gstreamer as gst;
use gst::prelude::*;
use gst::subclass::prelude::*;
use glib::translate::{ToGlibPtr, FromGlibPtrBorrow};
use once_cell::sync::Lazy;

use opentelemetry::{
    metrics::{Histogram, Meter},
    KeyValue,
};
use opentelemetry_sdk::trace as sdktrace;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::runtime::Tokio;   // feature rt-tokio

// OTel bootstrap ─────────────────────────────────────────────────
static OTEL: Lazy<(sdktrace::Tracer, Meter, Histogram<f64>)> = Lazy::new(|| {
    // Tokio rt for exporter
    let _rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let exporter = opentelemetry_otlp::new_exporter()
        .tonic()
        .with_export_config(Default::default());

    let tracer = opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_exporter(exporter.clone())
        .install_batch(Tokio)
        .unwrap();

    let meter_provider = opentelemetry_otlp::new_pipeline()
        .metrics(exporter)
        .install_batch(Tokio)
        .unwrap();

    let hist = meter_provider
        .meter("gst-tracer")
        .f64_histogram("gstreamer.element.latency.ns")
        .init();

    (tracer, meter_provider.meter("gst-tracer"), hist)
});

// Tracer subclass ────────────────────────────────────────────────
mod imp {
    use super::*;
    use gst::ffi;
    use glib::ffi::GCallback;

    #[derive(Default)]
    pub struct OtelTracer;

    #[glib::object_subclass]
    impl ObjectSubclass for OtelTracer {
        const NAME: &'static str = "OtelTracer";
        type Type = super::OtelTracer;
        type ParentType = gst::Tracer;
    }

    impl ObjectImpl for OtelTracer {
        fn constructed(&self, obj: &Self::Type) {
            self.parent_constructed();   // 0.20 signature

            unsafe extern "C" fn elem_lat(
                _tr: *mut ffi::GstTracer,
                element: *mut ffi::GstElement,
                time: ffi::GstClockTime,
                _ud: glib::ffi::gpointer,
            ) {
                if time == ffi::GST_CLOCK_TIME_NONE { return; }
                let elem = gst::Element::from_glib_borrow(element);
                let (_, _, hist) = &*super::OTEL;
                hist.record(time as f64, &[KeyValue::new("element", elem.name())]);
            }

            unsafe extern "C" fn pad_push(
                _tr: *mut ffi::GstTracer,
                pad: *mut ffi::GstPad,
                _buf: *mut ffi::GstBuffer,
                _ud: glib::ffi::gpointer,
            ) {
                if fastrand::u32(..1000) != 0 { return; }
                let p = gst::Pad::from_glib_borrow(pad);
                let (tracer, _, _) = &*super::OTEL;
                let span = tracer
                    .span_builder("PadPush")
                    .with_attributes(vec![
                        KeyValue::new("element", p.parent_element().map(|e| e.name()).unwrap_or_default()),
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
    impl TracerImpl   for OtelTracer {}
}

glib::wrapper! {
    pub struct OtelTracer(ObjectSubclass<imp::OtelTracer>)
        @extends gst::Tracer, gst::Object;
}

gst::plugin_define!(
    gst_otel,
    "OTel tracer",
    |plugin| {
        gst::Tracer::register(Some(plugin), "otel-tracer", OtelTracer::static_type())?;
        Ok(())
    },
    env!("CARGO_PKG_VERSION"),
    "MIT",
    "gst_otel",
    "gst_otel",
    "https://example.com"
);
