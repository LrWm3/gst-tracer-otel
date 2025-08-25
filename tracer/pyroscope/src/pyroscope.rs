// pyroscope.rs
//
// SPDX-License-Identifier: LGPL

/* This library is free software; you can redistribute it and/or
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

/**
 * pyroscope:
 *
 * This tracer uses [Pyroscope](https://pyroscope.io/) to collect and visualize performance data.
 *
 * Example:
 *
 * ```console
 * $ GST_TRACERS='pyroscope(server-url=http://localhost:4040,tracer-name=myapp,sample-rate=100,tags=env=dev,team=video)'
 * ```
 *
 * ## Parameters
 *
 * ### server-url
 *
 * The URL of the Pyroscope server to which the agent will send profiling data.
 *
 * Default: `http://localhost:4040`
 *
 * ### tracer-name
 *
 * The name of the tracer, which will appear in the Pyroscope UI.
 *
 * Default: `gst.pyroscope`
 *
 * ### sample-rate
 *
 * The sampling rate in Hz (samples per second).
 *
 * Default: `100`
 *
 * ### stop-agent-on-dispose
 *
 * Whether to stop the Pyroscope agent when the tracer is disposed.
 *
 * Default: `true`
 *
 * ### tags
 *
 * Additional tags to attach to the profiling data, comma-separated, in the form `key=value`.
 *
 * Example: `env=dev,team=video`
 *
 * Default: empty
 */
use glib::subclass::prelude::*;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gstreamer as gst;

mod imp {
    use std::{str::FromStr, sync::LazyLock};

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

    #[derive(Debug)]
    struct Settings {
        server_url: String,
        tracer_name: String,
        sample_rate: u32,
        stop_agent_on_dispose: bool,
        tags: Vec<(String, String)>,
    }

    impl Default for Settings {
        fn default() -> Self {
            Self {
                server_url: "http://localhost:4040".into(),
                tracer_name: "gst.pyroscope".into(),
                sample_rate: 100,
                stop_agent_on_dispose: true,
                tags: vec![],
            }
        }
    }

    impl Settings {
        fn update_from_params(&mut self, imp: &PyroscopeTracer, params: String) {
            let s = match gst::Structure::from_str(&format!("pyroscope,{params}")) {
                Ok(s) => s,
                Err(err) => {
                    gst::warning!(CAT, imp = imp, "failed to parse tracer parameters: {}", err);
                    return;
                }
            };
            if let Ok(v) = s.get::<String>("server-url") {
                self.server_url = v;
            }
            if let Ok(v) = s.get::<String>("tracer-name") {
                self.tracer_name = v;
            }
            if let Ok(v) = s.get::<u32>("sample-rate") {
                self.sample_rate = v;
            }
            if let Ok(v) = s.get::<bool>("stop-agent-on-dispose") {
                self.stop_agent_on_dispose = v;
            }
            if let Ok(v) = s.get::<String>("tags") {
                let parsed_tags: Vec<(String, String)> = v
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
                self.tags = parsed_tags;
            }
        }
    }

    #[derive(Debug, Default)]
    struct State {
        agent: Option<PyroscopeAgent<PyroscopeAgentRunning>>,
    }

    #[derive(Debug, Default)]
    pub struct PyroscopeTracer {
        state: std::sync::RwLock<State>,
        settings: std::sync::RwLock<Settings>,
    }

    impl PyroscopeTracer {
        fn create_first_agent(&self, tags: Vec<(&str, &str)>) {
            // First, check with a read lock to save time
            {
                let state_read = &self.state.read().unwrap();
                if state_read.agent.is_some() {
                    return;
                }
            }
            // If not present, acquire write lock and initialize if still not present
            let mut state_write = self.state.write().unwrap();
            if state_write.agent.is_none() {
                gst::debug!(CAT, "Creating new Pyroscope agent");
                state_write.agent =
                    Some(self.create_pyroscope_agent(&self.settings.read().unwrap(), tags));
            }
        }

        fn remove_agent_if_present(&self) {
            let mut agent_write = self.state.write().unwrap();
            if let Some(agent) = agent_write.agent.take() {
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
            settings: &Settings,
            tags: Vec<(&str, &str)>,
        ) -> PyroscopeAgent<PyroscopeAgentRunning> {
            let url = settings.server_url.clone();
            let tracer_name = settings.tracer_name.clone();
            let sample_rate = settings.sample_rate;

            let settings_tags = settings.tags.clone();

            gst::debug!(CAT, "Creating Pyroscope agent with URL: {}", url);

            let all_tags: Vec<(&str, &str)> = vec![
                ("service", env!("CARGO_PKG_NAME")),
                ("version", env!("CARGO_PKG_VERSION")),
                ("repo", env!("CARGO_PKG_REPOSITORY")),
                ("os", std::env::consts::OS),
                ("arch", std::env::consts::ARCH),
            ]
            .into_iter()
            .chain(settings_tags.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .chain(tags)
            .collect();

            PyroscopeAgent::builder(url, tracer_name)
                .tags(all_tags)
                .backend(pprof_backend(PprofConfig::new().sample_rate(sample_rate)))
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
        /// Called whenever the plugin itself is loaded; including during gst-inspect-1.0
        /// and other utility commands; avoid starting collectors or doing other heavy work here.
        fn constructed(&self) {
            self.parent_constructed();

            // Get parameterized settings.
            if let Some(params) = self.obj().property::<Option<String>>("params") {
                let mut settings = self.settings.write().unwrap();
                settings.update_from_params(self, params);
            }

            self.register_hook(TracerHook::BinAddPost);
        }

        /// Called when the tracer is disposed, typically when the pipeline is stopped or the plugin is unloaded.
        /// This is where we stop the agent if it is running.
        fn dispose(&self) {
            if self.settings.read().unwrap().stop_agent_on_dispose {
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
