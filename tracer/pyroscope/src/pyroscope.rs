/*
*
* This library is free software; you can redistribute it and/or
* modify it under the terms of the GNU Library General Public
* License as published by the Free Software Foundation; either
* version 2 of the License, or (at your option) any later version.
 *
 * This library is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
 * Library General Public License for more details.
 *
 * You should have received a copy of the GNU Library General Public
 * License along with this library; if not, write to the
 * Free Software Foundation, Inc., 51 Franklin St, Fifth Floor,
 * Boston, MA 02110-1301, USA.
 */
use glib::subclass::prelude::*;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gstreamer as gst;

// Our Tracer subclass
mod imp {
    use std::{ops::Deref, sync::LazyLock};

    use super::*;

    use glib::translate::{FromGlibPtrNone, ToGlibPtr};
    use pyroscope::{pyroscope::PyroscopeAgentRunning, PyroscopeAgent};
    use pyroscope_pprofrs::{pprof_backend, PprofConfig};

    static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
        gst::DebugCategory::new(
            "pyroscope",
            gst::DebugColorFlags::empty(),
            Some("Pyroscope tracer"),
        )
    });

    #[derive(Default)]
    pub struct PyroscopeTracer {
        agent: Option<PyroscopeAgent<PyroscopeAgentRunning>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PyroscopeTracer {
        const NAME: &'static str = "GstPyroscopeTracer";
        type Type = super::PyroscopeTracer;
        type ParentType = gst::Tracer;

        fn new() -> Self {
            gst::debug!(CAT, "Creating new PyroscopeTracer instance");
            Self { agent: None }
        }
    }

    fn create_pyroscope_agent() -> PyroscopeAgent<PyroscopeAgentRunning> {
        PyroscopeAgent::builder("http://localhost:4040", "pyroscope_tracer")
            .backend(pprof_backend(PprofConfig::new().sample_rate(100)))
            .build()
            .unwrap()
            .start()
            .unwrap()
    }

    impl ObjectImpl for PyroscopeTracer {
        // Called once when the class is initialized
        fn constructed(&self) {
            self.parent_constructed();
            gst::debug!(CAT, "PyroscopeTracer constructed");
            let obj = self.obj();
            let tracer_obj: &gst::Tracer = obj.upcast_ref();

            self.register_hook(TracerHook::BinAddPost);
        }

        fn dispose(&self) {
            gst::debug!(CAT, "PyroscopeTracer disposed");
        }
    }

    impl GstObjectImpl for PyroscopeTracer {}
    impl TracerImpl for PyroscopeTracer {
        fn bin_add_post(
            &self,
            ts: u64,
            bin: &gstreamer::Bin,
            element: &gstreamer::Element,
            success: bool,
        ) {
            gst::debug!(
                CAT,
                "bin_add_post called on bin {} with element {} at timestamp {}, success: {}",
                bin.name(),
                element.name(),
                ts,
                success
            );

            // If the agent is not running, start it
            // This is unsafe but whatever.
            if self.agent.is_none() {
                gst::debug!(CAT, "Pyroscope agent not running, starting it up");
                unsafe {
                    let raw_self: *mut imp::PyroscopeTracer =
                        self as *const _ as *mut imp::PyroscopeTracer;
                    (*raw_self).agent = Some(create_pyroscope_agent());
                };
            }
        }
    }
    impl Drop for PyroscopeTracer {
        fn drop(&mut self) {
            // Stop the agent when the tracer is dropped
            self.agent.take().map(|agent| {
                gst::debug!(CAT, "Stopping Pyroscope agent");
                agent.stop().unwrap();
                gst::debug!(CAT, "Pyroscope agent stopped");
            });
        }
    }
}

glib::wrapper! {
    pub struct PyroscopeTracer(ObjectSubclass<imp::PyroscopeTracer>)
        @extends gst::Tracer, gst::Object;
}

// Register the plugin with GStreamer
pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    // Register the tracer factory
    gst::Tracer::register(Some(plugin), "pyroscope", PyroscopeTracer::static_type())?;

    Ok(())
}
