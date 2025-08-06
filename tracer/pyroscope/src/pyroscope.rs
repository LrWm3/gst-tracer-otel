/* pyroscope.rs
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
        agent: std::sync::RwLock<Option<PyroscopeAgent<PyroscopeAgentRunning>>>,
    }

    impl PyroscopeTracer {
        fn create_first_agent(&self, tags: Vec<(&str, &str)>) {
            // First, check with a read lock
            {
                let agent_read = self.agent.read().unwrap();
                if agent_read.is_some() {
                    return;
                }
            }
            // If not present, acquire write lock and initialize
            let mut agent_write = self.agent.write().unwrap();
            if agent_write.is_none() {
                gst::debug!(CAT, "Creating new Pyroscope agent");
                *agent_write = Some(self.create_pyroscope_agent(tags));
            }
        }

        fn remove_agent_if_present(&self) {
            let mut agent_write = self.agent.write().unwrap();
            if let Some(agent) = agent_write.take() {
                gst::debug!(
                    CAT,
                    "Disposing PyroscopeTracer, stopping agent... This can take several minutes..."
                );
                let agent_stopped = agent.stop().unwrap();
                agent_stopped.shutdown();
                gst::debug!(CAT, "Pyroscope agent stopped");
            }
        }

        fn create_pyroscope_agent(
            &self,
            tags: Vec<(&str, &str)>,
        ) -> PyroscopeAgent<PyroscopeAgentRunning> {
            // Messy config, should probably allow for setting through element properties.
            let url = std::env::var("GST_PYROSCOPE_SERVER_URL")
                .unwrap_or_else(|_| "http://localhost:4040".into());
            gst::debug!(CAT, "Creating Pyroscope agent with URL: {}", url);
            PyroscopeAgent::builder(
                url,
                std::env::var("GST_PYROSCOPE_TRACER_NAME").unwrap_or_else(|_| "gst.otel".into()),
            )
            .tags(
                vec![
                    ("service", env!("CARGO_PKG_NAME")),
                    ("version", env!("CARGO_PKG_VERSION")),
                    ("repo", env!("CARGO_PKG_REPOSITORY")),
                    ("os", std::env::consts::OS),
                    ("arch", std::env::consts::ARCH),
                ]
                .into_iter()
                .chain(
                    std::env::var("GST_PYROSCOPE_TAGS")
                        .unwrap_or_else(|_| String::new())
                        .split(',')
                        .filter_map(|tag| {
                            let mut parts = tag.splitn(2, '=');
                            if let (Some(key), Some(value)) = (parts.next(), parts.next()) {
                                Some((key, value))
                            } else {
                                None
                            }
                        }),
                )
                .chain(tags)
                .collect(),
            )
            .backend(pprof_backend(
                PprofConfig::new().sample_rate(
                    std::env::var("GST_PYROSCOPE_SAMPLE_RATE")
                        .unwrap_or_else(|_| "100".into())
                        .parse()
                        .unwrap_or(100),
                ),
            ))
            .build()
            .unwrap()
            .start()
            .unwrap()
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PyroscopeTracer {
        const NAME: &'static str = "GstPyroscopeTracer";
        type Type = super::PyroscopeTracer;
        type ParentType = gst::Tracer;

        fn new() -> Self {
            gst::debug!(CAT, "Creating new PyroscopeTracer instance");
            Self {
                agent: std::sync::RwLock::new(None),
            }
        }
    }

    impl ObjectImpl for PyroscopeTracer {
        /// Called whenever the plugin itself is loaded; including during gst-inspect-1.0
        /// and other utility commands; avoid starting collectors or doing other heavy work here.
        fn constructed(&self) {
            self.parent_constructed();
            self.register_hook(TracerHook::BinAddPost);
        }

        /// Called when the tracer is disposed, typically when the pipeline is stopped or the plugin is unloaded.
        /// This is where we stop the agent if it is running.
        fn dispose(&self) {
            // Stop the agent when the tracer is dropped
            self.remove_agent_if_present();
        }
    }

    impl GstObjectImpl for PyroscopeTracer {}
    impl TracerImpl for PyroscopeTracer {
        /// Because the pipeline overall is a bin, we can use this hook as a
        /// signal that the tracer should start collecting data.
        ///
        /// In other plugins we prefer to use the ffi hooks for performance
        /// reasons but this is typically not a hot hook, so we prefer to use
        /// the safe variant.
        ///
        /// We shutdown in the corresponding dispose method.
        fn bin_add_post(
            &self,
            _ts: u64,
            bin: &gstreamer::Bin,
            _element: &gstreamer::Element,
            success: bool,
        ) {
            // If the agent is not running & this is the pipeline bin, start it up.
            if success && bin.downcast_ref::<gst::Pipeline>().is_some() {
                self.create_first_agent(vec![("pipeline", bin.name().as_str())]);
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
