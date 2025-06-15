//! gst-otel-tracer – glib 0.20 | gstreamer 0.23 | OTLP 0.15

use std::sync::Arc;
use gstreamer as gst;
use gst::prelude::*;
use gst::subclass::prelude::*;
use glib::translate::{FromGlibPtrBorrow, ToGlibPtr};
use once_cell::sync::Lazy;
use dashmap::DashMap;
use opentelemetry::{
    metrics::{Histogram, Meter, MeterProvider, Unit},
    KeyValue,
};
use opentelemetry::trace::Span;
use opentelemetry::trace::Tracer;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{
    runtime::Tokio,
    trace::{self as sdktrace, Span as SdkSpan},
};
use tokio::runtime::Runtime;
use gobject_sys::GCallback;

// ───────────────── global Tokio runtime ─────────────────
static TOKIO_RT: Lazy<Runtime> = Lazy::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime")
});

// ───────────────── OpenTelemetry setup ──────────────────
fn sampling_ratio() -> f64 {
    std::env::var("OTEL_TRACES_SAMPLING_RATIO")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|&r| (0.0..=1.0).contains(&r))
        .unwrap_or(0.001) // 0.1 % default
}

static OTEL: Lazy<(sdktrace::Tracer, Meter, Histogram<f64>)> = Lazy::new(|| {
    TOKIO_RT.block_on(async {
        let tracer = opentelemetry_otlp::new_pipeline()
            .tracing()
            .with_exporter(
                opentelemetry_otlp::new_exporter()
                    .tonic()
                    .with_export_config(Default::default()),
            )
            .with_trace_config(
                sdktrace::config()
                    .with_sampler(sdktrace::Sampler::TraceIdRatioBased(sampling_ratio())),
            )
            .install_batch(Tokio)
            .unwrap();

        let meter_provider = opentelemetry_otlp::new_pipeline()
            .metrics(Tokio)
            .with_exporter(
                opentelemetry_otlp::new_exporter()
                    .tonic()
                    .with_export_config(Default::default()),
            )
            .build()
            .unwrap();

        let meter = meter_provider.meter("gst-otel-tracer");
        let hist = meter
            .f64_histogram("gstreamer.pad.latency.ns")
            .with_unit(Unit::new("ns"))
            .init();

        (tracer, meter, hist)
    })
});

// ───────────────── per-pad caches ───────────────────────
static PAD_ATTRS: Lazy<DashMap<usize, Arc<[KeyValue]>>> = Lazy::new(DashMap::new);
static PAD_SPANS: Lazy<DashMap<usize, SdkSpan>> = Lazy::new(DashMap::new);
static PAD_TS:    Lazy<DashMap<usize, u64>>     = Lazy::new(DashMap::new);

fn attrs_for(pad: &gst::Pad) -> Arc<[KeyValue]> {
    let key = pad.as_ptr() as usize;
    PAD_ATTRS
        .entry(key)
        .or_insert_with(|| {
            Arc::from([
                KeyValue::new(
                    "element",
                    pad.parent_element()
                        .map(|e| e.name().to_string())
                        .unwrap_or_default(),
                ),
                KeyValue::new("direction", format!("{:?}", pad.direction())),
            ])
        })
        .clone()
}

// ───────────────── tracer subclass ──────────────────────
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
            let tracer_obj: &gst::Tracer = self.obj().upcast_ref();

            // ---------- hook fns -----------
            unsafe extern "C" fn push_pre(
                _tr: *mut ffi::GstTracer,
                ts: u64,
                pad: *mut ffi::GstPad,
            ) {
                if pad.is_null() {
                    return;
                }
                let p = gst::Pad::from_glib_borrow(pad);
                let key = pad as usize;

                PAD_TS.insert(key, ts);

                let (tracer, _, _) = &*super::OTEL;
                let span = tracer
                    .span_builder("PadPush")
                    .with_attributes(super::attrs_for(&p).as_ref())
                    .start(tracer);

                PAD_SPANS.insert(key, span);
            }

            unsafe extern "C" fn push_post(
                _tr: *mut ffi::GstTracer,
                ts: u64,
                pad: *mut ffi::GstPad,
            ) {
                if pad.is_null() {
                    return;
                }
                let p = gst::Pad::from_glib_borrow(pad);
                let key = pad as usize;
                let (_, _, hist) = &*super::OTEL;

                // latency
                if let Some((_, start_ts)) = PAD_TS.remove(&key) {
                    let delta = ts.saturating_sub(start_ts);
                    hist.record(delta as f64, super::attrs_for(&p).as_ref());
                }

                // span end
                if let Some((_, mut span)) = PAD_SPANS.remove(&key) {
                    span.end();
                }
            }

            // ---------- register all latency-relevant hooks ----------
            unsafe {
                for &name in &[
                    b"pad-push-pre\0".as_ref(),
                    b"pad-push-list-pre\0".as_ref(),
                    b"pad-pull-range-pre\0".as_ref(),
                ] {
                    ffi::gst_tracing_register_hook(
                        tracer_obj.to_glib_none().0,
                        name.as_ptr().cast(),
                        std::mem::transmute::<_, GCallback>(push_pre as *const ()),
                    );
                }
                for &name in &[
                    b"pad-push-post\0".as_ref(),
                    b"pad-push-list-post\0".as_ref(),
                    b"pad-pull-range-post\0".as_ref(),
                ] {
                    ffi::gst_tracing_register_hook(
                        tracer_obj.to_glib_none().0,
                        name.as_ptr().cast(),
                        std::mem::transmute::<_, GCallback>(push_post as *const ()),
                    );
                }
            }
        }
    }

    impl GstObjectImpl for OtelTracer {}
    impl TracerImpl    for OtelTracer {}
}

glib::wrapper! {
    pub struct OtelTracer(ObjectSubclass<imp::OtelTracer>)
        @extends gst::Tracer, gst::Object;
}

// ───────────────── plugin boilerplate ──────────────────
fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Tracer::register(Some(plugin), "otel-tracer", OtelTracer::static_type())?;
    Ok(())
}

gst::plugin_define!(
    oteltracer,                              // → libgstoteltracer.so
    "GStreamer → OpenTelemetry tracer",
    plugin_init,
    env!("CARGO_PKG_VERSION"),
    "MIT",
    "gst_otel_tracer",
    "gst_otel_tracer",
    "https://example.com"
);
