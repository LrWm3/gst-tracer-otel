//! Optional debug logging is deliberately separate from compact span capture.
use gstreamer as gst;
use opentelemetry::logs::{LogRecord, Logger, LoggerProvider, Severity};
use opentelemetry_otlp::{RetryPolicy, WithHttpConfig};
use opentelemetry_sdk::{logs::SdkLoggerProvider, Resource};
use std::cell::Cell;
use std::sync::OnceLock;
use std::time::Duration;

static PROVIDER: OnceLock<SdkLoggerProvider> = OnceLock::new();
thread_local! { static IN_LOG: Cell<bool> = const { Cell::new(false) }; }

pub(crate) fn initialize(resource: Resource) -> Result<(), String> {
    let exporter = opentelemetry_otlp::LogExporter::builder()
        .with_http()
        .with_retry_policy(RetryPolicy::disabled())
        .build()
        .map_err(|e| e.to_string())?;
    let provider = SdkLoggerProvider::builder()
        .with_resource(resource)
        .with_batch_exporter(exporter)
        .build();
    let logger = provider.logger("mars-otel");
    let _ = PROVIDER.set(provider);
    gst::log::add_log_function(
        move |category, level, file, function, line, _object, message| {
            if IN_LOG.with(|v| v.replace(true)) {
                return;
            }
            let mut record = logger.create_log_record();
            record.set_severity_number(match level {
                gst::DebugLevel::Error | gst::DebugLevel::Fixme => Severity::Error,
                gst::DebugLevel::Warning => Severity::Warn,
                gst::DebugLevel::Info => Severity::Info,
                gst::DebugLevel::Debug | gst::DebugLevel::Log => Severity::Debug,
                _ => Severity::Trace,
            });
            record.set_timestamp(std::time::SystemTime::now());
            record.set_body(
                message
                    .get()
                    .map(|m| m.to_string())
                    .unwrap_or_default()
                    .into(),
            );
            record.add_attribute("gst.category", category.name().to_string());
            record.add_attribute("code.file", file.to_string());
            record.add_attribute("code.function", function.to_string());
            record.add_attribute("code.line", line as i64);
            if let Some(context) = crate::capture::current_context().filter(|c| c.is_valid()) {
                record.set_trace_context(
                    context.trace_id(),
                    context.span_id(),
                    Some(context.trace_flags()),
                );
            }
            logger.emit(record);
            IN_LOG.with(|v| v.set(false));
        },
    );
    Ok(())
}

pub(crate) fn flush() {
    if let Some(provider) = PROVIDER.get() {
        let _ = provider.force_flush();
    }
}
pub(crate) fn shutdown(timeout: Duration) {
    if let Some(provider) = PROVIDER.get() {
        let _ = provider.shutdown_with_timeout(timeout);
    }
}
