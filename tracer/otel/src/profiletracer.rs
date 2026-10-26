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
        fn signals() -> &'static [glib::subclass::Signal] {
            static SIGNALS: LazyLock<Vec<glib::subclass::Signal>> =
                LazyLock::new(crate::lifecycle::signals);
            &SIGNALS
        }
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
            for hook in [
                TracerHook::PadPushPre,
                TracerHook::PadPushListPre,
                TracerHook::PadPullRangePre,
                TracerHook::PadPullRangePost,
            ] {
                self.register_hook(hook);
            }
            crate::profilecontext::register(self);
            #[cfg(target_os = "linux")]
            if crate::nvtxcontext::enabled() {
                self.register_hook(TracerHook::ElementChangeStatePre);
                self.register_hook(TracerHook::ElementChangeStatePost);
            }
        }
    }
    impl GstObjectImpl for ProfilesTracer {}
    impl TracerImpl for ProfilesTracer {
        #[cfg(target_os = "linux")]
        fn element_change_state_pre(
            &self,
            _: u64,
            element: &gst::Element,
            change: gst::StateChange,
        ) {
            crate::nvtxcontext::state_pre(element, change);
        }
        #[cfg(target_os = "linux")]
        fn element_change_state_post(
            &self,
            _: u64,
            element: &gst::Element,
            change: gst::StateChange,
            _: Result<gst::StateChangeSuccess, gst::StateChangeError>,
        ) {
            crate::nvtxcontext::state_post(element, change);
        }
        fn pad_pull_range_pre(&self, _: u64, pad: &gst::Pad, _: u64, _: u32) {
            crate::profiles::register_thread();
            crate::profilecontext::begin(pad, 2);
        }
        fn pad_pull_range_post(
            &self,
            _: u64,
            pad: &gst::Pad,
            _: Result<&gst::Buffer, gst::FlowError>,
        ) {
            crate::profilecontext::end(pad, 2);
        }
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
