use once_cell::sync::Lazy;
use std::{time::Duration};

use glib::subclass::prelude::*;
use gst::subclass::prelude::*;
use gst::{glib, prelude::*};

use opentelemetry::{
    metrics::{Histogram, Meter},
    trace::{Span, Tracer},
    KeyValue, Context,
};
use opentelemetry_otlp::WithExportConfig;

// ──────────────── OpenTelemetry bootstrap ────────────────
static OTEL: Lazy<(Tracer, Meter, Histogram<f64>)> = Lazy::new(|| {
    // 1) Tokio runtime for the exporter.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio");

    // 2) OTLP exporter obeys OTEL_EXPORTER_OTLP_ENDPOINT et al.
    let exporter = opentelemetry_otlp::new_exporter()
        .tonic()
        .with_env();

    let provider = opentelemetry_otlp::new_pipeline()
        .metrics(|m| m.with_exporter(exporter.clone())
                      .with_timeout(Duration::from_secs(3)))
        .tracing(|t| t.with_exporter(exporter))
        .install_batch(opentelemetry_sdk::runtime::Tokio)
        .expect("otel pipeline");

    // 3) Meter + histogram in raw nanoseconds (starts at 100 ns).
    let meter = provider.meter("gst-otel-tracer");
    let hist = meter
        .f64_histogram("gstreamer.element.latency.ns")
        .with_description("Per-element latency in nanoseconds")
        .with_unit("ns")
        .with_explicit_bucket_boundaries(vec![
           100., 200., 500., 1_000., 2_000., 5_000., 10_000., 20_000., 50_000.,
           100_000., 200_000., 500_000., 1_000_000., 2_000_000., 5_000_000.,
           10_000_000., 20_000_000., 50_000_000., 100_000_000.,
           200_000_000., 500_000_000., 1_000_000_000.])
        .init();

    let tracer = provider.versioned_tracer(
        "gst-otel",
        Some(env!("CARGO_PKG_VERSION")),
        None);

    (tracer, meter, hist)
});

// ──────────────── Tracer GObject subclass ────────────────
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
    impl ObjectImpl    for OtelTracer {}
    impl GstObjectImpl for OtelTracer {}

    impl TracerImpl for OtelTracer {
        fn constructed(&self, tracer: &Self::Type) {
            self.parent_constructed(tracer);

            // Ensure OTEL static is initialised
            let (otel_tracer, _, histogram) = &*super::OTEL;

            unsafe {
                // ── 1. ELEMENT_LATENCY ────────────────────────────────
                extern "C" fn element_latency_hook(
                    _tracer: *mut ffi::GstTracer,
                    element: *mut ffi::GstElement,
                    time: ffi::GstClockTime,
                    _ud: glib::ffi::gpointer,
                ) {
                    if time == ffi::GST_CLOCK_TIME_NONE { return; }
                    let elem = gst::Element::from_glib_borrow(element);
                    let (_, _, hist) = &*super::OTEL;
                    let attrs = [KeyValue::new("element", elem.name())];

                    // Use the current span context so OTLP exporter can attach exemplars
                    hist.record(time as f64, &attrs);
                }

                // ── 2. PAD_PUSH  (sampled 1 out of 1000) ─────────────
                extern "C" fn pad_push_hook(
                    _tracer: *mut ffi::GstTracer,
                    pad: *mut ffi::GstPad,
                    _buffer: *mut ffi::GstBuffer,
                    _ud: glib::ffi::gpointer,
                ) {
                    let (otel_tracer, _, _) = &*super::OTEL;
                    // Simple head-based sampler
                    if fastrand::u32(..1000) != 0 { return; }

                    let p = gst::Pad::from_glib_borrow(pad);
                    let attrs = [
                        KeyValue::new("pad.direction", format!("{:?}", p.direction())),
                        KeyValue::new("element", p.parent_element().map(|e| e.name()).unwrap_or_default()),
                    ];
                    let span = otel_tracer
                        .span_builder("PadPush")
                        .with_attributes(attrs)
                        .start(otel_tracer);

                    span.end();
                }

                // register hooks
                ffi::gst_tracing_register_hook(
                    tracer.to_glib_none().0,
                    std::ptr::null(),
                    Some(std::mem::transmute::<_, ffi::GCallback>(
                        element_latency_hook as *const ())),
                );
                ffi::gst_tracing_register_hook(
                    tracer.to_glib_none().0,
                    std::ptr::null(),
                    Some(std::mem::transmute::<_, ffi::GCallback>(
                        pad_push_hook as *const ())),
                );
            }
        }
    }
}

glib::wrapper! {
    pub struct OtelTracer(ObjectSubclass<imp::OtelTracer>)
        @extends gst::Tracer, gst::Object;
}

// ──────────────── Plugin boilerplate ────────────────
gst::plugin_define!(
    gst_otel,
    "GStreamer OpenTelemetry tracer",
    plugin_init,
    env!("CARGO_PKG_VERSION"),
    "MIT",
    "gst_otel_tracer",
    "gst_otel_tracer",
    "https://example.com"
);

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Tracer::register(
        Some(plugin),
        "otel-tracer",
        OtelTracer::static_type(),
    )?;
    Ok(())
}
