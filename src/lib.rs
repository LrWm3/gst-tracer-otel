//! gst-otel-tracer (glib 0.20 + gstreamer 0.23 + OTLP 0.15)

use gstreamer as gst;
use gst::prelude::*;
use gst::subclass::prelude::*;
use glib::translate::{FromGlibPtrBorrow, ToGlibPtr};
use once_cell::sync::Lazy;

use opentelemetry::{
    metrics::{Histogram, Meter, MeterProvider},
    trace::{Tracer, Span as _},
    KeyValue,
};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{runtime::Tokio, trace as sdktrace};

/// ---------- OTel bootstrap ------------------------------------------------
static OTEL: Lazy<(sdktrace::Tracer, Meter, Histogram<f64>)> = Lazy::new(|| {
    // small async runtime for batch exporters
    let _rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();

    // separate exporters because TonicExporterBuilder is *not* Clone
    let exp_traces  = opentelemetry_otlp::new_exporter().tonic()
        .with_export_config(Default::default());
    let exp_metrics = opentelemetry_otlp::new_exporter().tonic()
        .with_export_config(Default::default());

    // -------- traces -------------------------------------------------------
    let tracer = opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_exporter(exp_traces)
        .install_batch(Tokio)
        .unwrap();

    // -------- metrics ------------------------------------------------------
    let meter_provider = opentelemetry_otlp::new_pipeline()
        .metrics(Tokio)            // runtime first in 0.15
        .with_exporter(exp_metrics)
        .build()
        .unwrap();

    let meter = meter_provider.meter("gst-tracer");
    let hist  = meter
        .f64_histogram("gstreamer.element.latency.ns")
        .with_unit("ns")
        .with_explicit_bucket_boundaries(vec![
             100., 200., 500., 1_000., 2_000., 5_000.,
          10_000., 20_000., 50_000.,
         100_000., 200_000., 500_000.,
       1_000_000.,  2_000_000.,  5_000_000.,
      10_000_000., 20_000_000., 50_000_000.,
     100_000_000., 200_000_000., 500_000_000.,
   1_000_000_000.,
        ])
        .init();

    (tracer, meter, hist)
});

/// ---------- tracer subclass ----------------------------------------------
mod imp {
    use super::*;
    use gstreamer as gst;
    use gst::ffi;
    use glib_sys::GCallback;          // <- glib-sys provides the alias

    #[derive(Default)]
    pub struct OtelTracer;

    #[glib::object_subclass]
    impl ObjectSubclass for OtelTracer {
        const NAME: &'static str = "OtelTracer";
        type Type       = super::OtelTracer;
        type ParentType = gst::Tracer;
    }

    // *** NOTE: on glib-0.20 the life-cycle hook is on ObjectImpl ***
    impl ObjectImpl for OtelTracer {
        fn constructed(&self) {
            self.parent_constructed();          // no arg in 0.20

            // ensure single initialisation
            let _ = &*super::OTEL;

            unsafe extern "C" fn element_latency(
                tracer_ptr: *mut ffi::GstTracer,
                element: *mut ffi::GstElement,
                time:    ffi::GstClockTime,
                _ud:     glib::ffi::gpointer,
            ) {
                if time == ffi::GST_CLOCK_TIME_NONE { return; }
                let elem = gst::Element::from_glib_borrow(element);
                let (_, _, hist) = &*super::OTEL;
                // convert GString → owned String to satisfy 'static
                hist.record(time as f64, &[KeyValue::new("element", elem.name().to_string())]);
            }

            unsafe extern "C" fn pad_push(
                tracer_ptr: *mut ffi::GstTracer,
                pad:   *mut ffi::GstPad,
                _buf:  *mut ffi::GstBuffer,
                _ud:   glib::ffi::gpointer,
            ) {
                if fastrand::u32(..1000) != 0 { return; } // 0.1 % sample

                let p = gst::Pad::from_glib_borrow(pad);
                let (tracer, _, _) = &*super::OTEL;

                let span = tracer
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
                // get a *GstTracer pointer* for the registration call
                let this: &super::OtelTracer = self       .instance();
                let gst_tracer = this.upcast_ref::<gst::Tracer>();

                ffi::gst_tracing_register_hook(
                    gst_tracer.to_glib_none().0,
                    std::ptr::null(),
                    Some(std::mem::transmute::<_, GCallback>(element_latency as *const ())),
                );
                ffi::gst_tracing_register_hook(
                    gst_tracer.to_glib_none().0,
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

/// ---------- plugin boilerplate (0.23 syntax) ------------------------------
fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Tracer::register(Some(plugin), "otel-tracer", OtelTracer::static_type())?;
    Ok(())
}

gst::plugin_define!(
    oteltracer,                                // name
    "GStreamer → OpenTelemetry tracer",        // description
    plugin_init,                               // init fn ident
    env!("CARGO_PKG_VERSION"),                 // version
    "MIT", "gst_otel_tracer", "gst_otel_tracer", "https://example.com"
);
