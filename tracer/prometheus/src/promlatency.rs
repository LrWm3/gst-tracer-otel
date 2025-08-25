use glib::prelude::*;
use gstreamer as gst;

use crate::promlatencyimp::{PromLatencyTracerImp, CAT};

mod imp {
    use super::*;
    use gst::subclass::prelude::*;
    use std::{
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
        fn update_from_params(&mut self, imp: &PromLatencyTracer, params: String) {
            let s = match gst::Structure::from_str(&format!("promlatency,{params}")) {
                Ok(s) => s,
                Err(err) => {
                    gst::warning!(CAT, imp = imp, "failed to parse tracer parameters: {}", err);
                    return;
                }
            };
            if let Ok(v) = s.get::<u32>("server-port") {
                self.server_port = v as u16;
            }
        }
    }

    #[derive(Default)]
    pub struct PromLatencyTracer {
        core: PromLatencyTracerImp,
        settings: RwLock<Settings>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PromLatencyTracer {
        const NAME: &'static str = "promlatencytracer";
        type Type = super::PromLatencyTracer;
        type ParentType = gst::Tracer;
    }

    impl ObjectImpl for PromLatencyTracer {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            let tracer_obj: &gst::Tracer = obj.upcast_ref();

            // Initialize settings with default values
            let settings = Settings::default();
            // Update settings from parameters if provided
            if let Some(params) = self.obj().property::<Option<String>>("params") {
                let mut settings = self.settings.write().unwrap();
                settings.update_from_params(self, params);
            }

            // Store settings
            {
                let mut s = self.settings.write().unwrap();
                *s = settings;
            }

            // Register all tracer hooks via the core implementation
            self.core.constructed(tracer_obj);

            // Register callback to start metrics server if needed.
            self.register_hook(TracerHook::ElementNew);
        }

        fn signals() -> &'static [glib::subclass::Signal] {
            static SIGNALS: OnceLock<Vec<glib::subclass::Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![glib::subclass::Signal::builder("request-metrics")
                    .flags(glib::SignalFlags::ACTION)
                    .return_type::<Option<String>>()
                    .class_handler(|_, _args| {
                        let ret = PromLatencyTracerImp::request_metrics();
                        gst::info!(
                            CAT,
                            "Prometheus metrics requested via signal, returning {} bytes",
                            ret.len()
                        );
                        Some(ret.to_value())
                    })
                    .accumulator(|_hint, ret, value| {
                        *ret = value.clone();
                        true
                    })
                    .build()]
            })
        }
    }

    impl GstObjectImpl for PromLatencyTracer {}

    impl TracerImpl for PromLatencyTracer {
        fn element_new(&self, ts: u64, element: &gst::Element) {
            let port = self.settings.read().unwrap().server_port;
            self.core.element_new(ts, element, port);
        }
    }
}

glib::wrapper! {
    pub struct PromLatencyTracer(ObjectSubclass<imp::PromLatencyTracer>)
        @extends gst::Tracer, gst::Object;
}

// Register the plugin with GStreamer
pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Tracer::register(
        Some(plugin),
        "prom-latency",
        PromLatencyTracer::static_type(),
    )?;
    Ok(())
}
