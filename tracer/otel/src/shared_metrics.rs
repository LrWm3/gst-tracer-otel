//! Full collector with selectable export. Network and encoding stay off the
//! streaming threads; flush and shutdown share the caller's deadline.
use crate::{
    export::FlushError,
    metrics::MetricStats,
    settings::{MetricExporter, Settings},
    shared_metrics_wire as wire,
};
use crossbeam_channel::{bounded, Receiver, Sender};
use gst_mars_metrics_core as core;
use opentelemetry_proto::{
    tonic::{
        collector::metrics::v1::ExportMetricsServiceRequest,
        common::v1::InstrumentationScope,
        metrics::v1::{ResourceMetrics, ScopeMetrics},
        resource::v1::Resource as ProtoResource,
    },
    transform::common::tonic::ResourceAttributesWithSchema,
};
use opentelemetry_sdk::Resource;
use prost::Message;
use std::{
    sync::{
        atomic::{
            AtomicBool, AtomicU64, AtomicUsize,
            Ordering::{Acquire, Relaxed, Release},
        },
        Arc, OnceLock,
    },
    time::{Duration, Instant, SystemTime},
};

enum Command {
    Flush(Sender<Result<(), FlushError>>),
    Wake,
}
#[derive(Default)]
struct State {
    stop: AtomicBool,
    result: OnceLock<Result<(), FlushError>>,
    exported: AtomicU64,
    failed: AtomicU64,
    warnings: AtomicU64,
    series: AtomicUsize,
}
pub(crate) struct Metrics {
    state: Arc<State>,
    command: Sender<Command>,
    done: Receiver<()>,
}

impl Metrics {
    pub fn new(settings: &Settings, resource: &Resource) -> Result<Self, String> {
        core::validate_settings()?;
        let exporter = if settings.exporter == MetricExporter::Otlp {
            Some(crate::transport::HttpExporter::new("METRICS")?)
        } else {
            None
        };
        // Bind before starting the worker so a conflicting port fails visibly.
        let server = if settings.exporter == MetricExporter::Prometheus {
            Some(tiny_http::Server::http(("0.0.0.0", settings.port)).map_err(|e| e.to_string())?)
        } else {
            None
        };
        let resource = ResourceAttributesWithSchema::from(resource);
        let interval = settings.interval;
        let state = Arc::new(State::default());
        let worker_state = state.clone();
        let (command, commands) = bounded(1);
        let (finished, done) = bounded(0);
        std::thread::Builder::new()
            .name("mars-metrics-tx".into())
            .spawn(move || {
                let _finished = finished;
                let _suppression = opentelemetry::Context::enter_telemetry_suppressed_scope();
                let mut failed = false;
                let mut deadline = Instant::now() + interval;
                let send = || -> Result<(), FlushError> {
                    let Some(exporter) = &exporter else {
                        return Ok(());
                    };
                    let examples = core::exemplars::snapshot();
                    let families = core::snapshot();
                    worker_state.series.store(
                        families
                            .iter()
                            .filter(|f| !f.name().starts_with("mars_instrument_"))
                            .map(|f| f.get_metric().len())
                            .sum(),
                        Relaxed,
                    );
                    let now = SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos() as u64;
                    let metrics = wire::otlp(&families, &examples, now);
                    if metrics.is_empty() {
                        return Ok(());
                    }
                    wire::record_count(&metrics);
                    let request = ExportMetricsServiceRequest {
                        resource_metrics: vec![ResourceMetrics {
                            resource: Some(ProtoResource {
                                attributes: resource.attributes.0.clone(),
                                ..Default::default()
                            }),
                            schema_url: resource.schema_url.clone().unwrap_or_default(),
                            scope_metrics: vec![ScopeMetrics {
                                scope: Some(InstrumentationScope {
                                    name: "mars-instrument".into(),
                                    version: env!("CARGO_PKG_VERSION").into(),
                                    ..Default::default()
                                }),
                                metrics,
                                ..Default::default()
                            }],
                        }],
                    };
                    let result = exporter
                        .send_response(request.encode_to_vec().into())
                        .and_then(|body| crate::metrics::check_response(&body));
                    if let Ok(warned) = result {
                        worker_state.exported.fetch_add(1, Relaxed);
                        worker_state.warnings.fetch_add(u64::from(warned), Relaxed);
                    } else {
                        worker_state.failed.fetch_add(1, Relaxed);
                    }
                    result.map(|_| ())
                };
                loop {
                    if worker_state.stop.load(Acquire) {
                        failed |= send().is_err();
                        let _ = worker_state.result.set(if failed {
                            Err(FlushError::ExportFailed)
                        } else {
                            Ok(())
                        });
                        break;
                    }
                    if let Some(server) = &server {
                        if let Ok(Some(request)) = server.recv_timeout(Duration::from_millis(20)) {
                            if request.url() != "/metrics"
                                || request.method() != &tiny_http::Method::Get
                            {
                                let _ = request.respond(tiny_http::Response::empty(404));
                            } else {
                                let examples = core::exemplars::snapshot();
                                let families = core::snapshot();
                                worker_state.series.store(
                                    families
                                        .iter()
                                        .filter(|f| !f.name().starts_with("mars_instrument_"))
                                        .map(|f| f.get_metric().len())
                                        .sum(),
                                    Relaxed,
                                );
                                let body = wire::openmetrics(&families, &examples);
                                let header = tiny_http::Header::from_bytes(
                                    "Content-Type",
                                    "application/openmetrics-text; version=1.0.0; charset=utf-8",
                                )
                                .unwrap();
                                if request
                                    .respond(
                                        tiny_http::Response::from_string(body).with_header(header),
                                    )
                                    .is_ok()
                                {
                                    worker_state.exported.fetch_add(1, Relaxed);
                                } else {
                                    worker_state.failed.fetch_add(1, Relaxed);
                                }
                            }
                        }
                    }
                    let wait = if server.is_some() {
                        Duration::ZERO
                    } else {
                        deadline.saturating_duration_since(Instant::now())
                    };
                    match commands.recv_timeout(wait) {
                        Ok(Command::Flush(reply)) => {
                            failed |= send().is_err();
                            let _ = reply.send(if failed {
                                Err(FlushError::ExportFailed)
                            } else {
                                Ok(())
                            });
                            failed = false;
                            deadline = Instant::now() + interval;
                        }
                        Ok(Command::Wake) => {}
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                            if server.is_none() {
                                failed |= send().is_err();
                                deadline = Instant::now() + interval;
                            }
                        }
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        core::enable_accounting();
        Ok(Self {
            state,
            command,
            done,
        })
    }
    pub fn stats(&self) -> MetricStats {
        MetricStats {
            recorded_pushes: core::recorded_pushes(),
            exported_batches: self.state.exported.load(Relaxed),
            failed_exports: self.state.failed.load(Relaxed),
            export_warnings: self.state.warnings.load(Relaxed),
            active_series: self.state.series.load(Relaxed),
            ..Default::default()
        }
    }
    pub fn flush(&self, timeout: Duration) -> Result<(), FlushError> {
        let start = Instant::now();
        let (reply, result) = bounded(1);
        self.command
            .send_timeout(Command::Flush(reply), timeout)
            .map_err(|e| {
                if e.is_timeout() {
                    FlushError::Timeout
                } else {
                    FlushError::Closed
                }
            })?;
        result
            .recv_timeout(timeout.saturating_sub(start.elapsed()))
            .map_err(|e| {
                if e.is_timeout() {
                    FlushError::Timeout
                } else {
                    FlushError::Closed
                }
            })?
    }
    pub fn request_stop(&self) {
        core::background::request_stop();
        self.state.stop.store(true, Release);
        let _ = self.command.try_send(Command::Wake);
    }
    pub fn shutdown(&self, timeout: Duration) -> Result<(), FlushError> {
        let start = Instant::now();
        self.request_stop();
        if self
            .done
            .recv_timeout(timeout)
            .is_err_and(|e| e.is_timeout())
        {
            return Err(FlushError::Timeout);
        }
        if !core::background::shutdown(timeout.saturating_sub(start.elapsed())) {
            return Err(FlushError::Timeout);
        }
        self.state
            .result
            .get()
            .cloned()
            .unwrap_or(Err(FlushError::Closed))
    }
}
impl Drop for Metrics {
    fn drop(&mut self) {
        self.request_stop();
    }
}
