//! Profiles-only entry point for the shared embedded sampler.
use glib::subclass::prelude::*;
use gstreamer::{self as gst, prelude::*, subclass::prelude::*};
use std::sync::LazyLock;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "mars-profiles",
        gst::DebugColorFlags::empty(),
        Some("Embedded CPU profiles without OTLP capture"),
    )
});

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct ProfilesTracer;

    #[glib::object_subclass]
    impl ObjectSubclass for ProfilesTracer {
        const NAME: &'static str = "MarsProfilesTracer";
        type Type = super::ProfilesTracer;
        type ParentType = gst::Tracer;
    }

    impl ObjectImpl for ProfilesTracer {
        fn constructed(&self) {
            self.parent_constructed();
            if self
                .obj()
                .property::<Option<String>>("params")
                .is_some_and(|params| !params.trim().is_empty())
            {
                gst::error!(CAT, "mars-profiles uses GST_OTEL_PROFILE_* and OTEL_SERVICE_NAME; tracer parameters are not supported");
                return;
            }
            if !crate::tracer::activate(crate::tracer::Mode::Profiles) {
                return;
            }
            for hook in [TracerHook::PadPushPre, TracerHook::PadPushListPre] {
                self.register_hook(hook);
            }
            crate::profilecontext::register(self);
        }
    }
    impl GstObjectImpl for ProfilesTracer {}
    impl TracerImpl for ProfilesTracer {
        fn pad_push_pre(&self, _: u64, _: &gst::Pad, _: &gst::Buffer) {
            crate::profiles::register_thread();
        }
        fn pad_push_list_pre(&self, _: u64, _: &gst::Pad, _: &gst::BufferList) {
            crate::profiles::register_thread();
        }
        crate::profilecontext::methods!();
    }
}

glib::wrapper! {
    /// CPU profiles from participating GStreamer threads, without OTLP signals.
    /// Shares the process-wide configuration and shutdown API with the OTel tracers.
    pub struct ProfilesTracer(ObjectSubclass<imp::ProfilesTracer>)
        @extends gst::Tracer, gst::Object;
}

pub(crate) fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Tracer::register(Some(plugin), "mars-profiles", ProfilesTracer::static_type())
}
