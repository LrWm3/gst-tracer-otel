use glib::{subclass::prelude::*, translate::IntoGlib};
use gstreamer::{self as gst, prelude::*, subclass::prelude::*};
use std::sync::{LazyLock, OnceLock};

use crate::{
    capture,
    export::{self, Config, FlushError, Runtime},
    metadata,
};

static RUNTIME: OnceLock<Result<Runtime, String>> = OnceLock::new();
static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "otel-tracer",
        gst::DebugColorFlags::empty(),
        Some("Bounded OTLP push tracing"),
    )
});

pub(crate) fn runtime() -> Result<&'static Runtime, FlushError> {
    RUNTIME
        .get()
        .and_then(|r| r.as_ref().ok())
        .ok_or(FlushError::NotInitialized)
}

fn enabled(key: &str, default: bool) -> Result<bool, String> {
    match std::env::var(key).as_deref() {
        Err(_) => Ok(default),
        Ok("true" | "1") => Ok(true),
        Ok("false" | "0") => Ok(false),
        _ => Err(format!("{key} must be true or false")),
    }
}

fn initialize(instrument: bool) -> Result<Runtime, String> {
    let mut config = Config::from_env()?;
    let metrics = instrument && enabled("GST_INSTRUMENT_METRICS", true)?;
    let profiles = !instrument || enabled("GST_INSTRUMENT_PROFILES", true)?;
    if instrument {
        config.traces = enabled("GST_INSTRUMENT_TRACES", true)?;
        config.logs = enabled("GST_INSTRUMENT_LOGS", config.logs)?;
    }
    let resource = export::resource();
    let exporter = if config.traces {
        Some(crate::transport::OtlpExporter::new(&resource)?)
    } else {
        None
    };
    let metrics = if metrics {
        Some(crate::metrics::Metrics::new(
            crate::metrics::Config::from_env()?,
            &resource,
        )?)
    } else {
        None
    };
    if config.logs {
        crate::otellogbridge::initialize(resource.clone())?;
    }
    let mut runtime = Runtime::new(config, export::OptionalExporter(exporter), resource.clone())
        .map_err(|e| e.to_string())?;
    runtime.metrics = metrics;
    if profiles {
        if let Err(error) = crate::profiles::initialize(&resource) {
            gst::warning!(CAT, "Embedded profiles disabled: {error}");
        }
    }
    Ok(runtime)
}

static REGISTERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

// Both entry points use exactly the same hooks and capture stack. The first
// constructed tracer selects the process-wide signal configuration.
macro_rules! tracer_impl {
($module:ident, $wrapper:ident, $name:literal, $instrument:literal) => {
mod $module {
    use super::*;

    #[derive(Default)]
    pub struct OtelTracerImpl;

    #[glib::object_subclass]
    impl ObjectSubclass for OtelTracerImpl {
        const NAME: &'static str = $name;
        type Type = super::$wrapper;
        type ParentType = gst::Tracer;
    }

    impl ObjectImpl for OtelTracerImpl {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
                vec![glib::ParamSpecBoxed::builder::<gst::Structure>("stats")
                    .read_only()
                    .build(), glib::ParamSpecBoxed::builder::<gst::Structure>("metric-stats").read_only().build()]
            });
            PROPERTIES.as_ref()
        }
        fn property(&self, _: usize, pspec: &glib::ParamSpec) -> glib::Value {
            match pspec.name() {
                "stats" => runtime()
                    .ok()
                    .map(|r| r.stats.snapshot().to_structure())
                    .to_value(),
                "metric-stats" => runtime().ok().map(|r| r.metrics.as_ref().map_or_else(crate::metrics::MetricStats::default, |m| m.stats()).to_structure()).to_value(),
                _ => unreachable!(),
            }
        }

        fn constructed(&self) {
            self.parent_constructed();
            if let Err(error) = RUNTIME.get_or_init(|| initialize($instrument)) {
                gst::error!(CAT, "OTel tracer disabled: {error}");
                return;
            }
            // Multiple tracer instances would otherwise duplicate hooks and TLS frames.
            if REGISTERED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                gst::warning!(CAT, "A telemetry tracer is already active; retaining its signal configuration");
                return;
            }
            for hook in [
                TracerHook::PadPushPre,
                TracerHook::PadPushPost,
                TracerHook::PadLinkPost,
                TracerHook::PadUnlinkPost,
                TracerHook::ElementAddPad,
                TracerHook::ElementRemovePad,
                TracerHook::BinAddPost,
                TracerHook::BinRemovePost,
                TracerHook::ElementChangeStatePost,
            ] {
                self.register_hook(hook);
            }
        }
    }
    impl GstObjectImpl for OtelTracerImpl {}
    impl TracerImpl for OtelTracerImpl {
        fn pad_push_pre(&self, ts: u64, pad: &gst::Pad, buffer: &gst::Buffer) {
            if let Ok(runtime) = runtime() {
                capture::begin(runtime, ts, pad, buffer);
            }
        }
        fn pad_push_post(
            &self,
            ts: u64,
            pad: &gst::Pad,
            result: Result<gst::FlowSuccess, gst::FlowError>,
        ) {
            let flow = match result {
                Ok(v) => v.into_glib(),
                Err(v) => v.into_glib(),
            };
            if let Ok(runtime) = runtime() {
                capture::end(runtime, ts, pad, flow);
            }
        }
        fn pad_link_post(
            &self,
            _: u64,
            src: &gst::Pad,
            sink: &gst::Pad,
            _: Result<gst::PadLinkSuccess, gst::PadLinkError>,
        ) {
            metadata::invalidate(src);
            metadata::invalidate(sink);
        }
        fn pad_unlink_post(&self, _: u64, src: &gst::Pad, sink: &gst::Pad, _: bool) {
            metadata::invalidate(src);
            metadata::invalidate(sink);
        }
        fn element_add_pad(&self, _: u64, element: &gst::Element, pad: &gst::Pad) {
            metadata::invalidate(element);
            metadata::invalidate(pad);
        }
        fn element_remove_pad(&self, _: u64, element: &gst::Element, pad: &gst::Pad) {
            metadata::invalidate(element);
            metadata::invalidate(pad);
        }
        fn bin_add_post(&self, _: u64, _: &gst::Bin, element: &gst::Element, _: bool) {
            metadata::invalidate(element);
        }
        fn bin_remove_post(&self, _: u64, bin: &gst::Bin, _: bool) {
            // Old weak subscriptions also cover the subtree just removed.
            metadata::invalidate(bin);
        }
        fn element_change_state_post(
            &self,
            _: u64,
            element: &gst::Element,
            change: gst::StateChange,
            result: Result<gst::StateChangeSuccess, gst::StateChangeError>,
        ) {
            if element.is::<gst::Pipeline>()
                && change == gst::StateChange::ReadyToNull
                && result.is_ok()
            {
                if let Ok(runtime) = runtime() {
                    if let Err(error) = runtime.flush(runtime.config.flush_timeout) {
                        gst::warning!(
                            CAT,
                            "Telemetry drain failed: {error}; counters: {:?}",
                            runtime.stats.snapshot()
                        );
                    }
                    crate::otellogbridge::flush();
                }
            }
        }
    }
}

glib::wrapper! {
    /// Process-wide telemetry tracer. The first instance selects signal settings.
    pub struct $wrapper(ObjectSubclass<$module::OtelTracerImpl>)
        @extends gst::Tracer, gst::Object;
}
};
}
tracer_impl!(imp, TelemetryTracer, "GstTelemetryTracer", false);
tracer_impl!(
    instrument_imp,
    InstrumentTracer,
    "GstInstrumentTracer",
    true
);

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Tracer::register(Some(plugin), "otel-tracer", TelemetryTracer::static_type())?;
    gst::Tracer::register(
        Some(plugin),
        "gst-instrument",
        InstrumentTracer::static_type(),
    )
}
