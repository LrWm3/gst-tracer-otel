use glib::prelude::*;
use gstreamer as gst;

use crate::promlatencyimp::{PromLatencyTracerImp, CAT};

mod imp {
    use super::*;
    use gst::subclass::prelude::*;
    use std::sync::OnceLock;

    #[derive(Default)]
    pub struct PromLatencyTracer {
        pub core: PromLatencyTracerImp,
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
                        gst::info!(CAT, "Prometheus metrics requested via signal, returning {} bytes", ret.len());
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
            self.core.element_new(ts, element);
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

