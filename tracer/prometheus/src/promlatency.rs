use glib::prelude::*;
use gstreamer as gst;

use crate::promlatencyimp::{PromLatencyTracerImp, CAT};

mod imp {
    use super::*;
    use glib::{ParamSpec, ParamSpecUInt, Value};
    use gst::subclass::prelude::*;
    use std::sync::{OnceLock, RwLock};

    #[derive(Default)]
    pub struct PromLatencyTracer {
        pub core: PromLatencyTracerImp,
        pub metrics_port: RwLock<u16>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PromLatencyTracer {
        const NAME: &'static str = "promlatencytracer";
        type Type = super::PromLatencyTracer;
        type ParentType = gst::Tracer;
    }

    impl ObjectImpl for PromLatencyTracer {
        fn properties() -> &'static [ParamSpec] {
            static PROPERTIES: OnceLock<Vec<ParamSpec>> = OnceLock::new();
            PROPERTIES.get_or_init(|| {
                vec![ParamSpecUInt::builder("server-port")
                    .nick("Server Port")
                    .blurb("Port for the metrics HTTP server (0 disables)")
                    .default_value(0)
                    .build()]
            })
        }

        fn set_property(&self, id: usize, value: &Value, pspec: &ParamSpec) {
            match id {
                1 => {
                    let v = value.get::<u32>().unwrap();
                    *self.metrics_port.write().unwrap() = v as u16;
                }
                _ => panic!("Unknown property id {}", pspec.name()),
            }
        }

        fn property(&self, id: usize, pspec: &ParamSpec) -> Value {
            match id {
                1 => (*self.metrics_port.read().unwrap() as u32).to_value(),
                _ => panic!("Unknown property id {}", pspec.name()),
            }
        }

        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            let tracer_obj: &gst::Tracer = obj.upcast_ref();

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
            let port = *self.metrics_port.read().unwrap();
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
