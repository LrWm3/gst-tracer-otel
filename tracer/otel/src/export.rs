//! Bounded capture handoff and batched direct OTLP export.
use std::fmt;
use std::sync::atomic::{
    AtomicBool, AtomicU64, AtomicUsize,
    Ordering::{Relaxed, Release, SeqCst},
};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
#[cfg(test)]
use opentelemetry::trace::{SpanContext, SpanKind, Status};
#[cfg(test)]
use opentelemetry::{InstrumentationScope, KeyValue};
#[cfg(test)]
use opentelemetry_sdk::trace::{SpanData, SpanExporter};
use opentelemetry_sdk::Resource;

use crate::capture::Record;
#[path = "export_pool.rs"]
mod pool;
#[cfg(test)]
use crate::capture::Token;

// Fixed storage bounds memory even when applications create many short-lived
// threads. Threads share a shard only after the index wraps; no registry grows.
pub(crate) const SHARDS: usize = 64;
static NEXT_SHARD: AtomicUsize = AtomicUsize::new(0);
thread_local! { static SHARD: usize = NEXT_SHARD.fetch_add(1, Relaxed) % SHARDS; }
pub(crate) fn shard_index() -> usize {
    SHARD.with(|index| *index)
}

#[repr(align(128))]
#[derive(Default)]
struct SubmissionCount(AtomicUsize);

macro_rules! counters {
    ($($name:ident),* $(,)?) => {
        /// Cumulative counts. Export success means collector acceptance,
        /// not a guarantee of durable collector storage. Snapshots are concurrent.
        #[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
        pub struct ExportStats { $(pub $name: u64,)* pub pending_spans: u64 }
        impl ExportStats {
            pub fn to_structure(&self) -> gstreamer::Structure {
                gstreamer::Structure::builder("mars-otel-stats")
                    $(.field(stringify!($name), self.$name))*
                    .field("pending_spans", self.pending_spans).build()
            }
        }
        #[repr(align(128))]
        #[derive(Default)]
        pub(crate) struct LocalCounters { $(pub $name: AtomicU64,)* }
        pub(crate) struct Counters { shards: [LocalCounters; SHARDS] }
        impl Default for Counters {
            fn default() -> Self { Self { shards: std::array::from_fn(|_| LocalCounters::default()) } }
        }
        impl Counters {
            pub fn local(&self) -> &LocalCounters { &self.shards[shard_index()] }
            pub fn snapshot(&self) -> ExportStats {
                let mut result = ExportStats::default();
                for shard in &self.shards { $(result.$name = result.$name.wrapping_add(shard.$name.load(Relaxed));)* }
                // Producer and worker snapshots are not atomic. A fast export
                // may be observed before its enqueue count; never underflow.
                result.pending_spans = result.enqueued_spans.saturating_sub(
                    result.exported_spans.saturating_add(result.failed_export_spans));
                result
            }
        }
    }
}
counters! {
    started_spans, completed_spans, enqueued_spans, exported_spans,
    dropped_queue_full, dropped_closed, failed_export_spans, failed_exports,
    rejected_spans, export_warnings,
    unsampled_pushes, unsampled_pulls, unsampled_lists, skipped_closed, propagation_misses, propagation_fallbacks,
    dropped_depth, mismatched_hooks,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlushError {
    NotInitialized,
    Timeout,
    Closed,
    ExportFailed,
}
impl fmt::Display for FlushError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for FlushError {}

#[derive(Clone, Debug)]
pub(crate) struct Config {
    pub ratio: f64,
    pub traces: bool,
    pub queue_capacity: usize,
    pub batch_size: usize,
    pub concurrency: usize,
    pub interval: Duration,
    pub flush_timeout: Duration,
    pub attribute_limit: usize,
    pub value_length_limit: usize,
    pub logs: bool,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            ratio: 1.0,
            traces: true,
            queue_capacity: 8192,
            batch_size: 512,
            concurrency: 1,
            interval: Duration::from_millis(200),
            flush_timeout: Duration::from_secs(10),
            attribute_limit: 128,
            value_length_limit: 4096,
            logs: false,
        }
    }
}
impl Config {
    pub fn from_env() -> Result<Self, String> {
        fn number(
            name: &str,
            default: usize,
            max: usize,
            allow_zero: bool,
        ) -> Result<usize, String> {
            let value = match std::env::var(name) {
                Ok(s) => s.parse::<usize>().map_err(|_| format!("invalid {name}"))?,
                Err(_) => default,
            };
            if value > max || (!allow_zero && value == 0) {
                return Err(format!("{name} outside supported range"));
            }
            Ok(value)
        }
        let mut c = Self::default();
        c.ratio = match std::env::var("OTEL_TRACES_SAMPLER").as_deref() {
            Err(_) | Ok("parentbased_always_on") => 1.0,
            Ok("parentbased_always_off") => 0.0,
            Ok("parentbased_traceidratio") => {
                let value = std::env::var("OTEL_TRACES_SAMPLER_ARG")
                    .map_err(|_| "OTEL_TRACES_SAMPLER_ARG is required")?
                    .parse::<f64>()
                    .map_err(|_| "invalid OTEL_TRACES_SAMPLER_ARG")?;
                if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                    return Err("OTEL_TRACES_SAMPLER_ARG must be between 0 and 1".into());
                }
                value
            }
            Ok(value) => {
                return Err(format!(
                    "unsupported OTEL_TRACES_SAMPLER: {value}; use a parentbased sampler"
                ))
            }
        };
        c.queue_capacity = number(
            "GST_OTEL_QUEUE_CAPACITY",
            c.queue_capacity,
            1_048_576,
            false,
        )?;
        c.batch_size = number(
            "GST_OTEL_BATCH_SIZE",
            c.batch_size.min(c.queue_capacity),
            c.queue_capacity,
            false,
        )?;
        c.concurrency = number("GST_OTEL_EXPORT_CONCURRENCY", 1, 4, false)?;
        c.interval = Duration::from_millis(number(
            "GST_OTEL_EXPORT_INTERVAL_MS",
            200,
            60_000,
            false,
        )? as u64);
        c.flush_timeout =
            Duration::from_millis(
                number("GST_OTEL_FLUSH_TIMEOUT_MS", 10_000, 60_000, false)? as u64
            );
        c.attribute_limit = number("OTEL_SPAN_ATTRIBUTE_COUNT_LIMIT", 128, 4096, true)?;
        c.value_length_limit = number(
            "OTEL_SPAN_ATTRIBUTE_VALUE_LENGTH_LIMIT",
            4096,
            1_048_576,
            false,
        )?;
        c.logs = std::env::var("GST_OTEL_LOGS").is_ok_and(|v| v == "1" || v == "true");
        Ok(c)
    }
}

pub(crate) fn resource() -> Resource {
    Resource::builder().build()
}

pub(crate) fn text_value(value: &str, limit: usize) -> Arc<str> {
    // The OTel string limit counts Unicode characters, not UTF-8 bytes.
    let end = value
        .char_indices()
        .nth(limit)
        .map_or(value.len(), |(i, _)| i);
    Arc::from(&value[..end])
}

pub(crate) struct Anchor {
    pub hook_ts: u64,
    pub wall: SystemTime,
}
impl Anchor {
    pub fn nanos(&self, ts: u64) -> u64 {
        self.at(ts)
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64
    }
    fn at(&self, ts: u64) -> SystemTime {
        if ts >= self.hook_ts {
            self.wall
                .checked_add(Duration::from_nanos(ts - self.hook_ts))
                .unwrap_or(self.wall)
        } else {
            self.wall
                .checked_sub(Duration::from_nanos(self.hook_ts - ts))
                .unwrap_or(self.wall)
        }
    }
}

#[derive(Default)]
pub(crate) struct ExportOutcome {
    pub rejected: u64,
    pub warned: bool,
}

pub(crate) trait RecordExporter: Send + 'static {
    fn fork(&self, _resource: &Resource) -> std::io::Result<Self>
    where
        Self: Sized,
    {
        Err(std::io::Error::other(
            "exporter does not support concurrent requests",
        ))
    }
    fn set_resource(&mut self, _resource: &Resource) {}
    fn export(
        &mut self,
        records: &[Record],
        anchor: &Anchor,
        config: &Config,
    ) -> Result<ExportOutcome, FlushError>;
    fn shutdown(&mut self, _timeout: Duration) -> Result<(), FlushError> {
        Ok(())
    }
}

pub(crate) struct OptionalExporter(pub Option<crate::transport::OtlpExporter>);
impl RecordExporter for OptionalExporter {
    fn fork(&self, resource: &Resource) -> std::io::Result<Self> {
        self.0
            .as_ref()
            .map(|_| crate::transport::OtlpExporter::new(resource))
            .transpose()
            .map(Self)
            .map_err(std::io::Error::other)
    }
    fn export(
        &mut self,
        records: &[Record],
        anchor: &Anchor,
        config: &Config,
    ) -> Result<ExportOutcome, FlushError> {
        self.0
            .as_mut()
            .ok_or(FlushError::Closed)?
            .export(records, anchor, config)
    }
}

enum Message {
    Span(Record),
    Flush(Sender<Result<(), FlushError>>),
    Shutdown(Sender<Result<(), FlushError>>),
}

pub(crate) struct Runtime {
    pub config: Config,
    pub stats: Arc<Counters>,
    pub metrics: Option<crate::metrics::Metrics>,
    pub shared_metrics: Option<crate::shared_metrics::Metrics>,
    sender: Sender<Message>,
    accepting: AtomicBool,
    worker_done: Receiver<()>,
    submissions: [SubmissionCount; SHARDS],
    worker: Mutex<Option<JoinHandle<()>>>,
    anchor: Arc<OnceLock<Anchor>>,
    stopped: Arc<OnceLock<Result<(), FlushError>>>,
}
impl Runtime {
    pub fn new<E: RecordExporter>(
        config: Config,
        mut exporter: E,
        resource: Resource,
    ) -> std::io::Result<Self> {
        exporter.set_resource(&resource);
        let (sender, receiver) = bounded(if config.traces {
            config.queue_capacity
        } else {
            0
        });
        let stats = Arc::new(Counters::default());
        let anchor = Arc::new(OnceLock::new());
        let stopped = Arc::new(OnceLock::new());
        let worker_stopped = stopped.clone();
        let worker_config = config.clone();
        let worker_stats = stats.clone();
        let worker_anchor = anchor.clone();
        let exporter = if config.traces {
            Some(pool::BatchExporter::new(
                exporter,
                &config,
                stats.clone(),
                anchor.clone(),
                &resource,
            )?)
        } else {
            None
        };
        let (finished, worker_done) = bounded(0);
        let worker = if config.traces {
            Some(
                thread::Builder::new()
                    .name("mars-otel-tx".into())
                    .spawn(move || {
                        let _finished = finished;
                        worker_loop(
                            exporter.unwrap(),
                            receiver,
                            &worker_config,
                            &worker_stats,
                            &worker_anchor,
                            &worker_stopped,
                        );
                    })?,
            )
        } else {
            None
        };
        Ok(Self {
            config,
            stats,
            metrics: None,
            shared_metrics: None,
            sender,
            accepting: AtomicBool::new(true),
            worker_done,
            submissions: std::array::from_fn(|_| SubmissionCount::default()),
            worker: Mutex::new(worker),
            anchor,
            stopped,
        })
    }

    pub fn accepting(&self) -> bool {
        self.accepting.load(Relaxed)
    }

    pub fn anchor(&self, ts: u64) {
        self.anchor.get_or_init(|| Anchor {
            hook_ts: ts,
            wall: SystemTime::now(),
        });
    }

    pub fn timestamp(&self, ts: u64) -> u64 {
        self.anchor(ts);
        self.anchor.get().unwrap().nanos(ts)
    }

    pub fn metric_stats(&self) -> crate::metrics::MetricStats {
        if let Some(metrics) = &self.shared_metrics {
            metrics.stats()
        } else {
            self.metrics
                .as_ref()
                .map_or_else(Default::default, |m| m.stats())
        }
    }

    pub fn submit(&self, record: Record) -> bool {
        let index = shard_index();
        let stats = &self.stats.shards[index];
        let active = &self.submissions[index].0;
        stats.completed_spans.fetch_add(1, Relaxed);
        // SeqCst orders registration, the closed check, and shutdown's scan.
        // Any producer that sees open is registered before closure; any producer
        // registering after the shutdown scan must see closed and cannot enqueue.
        active.fetch_add(1, SeqCst);
        if !self.accepting.load(SeqCst) {
            active.fetch_sub(1, Release);
            stats.dropped_closed.fetch_add(1, Relaxed);
            return false;
        }
        let accepted = match self.sender.try_send(Message::Span(record)) {
            Ok(()) => {
                stats.enqueued_spans.fetch_add(1, Relaxed);
                gst_mars_metrics_core::monitoring::queue(
                    gst_mars_metrics_core::monitoring::Stage::Traces,
                    || self.sender.len(),
                );
                true
            }
            Err(error) => {
                let counter = match error {
                    TrySendError::Full(_) => &stats.dropped_queue_full,
                    TrySendError::Disconnected(_) => &stats.dropped_closed,
                };
                counter.fetch_add(1, Relaxed);
                false
            }
        };
        active.fetch_sub(1, Release);
        accepted
    }

    fn finish(&self, timeout: Duration, shutdown: bool) -> Result<ExportStats, FlushError> {
        if !self.config.traces {
            return Ok(self.stats.snapshot());
        }
        let start = Instant::now();
        let (tx, rx) = bounded(1);
        let message = if shutdown {
            Message::Shutdown(tx)
        } else {
            Message::Flush(tx)
        };
        self.sender.send_timeout(message, timeout).map_err(|e| {
            if e.is_timeout() {
                FlushError::Timeout
            } else {
                FlushError::Closed
            }
        })?;
        // A retry may queue another marker while the first shutdown is still
        // exporting. Crossbeam retains that queued marker (and its reply sender)
        // after the receiver exits, until all senders are dropped. Also watch
        // worker completion so retries and concurrent callers cannot wait on a
        // reply that will never arrive.
        crossbeam_channel::select! {
            recv(rx) -> result => result.map_err(|_| FlushError::Closed)??,
            recv(self.worker_done) -> _ => {
                if !shutdown {
                    return Err(FlushError::Closed);
                }
                self.stopped.get().cloned().unwrap_or(Err(FlushError::Closed))?;
            }
            default(timeout.saturating_sub(start.elapsed())) => return Err(FlushError::Timeout),
        }
        Ok(self.stats.snapshot())
    }

    #[cfg(test)]
    pub fn flush(&self, timeout: Duration) -> Result<ExportStats, FlushError> {
        let (traces, metrics) = self.flush_parts(timeout);
        traces.and_then(|stats| metrics.map(|()| stats))
    }

    pub fn flush_parts(
        &self,
        timeout: Duration,
    ) -> (Result<ExportStats, FlushError>, Result<(), FlushError>) {
        let start = Instant::now();
        let traces = self.finish(timeout, false);
        let metrics = self
            .metrics
            .as_ref()
            .map_or(Ok(()), |m| m.flush(timeout.saturating_sub(start.elapsed())));
        let shared = self
            .shared_metrics
            .as_ref()
            .map_or(Ok(()), |m| m.flush(timeout.saturating_sub(start.elapsed())));
        (traces, metrics.and(shared))
    }

    #[cfg(test)]
    pub fn shutdown(&self, timeout: Duration) -> Result<ExportStats, FlushError> {
        let (traces, metrics) = self.shutdown_parts(timeout);
        traces.and_then(|stats| metrics.map(|()| stats))
    }

    pub fn shutdown_parts(
        &self,
        timeout: Duration,
    ) -> (Result<ExportStats, FlushError>, Result<(), FlushError>) {
        let start = Instant::now();
        if let Some(metrics) = &self.metrics {
            metrics.request_stop();
        }
        if let Some(metrics) = &self.shared_metrics {
            metrics.request_stop();
        }
        let traces = self.shutdown_traces(timeout);
        let metrics = self.metrics.as_ref().map_or(Ok(()), |m| {
            m.shutdown(timeout.saturating_sub(start.elapsed()))
        });
        let shared = self.shared_metrics.as_ref().map_or(Ok(()), |m| {
            m.shutdown(timeout.saturating_sub(start.elapsed()))
        });
        (traces, metrics.and(shared))
    }

    fn shutdown_traces(&self, timeout: Duration) -> Result<ExportStats, FlushError> {
        let start = Instant::now();
        self.accepting.store(false, SeqCst);
        while self
            .submissions
            .iter()
            .any(|active| active.0.load(SeqCst) != 0)
        {
            if start.elapsed() >= timeout {
                return Err(FlushError::Timeout);
            }
            thread::yield_now();
        }
        let result = match self.stopped.get() {
            Some(result) => result.clone().map(|()| self.stats.snapshot()),
            None => self.finish(timeout.saturating_sub(start.elapsed()), true),
        };
        let mut worker = self.worker.lock().unwrap();
        if worker.as_ref().is_some_and(JoinHandle::is_finished) {
            if let Some(handle) = worker.take() {
                let _ = handle.join();
            }
        }
        if result == Err(FlushError::Closed) {
            if let Some(result) = self.stopped.get() {
                return result.clone().map(|()| self.stats.snapshot());
            }
        }
        result
    }
}

#[cfg(test)]
pub(crate) fn span_data(
    record: Record,
    anchor: &Anchor,
    config: &Config,
    scope: &InstrumentationScope,
) -> SpanData {
    let Token {
        trace_id,
        span_id,
        flags,
        trace_state,
        is_remote,
    } = record.token;
    let mut attributes = Vec::with_capacity(record.site.attributes.len() + 9);
    attributes.push(KeyValue::new(
        "gst.operation",
        text_value(record.operation.name(), config.value_length_limit).to_string(),
    ));
    attributes.extend(record.site.attributes.iter().cloned());
    attributes.extend([
        KeyValue::new("ts.start", record.start as i64),
        KeyValue::new("ts.end", record.end as i64),
        KeyValue::new("buffer.id", record.buffer_id as i64),
        KeyValue::new("buffer.size", record.buffer_size as i64),
        KeyValue::new("thread.id", record.thread.id.clone()),
        KeyValue::new("thread.name", record.thread.name.clone()),
        KeyValue::new("gst.flow_return", record.flow_return as i64),
    ]);
    let omitted_profile = record.profiled && config.value_length_limit < 16;
    if record.profiled && !omitted_profile {
        attributes.push(KeyValue::new("pyroscope.profile.id", span_id.to_string()));
    }
    if let Some(length) = record.list_len {
        attributes.push(KeyValue::new("buffer.list.length", length as i64));
    }
    let dropped =
        attributes.len().saturating_sub(config.attribute_limit) + usize::from(omitted_profile);
    attributes.truncate(config.attribute_limit);
    SpanData {
        span_context: SpanContext::new(
            trace_id,
            span_id,
            flags,
            is_remote,
            trace_state.as_deref().cloned().unwrap_or_default(),
        ),
        parent_span_id: record.parent,
        parent_span_is_remote: record.parent_is_remote,
        span_kind: SpanKind::Internal,
        name: record.site.span_name(record.operation).to_owned().into(),
        start_time: anchor.at(record.start),
        end_time: anchor.at(record.end.max(record.start)),
        attributes,
        dropped_attributes_count: dropped as u32,
        events: Default::default(),
        links: record
            .links
            .as_ref()
            .map_or_else(Default::default, |links| links.sdk()),
        status: record
            .operation
            .error(record.flow_return)
            .map_or(Status::Unset, Status::error),
        instrumentation_scope: scope.clone(),
    }
}

fn worker_loop<E: RecordExporter>(
    mut exporter: pool::BatchExporter<E>,
    receiver: Receiver<Message>,
    config: &Config,
    stats: &Counters,
    anchor: &OnceLock<Anchor>,
    stopped: &OnceLock<Result<(), FlushError>>,
) {
    // Exporter instrumentation must not recursively trace exporter work.
    let _suppression = opentelemetry::Context::enter_telemetry_suppressed_scope();
    let stats = stats.local();
    let mut batch = Vec::with_capacity(config.batch_size);
    let mut failed_since_flush = false;
    let mut deadline = Instant::now() + config.interval;
    let mut ready = None;
    let mut drained = 0usize;
    loop {
        let message = ready.take().map(Ok).unwrap_or_else(|| {
            receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        });
        match message {
            Ok(Message::Span(record)) => {
                batch.push(record);
                drained += 1;
                if batch.len() < config.batch_size {
                    if drained < 64 {
                        if let Ok(next) = receiver.try_recv() {
                            ready = Some(next);
                            continue;
                        }
                    }
                    drained = 0;
                    if Instant::now() < deadline {
                        continue;
                    }
                }
            }
            Ok(Message::Flush(reply)) => {
                failed_since_flush |= exporter.submit(&mut batch, stats, anchor, config).is_err();
                failed_since_flush |= exporter.drain().is_err();
                let result = if failed_since_flush {
                    Err(FlushError::ExportFailed)
                } else {
                    Ok(())
                };
                failed_since_flush = false;
                let _ = reply.send(result);
                deadline = Instant::now() + config.interval;
                continue;
            }
            Ok(Message::Shutdown(reply)) => {
                failed_since_flush |= exporter.submit(&mut batch, stats, anchor, config).is_err();
                failed_since_flush |= exporter.shutdown(config.flush_timeout).is_err();
                let result = if failed_since_flush {
                    Err(FlushError::ExportFailed)
                } else {
                    Ok(())
                };
                let _ = stopped.set(result.clone());
                let _ = reply.send(result);
                break;
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                let _ = exporter.submit(&mut batch, stats, anchor, config);
                let _ = exporter.shutdown(config.flush_timeout);
                break;
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
        }
        failed_since_flush |= exporter.submit(&mut batch, stats, anchor, config).is_err();
        deadline = Instant::now() + config.interval;
        drained = 0;
    }
}

fn export_batch<E: RecordExporter>(
    exporter: &mut E,
    batch: &mut Vec<Record>,
    stats: &LocalCounters,
    anchor: &OnceLock<Anchor>,
    config: &Config,
) -> Result<(), FlushError> {
    if batch.is_empty() {
        return Ok(());
    }
    let count = batch.len() as u64;
    gst_mars_metrics_core::monitoring::record_size(
        gst_mars_metrics_core::monitoring::Stage::Traces,
        0,
        batch.len(),
    );
    let result = exporter.export(
        batch,
        anchor.get().expect("capture sets clock anchor"),
        config,
    );
    batch.clear();
    match result {
        Ok(outcome) => {
            stats
                .exported_spans
                .fetch_add(count - outcome.rejected, Relaxed);
            stats.rejected_spans.fetch_add(outcome.rejected, Relaxed);
            stats
                .export_warnings
                .fetch_add(u64::from(outcome.warned), Relaxed);
            if outcome.rejected == 0 {
                return Ok(());
            }
            stats
                .failed_export_spans
                .fetch_add(outcome.rejected, Relaxed);
        }
        Err(_) => {
            stats.failed_export_spans.fetch_add(count, Relaxed);
        }
    }
    stats.failed_exports.fetch_add(1, Relaxed);
    Err(FlushError::ExportFailed)
}

#[cfg(test)]
impl<E: SpanExporter + 'static> RecordExporter for E {
    fn set_resource(&mut self, resource: &Resource) {
        SpanExporter::set_resource(self, resource);
    }
    fn export(
        &mut self,
        records: &[Record],
        anchor: &Anchor,
        config: &Config,
    ) -> Result<ExportOutcome, FlushError> {
        let scope = InstrumentationScope::builder("mars-otel")
            .with_version(env!("CARGO_PKG_VERSION"))
            .build();
        futures_executor::block_on(SpanExporter::export(
            self,
            records
                .iter()
                .cloned()
                .map(|r| span_data(r, anchor, config, &scope))
                .collect(),
        ))
        .map(|()| ExportOutcome::default())
        .map_err(|_| FlushError::ExportFailed)
    }
    fn shutdown(&mut self, timeout: Duration) -> Result<(), FlushError> {
        self.shutdown_with_timeout(timeout)
            .map_err(|_| FlushError::ExportFailed)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::capture::{Site, ThreadInfo};
    use opentelemetry::trace::{SpanId, TraceFlags, TraceId, TraceState};
    use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};

    #[derive(Debug, Clone, Default)]
    pub struct Collector(pub Arc<Mutex<Vec<SpanData>>>);
    impl SpanExporter for Collector {
        async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
            self.0.lock().unwrap().extend(batch);
            Ok(())
        }
    }
    pub fn record(index: u64) -> Record {
        Record {
            operation: crate::capture::Operation::Push,
            site: Arc::new(Site {
                span_names: Default::default(),
                metrics: Default::default(),
                metric_rejected_epoch: Default::default(),
                encoded: Default::default(),
                attributes: vec![KeyValue::new("src_pad.name", "src")],
                epoch: 0,
            }),
            token: Token {
                trace_id: TraceId::from(1),
                span_id: SpanId::from(index + 1),
                flags: TraceFlags::SAMPLED,
                is_remote: false,
                trace_state: Some(Arc::new(
                    TraceState::from_key_value([("vendor", "value")]).unwrap(),
                )),
            },
            parent: SpanId::from(7),
            parent_is_remote: true,
            start: 100,
            end: 150,
            buffer_id: 123,
            buffer_size: 256,
            list_len: None,
            links: None,
            thread: Arc::new(ThreadInfo {
                encoded: Default::default(),
                id: "thread1".into(),
                name: "stream".into(),
            }),
            flow_return: 0,
            profiled: false,
        }
    }
    pub fn runtime(config: Config, collector: Collector) -> Runtime {
        let runtime = Runtime::new(config, collector, Resource::builder_empty().build()).unwrap();
        runtime.anchor(100);
        runtime
    }

    #[test]
    fn enriches_records_and_preserves_context_and_limits() {
        let collector = Collector::default();
        let runtime = runtime(
            Config {
                attribute_limit: 2,
                ..Config::default()
            },
            collector.clone(),
        );
        runtime.submit(record(1));
        let stats = runtime.shutdown(Duration::from_secs(2)).unwrap();
        assert_eq!(stats.exported_spans, 1);
        assert_eq!(stats.pending_spans, 0);
        let spans = collector.0.lock().unwrap();
        let span = &spans[0];
        assert_eq!(span.parent_span_id, SpanId::from(7));
        assert!(span.parent_span_is_remote);
        assert!(!span.span_context.is_remote());
        assert_eq!(span.span_context.trace_state().get("vendor"), Some("value"));
        assert_eq!(span.attributes.len(), 2);
        assert_eq!(span.dropped_attributes_count, 7);
        assert_eq!(
            span.end_time.duration_since(span.start_time).unwrap(),
            Duration::from_nanos(50)
        );
        assert_eq!(&*text_value("aé🙂z", 3), "aé🙂");
    }

    #[derive(Debug)]
    struct GatedExporter {
        entered: Sender<()>,
        release: Receiver<()>,
        fail: bool,
    }
    impl SpanExporter for GatedExporter {
        async fn export(&self, _: Vec<SpanData>) -> OTelSdkResult {
            self.entered.send(()).unwrap();
            self.release.recv().unwrap();
            if self.fail {
                Err(OTelSdkError::InternalFailure("receiver unavailable".into()))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn shutdown_retry_observes_completion_of_the_first_marker() {
        for fail in [false, true] {
            let (entered, entering) = bounded(1);
            let (release, waiting) = bounded(1);
            let runtime = Arc::new(
                Runtime::new(
                    Config {
                        interval: Duration::from_secs(60),
                        ..Config::default()
                    },
                    GatedExporter {
                        entered,
                        release: waiting,
                        fail,
                    },
                    Resource::builder_empty().build(),
                )
                .unwrap(),
            );
            runtime.anchor(100);
            runtime.submit(record(0));
            assert_eq!(
                runtime.shutdown(Duration::from_millis(5)),
                Err(FlushError::Timeout)
            );
            entering.recv_timeout(Duration::from_secs(2)).unwrap();
            let retry = runtime.clone();
            let worker = thread::spawn(move || retry.shutdown(Duration::from_secs(2)));
            // Ensure the retry's marker is queued behind the in-flight shutdown.
            let start = Instant::now();
            while runtime.sender.is_empty() {
                assert!(start.elapsed() < Duration::from_secs(2));
                thread::yield_now();
            }
            release.send(()).unwrap();
            let result = worker.join().unwrap();
            if fail {
                assert_eq!(result, Err(FlushError::ExportFailed));
            } else {
                assert_eq!(result.unwrap().exported_spans, 1);
            }
            assert_eq!(runtime.stats.snapshot().pending_spans, 0);
        }
    }

    #[test]
    fn slow_receiver_bounds_queue_and_failed_exports_are_counted() {
        let (entered_tx, entered_rx) = bounded(10);
        let (release_tx, release_rx) = bounded(10);
        let runtime = Runtime::new(
            Config {
                queue_capacity: 2,
                batch_size: 1,
                ..Config::default()
            },
            GatedExporter {
                entered: entered_tx,
                release: release_rx,
                fail: true,
            },
            Resource::builder_empty().build(),
        )
        .unwrap();
        runtime.anchor(100);
        runtime.submit(record(0));
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        for n in 1..11 {
            runtime.submit(record(n));
        }
        assert_eq!(runtime.stats.snapshot().dropped_queue_full, 8);
        assert_eq!(runtime.stats.snapshot().pending_spans, 3);
        assert_eq!(
            runtime.flush(Duration::from_millis(5)),
            Err(FlushError::Timeout)
        );
        for _ in 0..3 {
            release_tx.send(()).unwrap();
        }
        assert_eq!(
            runtime.flush(Duration::from_secs(2)),
            Err(FlushError::ExportFailed)
        );
        let stats = runtime.shutdown(Duration::from_secs(2)).unwrap();
        assert_eq!(stats.failed_export_spans, 3);
        assert_eq!(stats.failed_exports, 3);
        assert_eq!(
            stats.completed_spans,
            stats.failed_export_spans + stats.dropped_queue_full
        );
        assert_eq!(stats.pending_spans, 0);
        runtime.submit(record(50));
        assert_eq!(runtime.stats.snapshot().dropped_closed, 1);
    }

    #[test]
    fn counters_survive_producer_retirement_and_shard_reuse() {
        let runtime = Arc::new(runtime(
            Config {
                queue_capacity: 256,
                batch_size: 32,
                ..Config::default()
            },
            Collector::default(),
        ));
        for _ in 0..SHARDS * 2 + 1 {
            let producer = runtime.clone();
            thread::spawn(move || {
                for i in 0..20 {
                    producer.submit(record(i));
                }
            })
            .join()
            .unwrap();
        }
        let stats = runtime.shutdown(Duration::from_secs(5)).unwrap();
        assert_eq!(stats.completed_spans, (SHARDS as u64 * 2 + 1) * 20);
        assert_eq!(
            stats.completed_spans,
            stats.exported_spans + stats.dropped_queue_full
        );
        assert_eq!(stats.pending_spans, 0);
    }

    #[test]
    fn concurrent_shutdown_accounts_for_every_completed_record() {
        let runtime = Arc::new(runtime(
            Config {
                queue_capacity: 8,
                batch_size: 4,
                ..Config::default()
            },
            Collector::default(),
        ));
        let producers: Vec<_> = (0..4)
            .map(|_| {
                let runtime = runtime.clone();
                thread::spawn(move || {
                    for i in 0..2000 {
                        runtime.submit(record(i));
                    }
                })
            })
            .collect();
        runtime.shutdown(Duration::from_secs(5)).unwrap();
        for producer in producers {
            producer.join().unwrap();
        }
        let stats = runtime.stats.snapshot();
        assert_eq!(stats.completed_spans, 8000);
        assert_eq!(
            stats.completed_spans,
            stats.exported_spans + stats.dropped_queue_full + stats.dropped_closed
        );
        assert_eq!(stats.pending_spans, 0);
        runtime.shutdown(Duration::from_secs(1)).unwrap();
    }
}
