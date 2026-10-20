use glib::{subclass::prelude::*, translate::IntoGlib};
use gstreamer::{self as gst, prelude::*, subclass::prelude::*};
use std::sync::{LazyLock, OnceLock};

use crate::{
    capture,
    export::{self, Config, FlushError, Runtime},
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

fn initialize() -> Result<Runtime, String> {
    let config = Config::from_env()?;
    let resource = export::resource();
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .build()
        .map_err(|e| e.to_string())?;
    if config.logs {
        crate::otellogbridge::initialize(resource.clone())?;
    }
    Runtime::new(config, exporter, resource).map_err(|e| e.to_string())
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct OtelTracerImpl;

    #[glib::object_subclass]
    impl ObjectSubclass for OtelTracerImpl {
        const NAME: &'static str = "GstTelemetryTracer";
        type Type = super::TelemetryTracer;
        type ParentType = gst::Tracer;
    }

    impl ObjectImpl for OtelTracerImpl {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
                vec![glib::ParamSpecBoxed::builder::<gst::Structure>("stats")
                    .read_only()
                    .build()]
            });
            PROPERTIES.as_ref()
        }
        fn property(&self, _: usize, pspec: &glib::ParamSpec) -> glib::Value {
            match pspec.name() {
                "stats" => runtime()
                    .ok()
                    .map(|r| r.stats.snapshot().to_structure())
                    .to_value(),
                _ => unreachable!(),
            }
        }

        fn constructed(&self) {
            self.parent_constructed();
            if let Err(error) = RUNTIME.get_or_init(initialize) {
                gst::error!(CAT, "OTel tracer disabled: {error}");
                return;
            }
            // Multiple tracer instances would otherwise duplicate hooks and TLS frames.
            static REGISTERED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if REGISTERED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            for hook in [
                TracerHook::PadPushPre,
                TracerHook::PadPushPost,
                TracerHook::PadLinkPost,
                TracerHook::PadUnlinkPost,
                TracerHook::ElementNew,
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
            _: &gst::Pad,
            _: &gst::Pad,
            _: Result<gst::PadLinkSuccess, gst::PadLinkError>,
        ) {
            capture::invalidate();
        }
        fn pad_unlink_post(&self, _: u64, _: &gst::Pad, _: &gst::Pad, _: bool) {
            capture::invalidate();
        }
        fn element_new(&self, _: u64, element: &gst::Element) {
            element.connect_notify(Some("name"), |_, _| capture::invalidate());
            capture::invalidate();
        }
        fn element_add_pad(&self, _: u64, _: &gst::Element, _: &gst::Pad) {
            capture::invalidate();
        }
        fn element_remove_pad(&self, _: u64, _: &gst::Element, _: &gst::Pad) {
            capture::invalidate();
        }
        fn bin_add_post(&self, _: u64, _: &gst::Bin, _: &gst::Element, _: bool) {
            capture::invalidate();
        }
        fn bin_remove_post(&self, _: u64, _: &gst::Bin, _: bool) {
            capture::invalidate();
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
                            "Trace drain failed: {error}; counters: {:?}",
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
    /// Process-wide push tracer. Normally created by `GST_TRACERS=otel-tracer`.
    pub struct TelemetryTracer(ObjectSubclass<imp::OtelTracerImpl>)
        @extends gst::Tracer, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Tracer::register(Some(plugin), "otel-tracer", TelemetryTracer::static_type())
}
