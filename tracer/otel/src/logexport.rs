//! Bounded log delivery. Cleanup callers wait on a deadline; the worker owns I/O.
use crate::{export::FlushError, transport::HttpExporter};
use crossbeam_channel::{bounded, Receiver, Sender};
use opentelemetry_proto::{
    tonic::{
        collector::logs::v1::{ExportLogsServiceRequest, ExportLogsServiceResponse},
        common::v1::InstrumentationScope,
        logs::v1::{LogRecord, ResourceLogs, ScopeLogs},
        resource::v1::Resource as ProtoResource,
    },
    transform::common::tonic::ResourceAttributesWithSchema,
};
use opentelemetry_sdk::Resource;
use prost::Message as _;
use std::{
    sync::{
        atomic::{
            AtomicBool, AtomicU64, AtomicUsize,
            Ordering::{Relaxed, SeqCst},
        },
        Arc, OnceLock,
    },
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Default)]
pub struct LogStats {
    pub enqueued: u64,
    pub exported: u64,
    pub failed: u64,
    pub rejected: u64,
    pub dropped_queue_full: u64,
    pub dropped_closed: u64,
    pub failed_exports: u64,
    pub warnings: u64,
    pub pending: u64,
}
#[derive(Default)]
struct State {
    closed: AtomicBool,
    submitting: AtomicUsize,
    stopped: OnceLock<Result<(), FlushError>>,
    enqueued: AtomicU64,
    exported: AtomicU64,
    failed: AtomicU64,
    rejected: AtomicU64,
    dropped_queue_full: AtomicU64,
    dropped_closed: AtomicU64,
    failed_exports: AtomicU64,
    warnings: AtomicU64,
}
enum Command {
    Record(Box<LogRecord>),
    Flush(Sender<Result<(), FlushError>>),
    Wake,
}
pub(crate) struct Logs {
    state: Arc<State>,
    tx: Sender<Command>,
    done: Receiver<()>,
}
impl Logs {
    pub fn new(resource: Resource) -> Result<Self, String> {
        fn number(key: &str, default: usize, max: usize) -> Result<usize, String> {
            let value = std::env::var(key).map_or(Ok(default), |s| {
                s.parse::<usize>().map_err(|_| format!("invalid {key}"))
            })?;
            if value == 0 || value > max {
                return Err(format!("{key} outside 1..={max}"));
            }
            Ok(value)
        }
        let capacity = number("OTEL_BLRP_MAX_QUEUE_SIZE", 2048, 65536)?;
        let batch_size = number(
            "OTEL_BLRP_MAX_EXPORT_BATCH_SIZE",
            512.min(capacity),
            capacity,
        )?;
        let interval =
            Duration::from_millis(number("OTEL_BLRP_SCHEDULE_DELAY", 1000, 60000)? as u64);
        let http = HttpExporter::new("LOGS")?;
        Self::start(capacity, batch_size, interval, move |records| {
            let resource = ResourceAttributesWithSchema::from(&resource);
            let count = records.len() as u64;
            let request = ExportLogsServiceRequest {
                resource_logs: vec![ResourceLogs {
                    resource: Some(ProtoResource {
                        attributes: resource.attributes.0,
                        ..Default::default()
                    }),
                    schema_url: resource.schema_url.unwrap_or_default(),
                    scope_logs: vec![ScopeLogs {
                        scope: Some(InstrumentationScope {
                            name: "mars-otel".into(),
                            version: env!("CARGO_PKG_VERSION").into(),
                            ..Default::default()
                        }),
                        log_records: std::mem::take(records),
                        ..Default::default()
                    }],
                }],
            };
            if request.encoded_len() > crate::wire::MAX_BODY {
                return Err(FlushError::ExportFailed);
            }
            let response = http.send_response(request.encode_to_vec().into())?;
            let response = ExportLogsServiceResponse::decode(response.as_slice())
                .map_err(|_| FlushError::ExportFailed)?;
            let Some(partial) = response.partial_success else {
                return Ok((0, false));
            };
            let rejected = u64::try_from(partial.rejected_log_records)
                .map_err(|_| FlushError::ExportFailed)?;
            if rejected > count {
                return Err(FlushError::ExportFailed);
            }
            // Diagnostics are counters, not GStreamer logs: logging this export
            // would recursively feed the same failing destination.
            Ok((rejected, !partial.error_message.is_empty()))
        })
    }

    fn start(
        capacity: usize,
        batch_size: usize,
        interval: Duration,
        mut export: impl FnMut(&mut Vec<LogRecord>) -> Result<(u64, bool), FlushError> + Send + 'static,
    ) -> Result<Self, String> {
        let state = Arc::new(State::default());
        let worker_state = state.clone();
        let (tx, rx) = bounded(capacity);
        let (finished, done) = bounded(0);
        std::thread::Builder::new()
            .name("mars-logs-tx".into())
            .spawn(move || {
                let _finished = finished;
                let _suppression = opentelemetry::Context::enter_telemetry_suppressed_scope();
                let state = worker_state;
                let mut batch = Vec::with_capacity(batch_size);
                let mut failed = false;
                let mut deadline = Instant::now() + interval;
                let mut send = |batch: &mut Vec<LogRecord>| {
                    if batch.is_empty() {
                        return false;
                    }
                    let count = batch.len() as u64;
                    let result = export(batch);
                    batch.clear();
                    let rejected = match result {
                        Ok((rejected, warned)) => {
                            state.exported.fetch_add(count - rejected, Relaxed);
                            state.rejected.fetch_add(rejected, Relaxed);
                            state.warnings.fetch_add(u64::from(warned), Relaxed);
                            rejected
                        }
                        Err(_) => count,
                    };
                    if rejected != 0 {
                        state.failed.fetch_add(rejected, Relaxed);
                        state.failed_exports.fetch_add(1, Relaxed);
                    }
                    rejected != 0
                };
                loop {
                    if state.closed.load(SeqCst)
                        && state.submitting.load(SeqCst) == 0
                        && rx.is_empty()
                    {
                        failed |= send(&mut batch);
                        let _ = state.stopped.set(if failed {
                            Err(FlushError::ExportFailed)
                        } else {
                            Ok(())
                        });
                        break;
                    }
                    match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                        Ok(Command::Record(record)) => batch.push(*record),
                        Ok(Command::Flush(reply)) => {
                            failed |= send(&mut batch);
                            let _ = reply.send(if failed {
                                Err(FlushError::ExportFailed)
                            } else {
                                Ok(())
                            });
                            failed = false;
                            deadline = Instant::now() + interval;
                        }
                        Ok(Command::Wake) | Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                    }
                    if batch.len() >= batch_size || Instant::now() >= deadline {
                        failed |= send(&mut batch);
                        deadline = Instant::now() + interval;
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self { state, tx, done })
    }
    pub fn emit(&self, record: LogRecord) {
        self.state.submitting.fetch_add(1, SeqCst);
        if self.state.closed.load(SeqCst) {
            self.state.dropped_closed.fetch_add(1, Relaxed);
        } else if self.tx.try_send(Command::Record(Box::new(record))).is_ok() {
            self.state.enqueued.fetch_add(1, Relaxed);
        } else {
            self.state.dropped_queue_full.fetch_add(1, Relaxed);
        }
        self.state.submitting.fetch_sub(1, SeqCst);
        if self.state.closed.load(SeqCst) {
            let _ = self.tx.try_send(Command::Wake);
        }
    }
    pub fn flush(&self, timeout: Duration) -> Result<(), FlushError> {
        if self.state.closed.load(SeqCst) {
            return Err(FlushError::Closed);
        }
        let start = Instant::now();
        let (tx, rx) = bounded(1);
        self.tx
            .send_timeout(Command::Flush(tx), timeout)
            .map_err(|e| {
                if e.is_timeout() {
                    FlushError::Timeout
                } else {
                    FlushError::Closed
                }
            })?;
        crossbeam_channel::select! {
            recv(rx) -> r => r.map_err(|_| FlushError::Closed)?,
            recv(self.done) -> _ => Err(FlushError::Closed),
            default(timeout.saturating_sub(start.elapsed())) => Err(FlushError::Timeout),
        }
    }
    pub fn shutdown(&self, timeout: Duration) -> Result<(), FlushError> {
        self.state.closed.store(true, SeqCst);
        let _ = self.tx.try_send(Command::Wake);
        match self.done.recv_timeout(timeout) {
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => Err(FlushError::Timeout),
            _ => self
                .state
                .stopped
                .get()
                .cloned()
                .unwrap_or(Err(FlushError::Closed)),
        }
    }
    pub fn stats(&self) -> LogStats {
        let s = &self.state;
        let mut stats = LogStats {
            enqueued: s.enqueued.load(Relaxed),
            exported: s.exported.load(Relaxed),
            failed: s.failed.load(Relaxed),
            rejected: s.rejected.load(Relaxed),
            dropped_queue_full: s.dropped_queue_full.load(Relaxed),
            dropped_closed: s.dropped_closed.load(Relaxed),
            failed_exports: s.failed_exports.load(Relaxed),
            warnings: s.warnings.load(Relaxed),
            pending: 0,
        };
        stats.pending = stats.enqueued.saturating_sub(stats.exported + stats.failed);
        stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deadlines_retries_and_concurrent_shutdown_observe_worker_completion() {
        for fail in [false, true] {
            let (entered, waiting) = bounded(1);
            let (release, gate) = bounded(1);
            let logs = Arc::new(
                Logs::start(8, 8, Duration::from_secs(60), move |_| {
                    entered.send(()).unwrap();
                    gate.recv().unwrap();
                    if fail {
                        Err(FlushError::ExportFailed)
                    } else {
                        Ok((0, false))
                    }
                })
                .unwrap(),
            );
            logs.emit(LogRecord::default());
            let first = logs.clone();
            let flush = std::thread::spawn(move || first.flush(Duration::from_millis(20)));
            waiting.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_eq!(flush.join().unwrap(), Err(FlushError::Timeout));
            assert_eq!(logs.shutdown(Duration::ZERO), Err(FlushError::Timeout));
            let second = logs.clone();
            let retry = std::thread::spawn(move || second.shutdown(Duration::from_secs(2)));
            release.send(()).unwrap();
            // The preceding flush reports its export error, even if its caller
            // timed out. Cumulative counters retain that loss after shutdown.
            retry.join().unwrap().unwrap();
            logs.shutdown(Duration::from_secs(2)).unwrap();
            let stats = logs.stats();
            assert_eq!(stats.pending, 0);
            assert_eq!(stats.failed, u64::from(fail));
            assert_eq!(stats.exported, u64::from(!fail));
        }
    }
    #[test]
    fn shutdown_reports_an_export_failure_and_stops_accepting_logs() {
        let logs = Logs::start(8, 8, Duration::from_secs(60), |_| {
            Err(FlushError::ExportFailed)
        })
        .unwrap();
        logs.emit(LogRecord::default());
        assert_eq!(
            logs.shutdown(Duration::from_secs(2)),
            Err(FlushError::ExportFailed)
        );
        assert_eq!(
            logs.shutdown(Duration::from_secs(2)),
            Err(FlushError::ExportFailed)
        );
        logs.emit(LogRecord::default());
        assert_eq!(logs.stats().dropped_closed, 1);
    }
}
