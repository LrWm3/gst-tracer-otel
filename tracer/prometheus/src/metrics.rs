use glib::prelude::*;
use gstreamer as gst;

use gst_mars_metrics_core::{MetricsTracerImp, CAT};

mod imp {
    use super::*;
    use gst::subclass::prelude::*;
    use std::{
        ops::ControlFlow,
        sync::{LazyLock, OnceLock, RwLock},
    };

    use gst_mars_metrics_core::configuration::{MonitoringSetting, Overrides, Spec, Toggle};

    static SPECS: LazyLock<Vec<Spec>> = LazyLock::new(|| {
        let mut specs = gst_mars_metrics_core::settings::specs::<Toggle, MonitoringSetting>();
        specs.push(Spec::number(
            "metrics-port",
            65535,
            &["MARS_METRICS_PORT"],
            "8080",
            "Prometheus listen port; 0 inherits (default 8080)",
        ));
        specs
    });
    #[derive(Debug, Default)]
    struct Settings {
        overrides: Overrides,
        server_port: u16,
    }

    #[derive(Default)]
    pub struct MetricsTracer {
        core: MetricsTracerImp,
        settings: RwLock<Settings>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for MetricsTracer {
        const NAME: &'static str = "MarsMetricsTracer";
        type Type = super::MetricsTracer;
        type ParentType = gst::Tracer;
    }

    impl ObjectImpl for MetricsTracer {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> =
                LazyLock::new(|| SPECS.iter().map(|s| s.property.clone()).collect());
            &PROPERTIES
        }
        fn set_property(&self, _: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            self.settings
                .write()
                .unwrap()
                .overrides
                .set_property(value, pspec);
        }
        fn property(&self, _: usize, pspec: &glib::ParamSpec) -> glib::Value {
            self.settings.read().unwrap().overrides.property(pspec)
        }
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            let tracer_obj: &gst::Tracer = obj.upcast_ref();
            let configuration = (|| {
                let mut settings = self.settings.write().unwrap();
                let mut http_disabled = false;
                if let Some(params) = self
                    .obj()
                    .property::<Option<String>>("params")
                    .filter(|p| !p.trim().is_empty())
                {
                    let mut params = format!("mars-metrics,{params}")
                        .parse::<gst::Structure>()
                        .map_err(|e| format!("invalid tracer parameters: {e}"))?;
                    // Legacy port=0 disables HTTP; metrics-port=0 means inherit.
                    if params.has_field("port") {
                        if !params.has_field("metrics-port") {
                            http_disabled = params.get::<i32>("port") == Ok(0)
                                || params.get::<u32>("port") == Ok(0);
                            if !http_disabled {
                                let port = params.value("port").unwrap().clone();
                                params.set_value("metrics-port", port);
                            }
                        }
                        params.remove_field("port");
                    }
                    settings.overrides.take_parameters(&mut params, &SPECS)?;
                    if let Some((key, _)) = params.iter().next() {
                        return Err(format!("unknown tracer parameter: {key}"));
                    }
                }
                let env = |key: &str| std::env::var(key).ok();
                settings.server_port = if http_disabled {
                    0
                } else {
                    settings
                        .overrides
                        .read_named(&SPECS, "metrics-port", &env)?
                        .unwrap()
                        .parse()
                        .unwrap()
                };
                let monitoring = settings
                    .overrides
                    .read_named(&SPECS, "monitoring", &env)?
                    .unwrap()
                    .parse::<gst_mars_metrics_core::monitoring::Monitoring>()?;
                let config = gst_mars_metrics_core::settings::Config::resolve(
                    &settings.overrides,
                    &SPECS,
                    env,
                )?;
                gst_mars_metrics_core::settings::configure(config)?;
                Ok::<_, String>(monitoring)
            })();
            let monitoring = match configuration {
                Ok(level) => level,
                Err(error) => {
                    gst::error!(CAT, "Metrics tracer disabled: {error}");
                    return;
                }
            };
            gst_mars_metrics_core::monitoring::configure(monitoring);
            if monitoring != gst_mars_metrics_core::monitoring::Monitoring::None {
                gst_mars_metrics_core::enable_accounting();
            }

            // Register all tracer hooks via the core implementation
            self.core.constructed(tracer_obj);

            // Register callback to start metrics server if needed.
            self.register_hook(TracerHook::ElementNew);
            self.register_hook(TracerHook::BinAddPost);
            self.register_hook(TracerHook::BinRemovePost);
            self.register_hook(TracerHook::ElementChangeStatePre);
            self.register_hook(TracerHook::ElementAddPad);
            self.register_hook(TracerHook::ElementRemovePad);
        }

        fn signals() -> &'static [glib::subclass::Signal] {
            static SIGNALS: OnceLock<Vec<glib::subclass::Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![glib::subclass::Signal::builder("metrics")
                    .flags(glib::SignalFlags::ACTION)
                    .return_type::<Option<String>>()
                    .class_handler(|_args| {
                        let ret = MetricsTracerImp::request_metrics();
                        gst::info!(
                            CAT,
                            "Prometheus metrics requested via signal, returning {} bytes",
                            ret.len()
                        );
                        Some(ret.to_value())
                    })
                    .accumulator(|_hint, _ret, value| ControlFlow::Continue(value.clone()))
                    .build()]
            })
        }
    }

    impl GstObjectImpl for MetricsTracer {}

    impl TracerImpl for MetricsTracer {
        fn element_add_pad(&self, _ts: u64, element: &gst::Element, pad: &gst::Pad) {
            MetricsTracerImp::invalidate_object(element);
            MetricsTracerImp::invalidate_object(pad);
        }

        fn element_remove_pad(&self, _ts: u64, element: &gst::Element, pad: &gst::Pad) {
            MetricsTracerImp::invalidate_object(element);
            MetricsTracerImp::invalidate_object(pad);
        }

        fn bin_add_post(&self, _ts: u64, _bin: &gst::Bin, element: &gst::Element, success: bool) {
            if success {
                MetricsTracerImp::invalidate_object(element);
            }
        }

        fn bin_remove_post(&self, _ts: u64, bin: &gst::Bin, success: bool) {
            if success {
                MetricsTracerImp::invalidate_object(bin);
            }
        }

        fn element_change_state_pre(
            &self,
            _ts: u64,
            element: &gst::Element,
            change: gst::StateChange,
        ) {
            if change == gst::StateChange::ReadyToPaused {
                // Also pick up names changed while the pipeline was stopped.
                MetricsTracerImp::invalidate_object(element);
            }
        }

        fn element_new(&self, ts: u64, element: &gst::Element) {
            let port = self.settings.read().unwrap().server_port;
            self.core.element_new(ts, element, port);
        }
    }
}

glib::wrapper! {
    pub struct MetricsTracer(ObjectSubclass<imp::MetricsTracer>)
        @extends gst::Tracer, gst::Object;
}

// Register the plugin with GStreamer
pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Tracer::register(Some(plugin), "mars-metrics", MetricsTracer::static_type())?;
    Ok(())
}
