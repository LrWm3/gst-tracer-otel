mod imp {
    use opentelemetry::{global::ObjectSafeSpan, trace::TraceContextExt, KeyValue};
    use opentelemetry_sdk::trace::SpanProcessor;

    #[derive(Debug, Default)]
    struct PyroscopeSpanProcessor {}
    impl SpanProcessor for PyroscopeSpanProcessor {
        fn on_start(&self, span: &mut opentelemetry_sdk::trace::Span, cx: &opentelemetry::Context) {
            // We only want to process spans that have have no parent or are remote
            let is_root_span = !cx.has_active_span() || cx.span().span_context().is_remote();
            if is_root_span {
                let s_str = span.span_context().span_id().to_string();
                span.set_attribute(KeyValue::new("pyroscope.profile.id", s_str));
                // python version
                // pyroscope.add_thread_tag(threading.get_ident(), PROFILE_ID_PYROSCOPE_TAG_KEY, _get_span_id(span))
            }
        }

        fn on_end(&self, span: opentelemetry_sdk::trace::SpanData) {
            let is_root_span = span.parent_span_id == 0.into() || span.span_context.is_remote();
            if is_root_span {
                // python version
                // pyroscope.remove_thread_tag(threading.get_ident(), PROFILE_ID_PYROSCOPE_TAG_KEY, _get_span_id(span))
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
