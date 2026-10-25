//! Optional GStreamer logs with bounded background delivery and native context.
use crate::{
    export::FlushError,
    logexport::{LogStats, Logs},
};
use gstreamer as gst;
use opentelemetry_proto::tonic::{
    common::v1::{any_value::Value, AnyValue, KeyValue},
    logs::v1::{LogRecord, SeverityNumber},
};
use opentelemetry_sdk::Resource;
use std::{
    cell::Cell,
    sync::OnceLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

static LOGS: OnceLock<Logs> = OnceLock::new();
thread_local! { static IN_LOG: Cell<bool> = const { Cell::new(false) }; }

pub(crate) fn initialize(resource: Resource) -> Result<(), String> {
    let logs = Logs::new(resource)?;
    LOGS.set(logs).map_err(|_| "logs already initialized")?;
    gst::log::add_log_function(
        move |category, level, file, function, line, _object, message| {
            // Worker diagnostics must never recursively create export traffic.
            if category.name().starts_with("mars-") || IN_LOG.with(|v| v.replace(true)) {
                return;
            }
            let text = |value: &str, limit| {
                Value::StringValue(crate::export::text_value(value, limit).to_string())
            };
            let attribute = |key: &str, value| KeyValue {
                key: key.into(),
                value: Some(AnyValue { value: Some(value) }),
                ..Default::default()
            };
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
                .min(u64::MAX as u128) as u64;
            let context = crate::capture::current_context().filter(|c| c.is_valid());
            let record = LogRecord {
                time_unix_nano: now,
                observed_time_unix_nano: now,
                severity_number: match level {
                    gst::DebugLevel::Error | gst::DebugLevel::Fixme => SeverityNumber::Error,
                    gst::DebugLevel::Warning => SeverityNumber::Warn,
                    gst::DebugLevel::Info => SeverityNumber::Info,
                    gst::DebugLevel::Debug | gst::DebugLevel::Log => SeverityNumber::Debug,
                    _ => SeverityNumber::Trace,
                } as i32,
                body: Some(AnyValue {
                    value: Some(text(message.get().as_deref().unwrap_or_default(), 16384)),
                }),
                attributes: vec![
                    attribute("gst.category", text(category.name(), 256)),
                    attribute("code.file", text(file, 4096)),
                    attribute("code.function", text(function, 4096)),
                    attribute("code.line", Value::IntValue(line as i64)),
                ],
                trace_id: context
                    .as_ref()
                    .map(|c| c.trace_id().to_bytes().to_vec())
                    .unwrap_or_default(),
                span_id: context
                    .as_ref()
                    .map(|c| c.span_id().to_bytes().to_vec())
                    .unwrap_or_default(),
                flags: context
                    .as_ref()
                    .map(|c| c.trace_flags().to_u8() as u32)
                    .unwrap_or_default(),
                ..Default::default()
            };
            LOGS.get().unwrap().emit(record);
            IN_LOG.with(|v| v.set(false));
        },
    );
    Ok(())
}

pub(crate) fn flush(timeout: Duration) -> Result<(), FlushError> {
    LOGS.get().map_or(Ok(()), |logs| logs.flush(timeout))
}
pub(crate) fn shutdown(timeout: Duration) -> Result<(), FlushError> {
    LOGS.get().map_or(Ok(()), |logs| logs.shutdown(timeout))
}
pub(crate) fn stats() -> LogStats {
    LOGS.get().map_or_else(LogStats::default, Logs::stats)
}
