use std::thread;

use glib::GStr;
use gst::DebugCategory;
use gst::DebugLevel;
use gstreamer as gst;
use gstreamer::DebugMessage;
use gstreamer::LoggedObject;
use opentelemetry::logs::Severity;

pub trait LogBridge: Send + Sync + 'static {
    /// Called for every GstDebugMessage
    ///
    /// Arguments are similar to GstDebugMessage, but with some additional fields:
    /// - `trace_id`: The trace ID of the current trace context.
    /// - `span_id`: The span ID of the current span context.
    ///
    /// This allows structured logging of debug messages with trace/span context.
    #[allow(clippy::too_many_arguments)]
    fn log_message(
        &self,
        category: &DebugCategory,
        level: DebugLevel,
        file: &GStr,
        function: &GStr,
        line: u32,
        message: &DebugMessage,
        obj: Option<&LoggedObject>,
        trace_id: &str,
        span_id: &str,
    );
}
use opentelemetry::logs::LogRecord;
use opentelemetry::logs::{AnyValue, Logger};
use opentelemetry::Key;
use opentelemetry::KeyValue;
use opentelemetry_otlp::LogExporter;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::Resource;

pub struct StructuredBridge<L: Logger> {
    logger: L,
}

impl<L: Logger> StructuredBridge<L> {
    pub fn new(logger: L) -> Self {
        StructuredBridge { logger }
    }
}
fn severity_of_debug_level(level: DebugLevel) -> Severity {
    match level {
        DebugLevel::None => Severity::Error,
        DebugLevel::Error => Severity::Error,
        DebugLevel::Warning => Severity::Warn,
        DebugLevel::Fixme => Severity::Error,
        DebugLevel::Info => Severity::Info,
        DebugLevel::Log => Severity::Debug,
        DebugLevel::Debug => Severity::Debug,
        DebugLevel::Trace => Severity::Trace,
        DebugLevel::Memdump => Severity::Trace,
        _ => todo!(),
    }
}

impl<L: Logger + 'static + Send + Sync> LogBridge for StructuredBridge<L> {
    fn log_message(
        &self,
        category: &DebugCategory,
        level: DebugLevel,
        file: &GStr,
        function: &GStr,
        line: u32,
        message: &DebugMessage,
        _obj: Option<&LoggedObject>,
        trace_id: &str,
        span_id: &str,
    ) {
        let mut record = self.logger.create_log_record();
        let debug_level = severity_of_debug_level(level);
        record.set_severity_number(debug_level);
        record.set_timestamp(std::time::SystemTime::now());

        // TODO - not sure how to comply with 'static lifetime
        // record.set_severity_text(&level.to_owned().to_string());
        record.set_body(
            // Convert GStr to String, or use empty string if None
            // This is a workaround for the fact that GStr does not implement Debug
            // and we need to convert it to a String for structured logging.
            // If message is None, we use an empty string.
            // This is similar to how GStreamer handles debug messages.
            message
                .get()
                .map(|s| s.to_string())
                .unwrap_or_default()
                .to_string()
                .into(),
        );

        // OTel attributes
        record.add_attribute(
            Key::new("gst.category"),
            AnyValue::String(category.name().into()),
        );
        record.add_attribute(
            Key::new("thread.name"),
            thread::current().name().unwrap_or("unknown").to_string(),
        );
        record.add_attribute(
            Key::new("thread.id"),
            format!("{:?}", thread::current().id()),
        );
        record.add_attribute(Key::new("trace.id"), trace_id.to_string());
        record.add_attribute(Key::new("span.id"), span_id.to_string());
        record.add_attribute(Key::new("code.file"), file.to_string());
        record.add_attribute(Key::new("code.function"), function.to_string());
        record.add_attribute(Key::new("code.line"), AnyValue::Int(line as i64));

        self.logger.emit(record);
    }
}
pub struct PlaintextBridge;

#[allow(dead_code)]
impl PlaintextBridge {
    pub fn new() -> Self {
        PlaintextBridge
    }
}

impl LogBridge for PlaintextBridge {
    fn log_message(
        &self,
        category: &DebugCategory,
        level: DebugLevel,
        file: &GStr,
        function: &GStr,
        line: u32,
        message: &DebugMessage,
        obj: Option<&LoggedObject>,
        trace_id: &str,
        span_id: &str,
    ) {
        let usecs = glib::monotonic_time(); // microseconds since boot
        let secs = usecs / 1_000_000;
        let micros = usecs % 1_000_000;
        let hours = secs / 3600;
        let mins = (secs / 60) % 60;
        let secs_rem = secs % 60;
        let nanos = micros * 1_000;
        let timestamp = format!("{hours}:{mins:02}:{secs_rem:02}.{nanos:09}");

        // pointer to current Thread handle
        let current_thread = thread::current();
        let thread_ptr = format!("{:p}", &current_thread);

        // level and category
        let level_str = format!("{level:?}").to_uppercase();
        let level_padded = format!("{level_str:<16}"); // pad to 16 chars
        let category_str = category.name();

        // the actual message text
        let msg = message.get().map(|s| s.to_string()).unwrap_or_default();

        // final formatted line
        eprintln!(
            "{} {:?} {} {} {} {}{} {}:{}:{}: {}",
            timestamp,
            obj.map(|o| o.as_ptr()).unwrap_or(core::ptr::null_mut()),
            trace_id,
            span_id,
            thread_ptr,
            level_padded,
            category_str,
            file,
            line,
            function,
            msg,
        );
    }
}

pub fn init_logs_otlp() -> SdkLoggerProvider {
    // 1. Build an OTLP LogExporter over gRPC
    let exporter = LogExporter::builder()
        .with_http()
        .build() // use HTTP
        .expect("failed to build OTLP exporter");

    // 3. Provider

    SdkLoggerProvider::builder()
        .with_resource(
            Resource::builder_empty()
                .with_attribute(KeyValue::new("service.name", "gst.otel"))
                .build(),
        )
        .with_batch_exporter(exporter)
        // .with_log_processor(BatchLogProcessor::builder(exporter).build())
        .build()
}
