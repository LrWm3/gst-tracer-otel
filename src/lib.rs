//! GStreamer tracer that exports element latency to OpenTelemetry
//!   * metric: histogram gstreamer.element.latency.ns   (100 ns … 1 s)
//!   * trace:  sampled PadPush spans (1 / 1000)

use once_cell::sync::Lazy;
use std::time::Duration;

// ── bring GStreamer in as `gst` so the old snippet compiles verbatim
use gstreamer as gst;
use gst::prelude::*;
use gst::subclass::prelude::*;

use opentelemetry::{
    metrics::{Histogram, Meter},
    trace::{Tracer, Span},
    KeyValue,
};
use opentelemetry_otlp::WithExportConfig;

// ╭─────────────────────── OTel bootstrap ───────────────────────╮
static OTEL: Lazy<(Tracer, Meter, Histogram<f64>)> = Lazy::new(|| {
    // Batch exporter needs an async runtime → spin a tiny Tokio RT
    let _rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");

    let exporter = opentelemetry_otlp::new_exporter()
        .tonic()
        .with_env();           // honours OTEL_EXPORTER_OTLP_* env vars

    let provider = opentelemetry_otlp::new_pipeline()
        .tracing(|t| t.with_exporter(exporter.clone()))
        .metrics(|m| m
            .with_exporter(exporter)
            .with_timeout(Duration::from_secs(3)))
        .install_batch(opentelemetry_sdk::runtime::Tokio)
        .expect("install OTel");

    let meter = provider.meter("gst-otel-tracer");
    let hist = meter
        .f64_histogram("gstreamer.element.latency.ns")
        .with_unit("ns")
        .with_description("Per-element latency (nanoseconds)")
        .with_explicit_bucket_boundaries(vec![
            100., 200., 500., 1_000., 2_000., 5_000.,
            10_000., 20_000., 50_000.,
            100_000., 200_000., 500_000.,
            1_000_000., 2_000_000., 5_000_000.,
            10_000_000., 20_000_000., 50_000_000.,
            100_000_000., 200_000_000., 500_000_000.,
            1_000_000_000.])        // … 1 s
        .init();

    let tracer = provider.versioned_tracer("gst-otel", Some(env!("CARGO_PKG_VERSION")), None);

    (tracer, meter, hist)
});
// ╰───────────────────────────────────────────────────────────────╯

// ───────────────────── tracer subclass ──────────────────────────
mod imp {
    use super::*;
    use gst::ffi;

    #[derive(Default)]
    pub struct OtelTracer;

    #[glib::object_subclass]
    impl ObjectSubclass for OtelTracer {
        const NAME: &'static str = "OtelTracer";
        type Type        = super::OtelTracer;
        type ParentType  = gst::Tracer;
    }

    impl ObjectImpl    for OtelTracer {}
    impl GstObjectImpl for OtelTracer {}
    impl TracerImpl    for OtelTracer {
        fn constructed(&self, tracer: &Self::Type) {
            self.parent_constructed(tracer);

            // touch the Lazy so the exporter starts exactly once
            let (_, _, _) = &*super::OTEL;

            unsafe {
                // ── ELEMENT_LATENCY → histogram (with exemplars) ─────
                extern "C" fn element_latency(
                    _tracer: *mut ffi::GstTracer,
                    element: *mut ffi::GstElement,
                    time: ffi::GstClockTime,
                    _ud: glib::ffi::gpointer,
                ) {
                    if time == ffi::GST_CLOCK_TIME_NONE { return; }
                    let elem = gst::Element::from_glib_borrow(element);
                    let (_, _, hist) = &*super::OTEL;
                    hist.record(time as f64, &[KeyValue::new("element", elem.name())]);
                }

                // ── PAD_PUSH sampling (1/1000) → child span ──────────
                extern "C" fn pad_push(
                    _tracer: *mut ffi::GstTracer,
                    pad: *mut ffi::GstPad,
                    _buf: *mut ffi::GstBuffer,
                    _ud: glib::ffi::gpointer,
                ) {
                    if fastrand::u32(..1000) != 0 { return; } // head sampler

                    let p = gst::Pad::from_glib_borrow(pad);
                    let (otel_tracer, _, _) = &*super::OTEL;

                    let span = otel_tracer
                        .span_builder("PadPush")
                        .with_attributes(&[
                            KeyValue::new("element", p.parent_element().map(|e| e.name()).unwrap_or_default()),
                            KeyValue::new("direction", format!("{:?}", p.direction())),
                        ])
                        .start(otel_tracer);

                    span.end();
                }

                // register hooks
                ffi::gst_tracing_register_hook(
                    tracer.to_glib_none().0,
                    std::ptr::null(),
                    Some(std::mem::transmute::<_, ffi::GCallback>(element_latency as *const ())),
                );
                ffi::gst_tracing_register_hook(
                    tracer.to_glib_none().0,
                    std::ptr::null(),
                    Some(std::mem::transmute::<_, ffi::GCallback>(pad_push as *const ())),
                );
            }
        }
    }
}

glib::wrapper! {
    pub struct OtelTracer(ObjectSubclass<imp::OtelTracer>)
        @extends gst::Tracer, gst::Object;
}

// ─────────────────── plugin boilerplate ──────────────────────────
gst::plugin_define!(
    gst_otel,
    "GStreamer → OpenTelemetry tracer",
    plugin_init,
    env!("CARGO_PKG_VERSION"),
    "MIT",
    "gst_otel_tracer",
    "gst_otel_tracer",
    "https://example.com"
);

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Tracer::register(Some(plugin), "otel-tracer", OtelTracer::static_type())?;
    Ok(())
}
