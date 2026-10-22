use glib::prelude::*;
use gstreamer as gst;

use crate::metricsimp::{MetricsTracerImp, CAT};

mod imp {
    use super::*;
    use gst::subclass::prelude::*;
    use std::{
        ops::ControlFlow,
        str::FromStr,
        sync::{OnceLock, RwLock},
    };

    #[derive(Debug)]
    struct Settings {
        pub server_port: u16,
    }

    impl Default for Settings {
        fn default() -> Self {
            Self {
                server_port: 8080u16,
            }
        }
    }

    impl Settings {
        fn update_from_params(&mut self, imp: &MetricsTracer, params: String) {
            let s = match gst::Structure::from_str(&format!("mars-metrics,{params}")) {
                Ok(s) => s,
                Err(err) => {
                    gst::warning!(CAT, imp = imp, "failed to parse tracer parameters: {}", err);
                    return;
                }
            };
            if let Ok(v) = s.get::<i32>("port") {
                gst::log!(CAT, imp = imp, "setting port to {}", v);
                self.server_port = v as u16;
            }
        }
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
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            let tracer_obj: &gst::Tracer = obj.upcast_ref();

            // Update settings from parameters if provided
            if let Some(params) = self.obj().property::<Option<String>>("params") {
                let mut settings = self.settings.write().unwrap();
                settings.update_from_params(self, params);
                gst::debug!(CAT, imp = self, "using settings: {:?}", *settings);
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
        fn element_add_pad(&self, _ts: u64, _element: &gst::Element, _pad: &gst::Pad) {
            MetricsTracerImp::invalidate_topology();
        }

        fn element_remove_pad(&self, _ts: u64, _element: &gst::Element, _pad: &gst::Pad) {
            MetricsTracerImp::invalidate_topology();
        }

        fn bin_add_post(&self, _ts: u64, _bin: &gst::Bin, _element: &gst::Element, success: bool) {
            if success {
                MetricsTracerImp::invalidate_topology();
            }
        }

        fn bin_remove_post(&self, _ts: u64, _bin: &gst::Bin, success: bool) {
            if success {
                MetricsTracerImp::invalidate_topology();
            }
        }

        fn element_change_state_pre(
            &self,
            _ts: u64,
            _element: &gst::Element,
            change: gst::StateChange,
        ) {
            if change == gst::StateChange::ReadyToPaused {
                // Also pick up names changed while the pipeline was stopped.
                MetricsTracerImp::invalidate_topology();
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
