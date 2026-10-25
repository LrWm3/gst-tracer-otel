use glib::{subclass::prelude::*, translate::IntoGlib};
use gstreamer::{self as gst, prelude::*, subclass::prelude::*};
use std::sync::{LazyLock, OnceLock};

use crate::{
    capture,
    export::{self, Config, FlushError, Runtime},
    metadata,
};

static RUNTIME: OnceLock<Result<Runtime, String>> = OnceLock::new();
pub(crate) static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "mars-otel",
        gst::DebugColorFlags::empty(),
        Some("Bounded OTLP push/pull tracing"),
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Traces,
    Instrument,
    Profiles,
}

fn initialize(mode: Mode, settings: Option<crate::settings::Settings>) -> Result<Runtime, String> {
    if let Some(settings) = settings {
        return initialize_explicit(settings);
    }
    let profiles_only = mode == Mode::Profiles;
    let instrument = mode == Mode::Instrument;
    // Profiles-only operation must not depend on trace/metric/log settings or
    // allocate their exporters, queues, capture stack, or metadata caches.
    let mut config = if profiles_only {
        Config {
            traces: false,
            ..Config::default()
        }
    } else {
        Config::from_env()?
    };
    let metrics = instrument && enabled("GST_INSTRUMENT_METRICS", true)?;
    crate::health::configure(if metrics {
        std::env::var("MARS_MONITORING")
            .unwrap_or_else(|_| "core".into())
            .parse()?
    } else {
        gst_mars_metrics_core::monitoring::Monitoring::None
    });
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
        if let Err(error) = crate::profiles::initialize(&resource, profiles_only) {
            if profiles_only {
                return Err(error);
            }
            gst::warning!(CAT, "Embedded profiles disabled: {error}");
        }
    }
    Ok(runtime)
}

fn initialize_explicit(settings: crate::settings::Settings) -> Result<Runtime, String> {
    crate::health::configure(settings.monitoring);
    #[cfg(not(all(
        feature = "profiles",
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )))]
    if settings.profiles {
        return Err(
            "profiles require the profiles Cargo feature on Linux x86-64 or AArch64".into(),
        );
    }
    let mut config = if settings.traces {
        Config::from_env()?
    } else {
        Config {
            traces: false,
            ..Config::default()
        }
    };
    config.logs = settings.logs;
    if !settings.traces {
        if let Ok(value) = std::env::var("GST_OTEL_FLUSH_TIMEOUT_MS") {
            let ms = value
                .parse::<u64>()
                .map_err(|_| "invalid GST_OTEL_FLUSH_TIMEOUT_MS")?;
            if !(1..=60_000).contains(&ms) {
                return Err("GST_OTEL_FLUSH_TIMEOUT_MS must be 1..=60000".into());
            }
            config.flush_timeout = std::time::Duration::from_millis(ms);
        }
    }
    let resource = export::resource();
    let exporter = if settings.traces {
        Some(crate::transport::OtlpExporter::new(&resource)?)
    } else {
        None
    };
    let metrics = if settings.metrics {
        Some(crate::shared_metrics::Metrics::new(&settings, &resource)?)
    } else {
        None
    };
    if settings.logs {
        crate::otellogbridge::initialize(resource.clone())?;
    }
    let mut runtime = Runtime::new(config, export::OptionalExporter(exporter), resource.clone())
        .map_err(|e| e.to_string())?;
    runtime.shared_metrics = metrics;
    if settings.profiles {
        crate::profiles::initialize(&resource, true)?;
    }
    if settings.metrics && (settings.traces || settings.profiles) {
        gst_mars_metrics_core::set_capture_hooks(gst_mars_metrics_core::CaptureHooks {
            exemplars: settings.traces,
            begin: capture_begin,
            pull_begin: capture_pull_begin,
            pull_end: |ts, pad, result, buffer| {
                let token = capture_pull_end(ts, pad, result, buffer)?;
                let runtime = crate::tracer::runtime().ok()?;
                Some(gst_mars_metrics_core::exemplars::Exemplar {
                    trace_id: token.trace_id.to_bytes(),
                    span_id: token.span_id.to_bytes(),
                    timestamp_ns: runtime.timestamp(ts),
                    value: 0.0,
                })
            },
            end: |ts, pad, result| {
                let runtime = crate::tracer::runtime().ok()?;
                if !runtime.config.traces {
                    return None;
                }
                let token = capture::end(runtime, ts, pad, result)?;
                Some(gst_mars_metrics_core::exemplars::Exemplar {
                    trace_id: token.trace_id.to_bytes(),
                    span_id: token.span_id.to_bytes(),
                    timestamp_ns: runtime.timestamp(ts),
                    value: 0.0,
                })
            },
        })?;
    }
    Ok(runtime)
}

fn capture_begin(ts: u64, pad: &gst::Pad, buffer: &gst::Buffer) {
    if let Ok(runtime) = runtime() {
        if runtime.config.traces || runtime.metrics.is_some() {
            capture::begin(runtime, ts, pad, buffer);
        } else {
            #[cfg(feature = "profiles")]
            crate::profiles::register_thread();
        }
    }
}

fn capture_pull_begin(ts: u64, pad: &gst::Pad) {
    #[cfg(feature = "profiles")]
    {
        crate::profiles::register_thread();
        crate::profilecontext::begin(pad, 2);
    }
    if let Ok(runtime) = runtime() {
        if runtime.config.traces {
            capture::begin_pull(runtime, ts, pad);
        }
    }
}

fn capture_pull_end(
    ts: u64,
    pad: &gst::Pad,
    result: i32,
    buffer: Option<&gst::Buffer>,
) -> Option<capture::Token> {
    let token = runtime()
        .ok()
        .filter(|r| r.config.traces)
        .and_then(|r| capture::end_pull(r, ts, pad, result, buffer));
    #[cfg(feature = "profiles")]
    crate::profilecontext::end(pad, 2);
    token
}

static REGISTERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn invalidate_metrics() {
    if runtime().is_ok_and(|r| r.shared_metrics.is_some()) {
        gst_mars_metrics_core::MetricsTracerImp::invalidate_topology();
    }
}

#[cfg(feature = "profiles")]
pub(crate) fn activate(mode: Mode) -> bool {
    activate_with_params(mode, None)
}

fn activate_with_params(mode: Mode, params: Option<&str>) -> bool {
    let settings = if mode == Mode::Instrument {
        match crate::settings::Settings::parse(params, |key| std::env::var(key).ok()) {
            Ok(settings) => settings,
            Err(error) => {
                gst::error!(CAT, "Telemetry tracer disabled: {error}");
                return false;
            }
        }
    } else {
        None
    };
    // Claim ownership before initialization: another mode must not win hook
    // registration while this mode is still creating its runtime.
    if REGISTERED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        gst::warning!(
            CAT,
            "A telemetry tracer is already active; retaining its signal configuration"
        );
        return false;
    }
    if let Err(error) = RUNTIME.get_or_init(|| {
        let result = initialize(mode, settings);
        if result.is_err() {
            // Initialization can fail after another selected signal started.
            // Do not leave its global log/profile worker running unattended.
            let timeout = std::time::Duration::from_secs(1);
            let start = std::time::Instant::now();
            let _ = crate::otellogbridge::shutdown(timeout);
            let _ = crate::profiles::shutdown(timeout.saturating_sub(start.elapsed()));
        }
        result
    }) {
        gst::error!(CAT, "Telemetry tracer disabled: {error}");
        return false;
    }
    true
}

// The tracing entry points share hooks and the capture stack. An explicit
// profiles-only configuration, like profiletracer.rs, bypasses span capture.
macro_rules! tracer_impl {
    ($module:ident, $wrapper:ident, $name:literal, $mode:expr) => {
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
                fn signals() -> &'static [glib::subclass::Signal] {
                    static SIGNALS: LazyLock<Vec<glib::subclass::Signal>> =
                        LazyLock::new(crate::lifecycle::signals);
                    &SIGNALS
                }
                fn properties() -> &'static [glib::ParamSpec] {
                    static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
                        vec![
                            glib::ParamSpecBoxed::builder::<gst::Structure>("stats")
                                .read_only()
                                .build(),
                            glib::ParamSpecBoxed::builder::<gst::Structure>("metric-stats")
                                .read_only()
                                .build(),
                        ]
                    });
                    PROPERTIES.as_ref()
                }
                fn property(&self, _: usize, pspec: &glib::ParamSpec) -> glib::Value {
                    match pspec.name() {
                        "stats" => runtime()
                            .ok()
                            .map(|r| r.stats.snapshot().to_structure())
                            .to_value(),
                        "metric-stats" => runtime()
                            .ok()
                            .map(|r| r.metric_stats().to_structure())
                            .to_value(),
                        _ => unreachable!(),
                    }
                }

                fn constructed(&self) {
                    self.parent_constructed();
                    let params = self.obj().property::<Option<String>>("params");
                    if !activate_with_params($mode, params.as_deref()) {
                        return;
                    }
                    let runtime = runtime().unwrap();
                    if runtime.shared_metrics.is_some() {
                        gst_mars_metrics_core::MetricsTracerImp
                            .constructed(self.obj().upcast_ref());
                        self.register_hook(TracerHook::ElementNew);
                        self.register_hook(TracerHook::ElementChangeStatePre);
                    } else if runtime.config.traces || runtime.metrics.is_some() {
                        self.register_hook(TracerHook::PadPushPre);
                        self.register_hook(TracerHook::PadPushPost);
                    } else {
                        #[cfg(feature = "profiles")]
                        if crate::profiles::active() {
                            self.register_hook(TracerHook::PadPushPre);
                        }
                    }
                    let pull_hooks = runtime.config.traces;
                    #[cfg(feature = "profiles")]
                    let pull_hooks = pull_hooks || crate::profiles::active();
                    // Shared metrics invoke capture around their own timing.
                    // Registering a second typed pull hook would duplicate spans.
                    if runtime.shared_metrics.is_none() && pull_hooks {
                        self.register_hook(TracerHook::PadPullRangePre);
                        self.register_hook(TracerHook::PadPullRangePost);
                    }
                    #[cfg(feature = "profiles")]
                    if crate::profiles::active() {
                        self.register_hook(TracerHook::PadPushListPre);
                    }
                    for hook in [
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
                    #[cfg(feature = "profiles")]
                    crate::profilecontext::register(self);
                }
            }
            impl GstObjectImpl for OtelTracerImpl {}
            impl TracerImpl for OtelTracerImpl {
                #[cfg(feature = "profiles")]
                crate::profilecontext::methods!();
                fn pad_push_pre(&self, ts: u64, pad: &gst::Pad, buffer: &gst::Buffer) {
                    capture_begin(ts, pad, buffer);
                }
                fn pad_pull_range_pre(&self, ts: u64, pad: &gst::Pad, _: u64, _: u32) {
                    capture_pull_begin(ts, pad);
                }
                fn pad_pull_range_post(
                    &self,
                    ts: u64,
                    pad: &gst::Pad,
                    result: Result<&gst::Buffer, gst::FlowError>,
                ) {
                    let (flow, buffer) = match result {
                        Ok(buffer) => (gst::ffi::GST_FLOW_OK, Some(buffer)),
                        Err(error) => (error.into_glib(), None),
                    };
                    capture_pull_end(ts, pad, flow, buffer);
                }
                fn pad_push_list_pre(&self, _: u64, _: &gst::Pad, _: &gst::BufferList) {
                    #[cfg(feature = "profiles")]
                    crate::profiles::register_thread();
                }
                fn element_new(&self, ts: u64, element: &gst::Element) {
                    gst_mars_metrics_core::MetricsTracerImp.element_new(ts, element, 0);
                }
                fn element_change_state_pre(
                    &self,
                    _: u64,
                    _: &gst::Element,
                    change: gst::StateChange,
                ) {
                    if change == gst::StateChange::ReadyToPaused {
                        invalidate_metrics();
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
                    invalidate_metrics();
                    metadata::invalidate(src);
                    metadata::invalidate(sink);
                }
                fn pad_unlink_post(&self, _: u64, src: &gst::Pad, sink: &gst::Pad, _: bool) {
                    invalidate_metrics();
                    metadata::invalidate(src);
                    metadata::invalidate(sink);
                }
                fn element_add_pad(&self, _: u64, element: &gst::Element, pad: &gst::Pad) {
                    invalidate_metrics();
                    metadata::invalidate(element);
                    metadata::invalidate(pad);
                }
                fn element_remove_pad(&self, _: u64, element: &gst::Element, pad: &gst::Pad) {
                    invalidate_metrics();
                    metadata::invalidate(element);
                    metadata::invalidate(pad);
                }
                fn bin_add_post(&self, _: u64, _: &gst::Bin, element: &gst::Element, _: bool) {
                    invalidate_metrics();
                    metadata::invalidate(element);
                }
                fn bin_remove_post(&self, _: u64, bin: &gst::Bin, _: bool) {
                    invalidate_metrics();
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
                            gst::debug!(
                                CAT,
                                "Draining telemetry; counters: {:?}",
                                runtime.stats.snapshot()
                            );
                            let start = std::time::Instant::now();
                            let (traces, metrics) =
                                runtime.flush_parts(runtime.config.flush_timeout);
                            if let Err(error) =
                                traces.as_ref().map(|_| ()).and(metrics.as_ref().copied())
                            {
                                gst::warning!(
                                    CAT,
                                    "Telemetry drain failed: {error}; counters: {:?}",
                                    runtime.stats.snapshot()
                                );
                            }
                            gst::debug!(
                                CAT,
                                "Trace/metric drain took {:?}; counters: {:?}",
                                start.elapsed(),
                                runtime.stats.snapshot()
                            );
                            let remaining =
                                runtime.config.flush_timeout.saturating_sub(start.elapsed());
                            let log_start = std::time::Instant::now();
                            let logs = crate::otellogbridge::flush(remaining);
                            if let Err(error) = &logs {
                                gst::warning!(
                                    CAT,
                                    "Log drain failed: {error}; counters: {:?}",
                                    crate::log_stats()
                                );
                            }
                            gst::debug!(CAT, "Log drain took {:?}", log_start.elapsed());
                            crate::health::cleanup(&crate::DrainReport {
                                traces,
                                metrics,
                                logs,
                                profiles: None,
                            });
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
tracer_impl!(imp, TelemetryTracer, "MarsOtelTracer", Mode::Traces);
tracer_impl!(
    instrument_imp,
    InstrumentTracer,
    "MarsInstrumentTracer",
    Mode::Instrument
);

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Tracer::register(Some(plugin), "mars-otel", TelemetryTracer::static_type())?;
    gst::Tracer::register(
        Some(plugin),
        "mars-instrument",
        InstrumentTracer::static_type(),
    )
}
