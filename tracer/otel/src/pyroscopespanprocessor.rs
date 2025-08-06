pub(crate) mod imp {
    use std::sync::LazyLock;

    use gstreamer as gst;
    use opentelemetry::{global::ObjectSafeSpan, trace::TraceContextExt, KeyValue};
    use opentelemetry_sdk::trace::SpanProcessor;
    use pyroscope::{backend::Tag, pyroscope::PyroscopeAgentRunning, PyroscopeAgent};
    use pyroscope_pprofrs::{pprof_backend, PprofConfig};

    static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
        gst::DebugCategory::new(
            "otel-tracer",
            gst::DebugColorFlags::empty(),
            Some("OTLP tracer with metrics"),
        )
    });
    #[derive(Debug, Default)]
    pub(crate) struct PyroscopeSpanProcessor {
        agent: std::sync::RwLock<Option<PyroscopeAgent<PyroscopeAgentRunning>>>,
    }
    impl PyroscopeSpanProcessor {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn create_first_agent(&self, tags: Vec<(&str, &str)>) {
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

        pub fn remove_agent_if_present(&self) {
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
                    ("package", env!("CARGO_PKG_NAME")),
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
    impl SpanProcessor for PyroscopeSpanProcessor {
        fn on_start(&self, span: &mut opentelemetry_sdk::trace::Span, cx: &opentelemetry::Context) {
            // We only want to process spans that have have no parent or are remote
            let is_root_span = !cx.has_active_span() || cx.span().span_context().is_remote();
            if is_root_span {
                let s_str = span.span_context().span_id().to_string();
                span.set_attribute(KeyValue::new("pyroscope.profile.id", s_str.clone()));
                // python version
                // pyroscope.add_thread_tag(threading.get_ident(), PROFILE_ID_PYROSCOPE_TAG_KEY, s_str)
                match self.agent.write().as_mut() {
                    Ok(agent) => {
                        let a = agent.as_ref().unwrap();
                        a.add_thread_tag(
                            thread_id::get() as u64,
                            Tag::new("span_id".to_owned(), s_str),
                        )
                        .expect("Failed to add thread tag");
                    }
                    Err(_) => {}
                }
            }
        }

        fn on_end(&self, span: opentelemetry_sdk::trace::SpanData) {
            let is_root_span = span.parent_span_id == 0.into() || span.span_context.is_remote();
            if is_root_span {
                // python version
                // pyroscope.remove_thread_tag(threading.get_ident(), PROFILE_ID_PYROSCOPE_TAG_KEY, s_str)
                let s_str = span.span_context.span_id().to_string();
                match self.agent.write().as_mut() {
                    Ok(agent) => {
                        let a = agent.as_ref().unwrap();
                        a.remove_thread_tag(
                            thread_id::get() as u64,
                            Tag::new("span_id".to_owned(), s_str),
                        )
                        .expect("Failed to remove thread id");
                    }
                    Err(_) => {}
                }
            }
        }

        fn force_flush(&self) -> opentelemetry_sdk::error::OTelSdkResult {
            return Ok(());
        }

        fn shutdown_with_timeout(
            &self,
            _timeout: std::time::Duration,
        ) -> opentelemetry_sdk::error::OTelSdkResult {
            return Ok(());
        }
    }
}
