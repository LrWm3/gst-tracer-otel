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
    use glib::{ParamSpec, ParamSpecBoolean, ParamSpecString, ParamSpecUInt, Value};

    static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
        gst::DebugCategory::new(
            "pyroscope",
            gst::DebugColorFlags::empty(),
            Some("Pyroscope tracer"),
        )
    });

    pub struct PyroscopeTracer {
        agent: std::sync::RwLock<Option<PyroscopeAgent<PyroscopeAgentRunning>>>,
        server_url: std::sync::RwLock<String>,
        tracer_name: std::sync::RwLock<String>,
        sample_rate: std::sync::RwLock<u32>,
        stop_agent_on_dispose: std::sync::RwLock<bool>,
        tags: std::sync::RwLock<String>,
    }

    impl Default for PyroscopeTracer {
        fn default() -> Self {
            Self {
                agent: std::sync::RwLock::new(None),
                server_url: std::sync::RwLock::new("http://localhost:4040".into()),
                tracer_name: std::sync::RwLock::new("gst.otel".into()),
                sample_rate: std::sync::RwLock::new(100),
                stop_agent_on_dispose: std::sync::RwLock::new(true),
                tags: std::sync::RwLock::new(String::new()),
            }
        }
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
            let url = self.server_url.read().unwrap().clone();
            let tracer_name = self.tracer_name.read().unwrap().clone();
            let sample_rate = *self.sample_rate.read().unwrap();
            let tags_str = self.tags.read().unwrap().clone();
            gst::debug!(CAT, "Creating Pyroscope agent with URL: {}", url);

            let parsed_tags: Vec<(String, String)> = tags_str
                .split(',')
                .filter_map(|tag| {
                    let mut parts = tag.splitn(2, '=');
                    if let (Some(key), Some(value)) = (parts.next(), parts.next()) {
                        Some((key.to_string(), value.to_string()))
                    } else {
                        None
                    }
                })
                .collect();

            let all_tags: Vec<(&str, &str)> = vec![
                ("service", env!("CARGO_PKG_NAME")),
                ("version", env!("CARGO_PKG_VERSION")),
                ("repo", env!("CARGO_PKG_REPOSITORY")),
                ("os", std::env::consts::OS),
                ("arch", std::env::consts::ARCH),
            ]
            .into_iter()
            .chain(parsed_tags.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .chain(tags)
            .collect();

            PyroscopeAgent::builder(url, tracer_name)
                .tags(all_tags)
                .backend(pprof_backend(
                    PprofConfig::new().sample_rate(sample_rate),
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
            Self::default()
        }
    }

    impl ObjectImpl for PyroscopeTracer {
        fn properties() -> &'static [ParamSpec] {
            static PROPERTIES: LazyLock<Vec<ParamSpec>> = LazyLock::new(|| {
                vec![
                    ParamSpecString::builder("server-url")
                        .nick("Server URL")
                        .blurb("Pyroscope server URL")
                        .default_value(Some("http://localhost:4040"))
                        .build(),
                    ParamSpecString::builder("tracer-name")
                        .nick("Tracer Name")
                        .blurb("Tracer name")
                        .default_value(Some("gst.otel"))
                        .build(),
                    ParamSpecUInt::builder("sample-rate")
                        .nick("Sample Rate")
                        .blurb("Sample rate in Hz")
                        .default_value(100)
                        .build(),
                    ParamSpecBoolean::builder("stop-agent-on-dispose")
                        .nick("Stop Agent On Dispose")
                        .blurb("Stop Pyroscope agent on dispose")
                        .default_value(true)
                        .build(),
                    ParamSpecString::builder("tags")
                        .nick("Tags")
                        .blurb("Additional tags in the form k1=v1,k2=v2")
                        .default_value(Some(""))
                        .build(),
                ]
            });

            PROPERTIES.as_ref()
        }

        fn set_property(&self, id: usize, value: &Value, pspec: &ParamSpec) {
            match id {
                1 => {
                    let v = value.get::<String>().unwrap();
                    *self.server_url.write().unwrap() = v;
                }
                2 => {
                    let v = value.get::<String>().unwrap();
                    *self.tracer_name.write().unwrap() = v;
                }
                3 => {
                    let v = value.get::<u32>().unwrap();
                    *self.sample_rate.write().unwrap() = v;
                }
                4 => {
                    let v = value.get::<bool>().unwrap();
                    *self.stop_agent_on_dispose.write().unwrap() = v;
                }
                5 => {
                    let v = value.get::<String>().unwrap();
                    *self.tags.write().unwrap() = v;
                }
                _ => panic!("Unknown property id {}", pspec.name()),
            }
        }

        fn property(&self, id: usize, pspec: &ParamSpec) -> Value {
            match id {
                1 => self.server_url.read().unwrap().to_value(),
                2 => self.tracer_name.read().unwrap().to_value(),
                3 => self.sample_rate.read().unwrap().to_value(),
                4 => self.stop_agent_on_dispose.read().unwrap().to_value(),
                5 => self.tags.read().unwrap().to_value(),
                _ => panic!("Unknown property id {}", pspec.name()),
            }
        }

        /// Called whenever the plugin itself is loaded; including during gst-inspect-1.0
        /// and other utility commands; avoid starting collectors or doing other heavy work here.
        fn constructed(&self) {
            self.parent_constructed();
            self.register_hook(TracerHook::BinAddPost);
        }

        /// Called when the tracer is disposed, typically when the pipeline is stopped or the plugin is unloaded.
        /// This is where we stop the agent if it is running.
        fn dispose(&self) {
            if *self.stop_agent_on_dispose.read().unwrap() {
                self.remove_agent_if_present();
            }
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
