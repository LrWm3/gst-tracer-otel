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
    use std::sync::LazyLock;

    use super::*;

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
        lock: std::sync::Mutex<()>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PyroscopeTracer {
        const NAME: &'static str = "GstPyroscopeTracer";
        type Type = super::PyroscopeTracer;
        type ParentType = gst::Tracer;

        fn new() -> Self {
            gst::debug!(CAT, "Creating new PyroscopeTracer instance");
            Self {
                agent: None,
                lock: std::sync::Mutex::new(()),
            }
        }
    }

    fn create_pyroscope_agent() -> PyroscopeAgent<PyroscopeAgentRunning> {
        // TODO - make all configurable.
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
            self.register_hook(TracerHook::BinAddPost);
        }

        fn dispose(&self) {
            // Stop the agent when the tracer is dropped
            if self.agent.is_some() {
                let _guard = self.lock.lock().unwrap();
                if self.agent.is_some() {
                    gst::debug!(
                        CAT,
                        "Disposing PyroscopeTracer, stopping agent... This can take several minutes..."
                    );
                    // TODO - if we aren't configured to stop the agent properly, we should exit instead of stopping
                    //        the agent properly. Stopping the agent can take two minutes easily.
                    unsafe {
                        let raw_self: *mut imp::PyroscopeTracer =
                            self as *const _ as *mut imp::PyroscopeTracer;
                        (*raw_self).agent.take().map(|agent| {
                            let agent_stopped = agent.stop().unwrap();
                            agent_stopped.shutdown();
                            gst::debug!(CAT, "Pyroscope agent stopped");
                        });
                    }
                }
            }
        }
    }

    impl GstObjectImpl for PyroscopeTracer {}
    impl TracerImpl for PyroscopeTracer {
        /// Because the pipeline overall is a bin, we can use this hook as a
        /// signal that the tracer should start collecting data.
        ///
        /// We shutdown in the corresponding dispose method.
        fn bin_add_post(
            &self,
            _ts: u64,
            _bin: &gstreamer::Bin,
            _element: &gstreamer::Element,
            success: bool,
        ) {
            // If the agent is not running, start it
            // This is unsafe but whatever.
            if success && self.agent.is_none() {
                gst::debug!(CAT, "Pyroscope agent not running, starting it up");
                // Lock to ensure thread safety
                let _lock = self.lock.lock().unwrap();
                if self.agent.is_none() {
                    gst::debug!(CAT, "Creating new Pyroscope agent");
                    unsafe {
                        let raw_self: *mut imp::PyroscopeTracer =
                            self as *const _ as *mut imp::PyroscopeTracer;
                        (*raw_self).agent = Some(create_pyroscope_agent());
                    };
                }
            }
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
