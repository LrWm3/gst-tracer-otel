//! Bounded cumulative push metrics. Streaming threads update cached atomic
//! aggregates; only the worker builds protobufs or touches the network.
use crate::{
    capture::Token,
    export::{shard_index, Anchor, FlushError, SHARDS},
    metadata::Site,
    transport::HttpExporter,
};
use crossbeam_channel::{bounded, Receiver, Sender};
use opentelemetry_proto::{
    tonic::{
        collector::metrics::v1::{ExportMetricsServiceRequest, ExportMetricsServiceResponse},
        common::v1::{InstrumentationScope, KeyValue},
        metrics::v1::{self as proto, metric::Data},
        resource::v1::Resource as ProtoResource,
    },
    transform::common::tonic::{Attributes, ResourceAttributesWithSchema},
};
use opentelemetry_sdk::Resource;
use prost::Message;
use std::{
    collections::HashMap,
    sync::{
        atomic::{
            AtomicBool, AtomicU64, AtomicUsize,
            Ordering::{Relaxed, Release, SeqCst},
        },
        Arc, Mutex, OnceLock,
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

const BOUNDS: [u64; 22] = [
    1_000,
    2_000,
    5_000,
    10_000,
    20_000,
    50_000,
    100_000,
    200_000,
    500_000,
    1_000_000,
    2_000_000,
    5_000_000,
    10_000_000,
    20_000_000,
    50_000_000,
    100_000_000,
    200_000_000,
    500_000_000,
    1_000_000_000,
    2_000_000_000,
    5_000_000_000,
    10_000_000_000,
];
const NAMES: [&str; 4] = [
    "gst.pad.push.wall",
    "gst.pad.push.exclusive.wall",
    "gst.pad.push.exclusive.cpu",
    "gst.pad.push.exclusive.blocked",
];
const DESCRIPTIONS: [&str; 4] = [
    "Inclusive synchronous pad push duration",
    "Synchronous pad push duration excluding direct nested pushes",
    "Sampled thread CPU duration excluding direct nested pushes",
    "Sampled exclusive wall minus thread CPU duration (includes scheduling delay)",
];
const EXPORT_CHUNK: usize = 32;

#[derive(Clone, Debug)]
pub(crate) struct Config {
    pub interval: Duration,
    pub max_series: usize,
    pub cpu_sample_rate: u64,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(10),
            max_series: 1024,
            cpu_sample_rate: 16,
        }
    }
}
impl Config {
    pub fn from_env() -> Result<Self, String> {
        let number = |key: &str, default, max| -> Result<u64, String> {
            let n = std::env::var(key)
                .map(|v| v.parse::<u64>())
                .unwrap_or(Ok(default))
                .map_err(|_| format!("invalid {key}"))?;
            if n == 0 || n > max {
                return Err(format!("{key} outside supported range"));
            }
            Ok(n)
        };
        Ok(Self {
            interval: Duration::from_millis(number(
                "GST_INSTRUMENT_METRIC_INTERVAL_MS",
                10_000,
                60_000,
            )?),
            max_series: number("GST_INSTRUMENT_METRIC_MAX_SERIES", 1024, 16_384)? as usize,
            cpu_sample_rate: number("GST_INSTRUMENT_CPU_SAMPLE_RATE", 16, 1_048_576)?,
        })
    }
}

macro_rules! counters {
    ($($name:ident),* $(,)?) => {
        /// Cumulative metrics accounting. Histogram snapshots are concurrent;
        /// counts and sums are exact after streaming stops. Export counts are HTTP
        /// successes, not acknowledgements of durable backend storage.
        #[derive(Debug, Default, Clone, Copy)]
        pub struct MetricStats { $(pub $name: u64,)* pub active_series: usize }
        impl MetricStats {
            pub fn to_structure(&self) -> gstreamer::Structure {
                gstreamer::Structure::builder("mars-instrument-metrics-stats")
                    $(.field(stringify!($name), self.$name))*
                    .field("active_series", self.active_series as u64).build()
            }
        }
        #[repr(align(128))]
        #[derive(Default)]
        struct LocalCounters { $($name: AtomicU64,)* active: AtomicUsize }
        struct Counters { shards: [LocalCounters; SHARDS] }
        impl Default for Counters {
            fn default() -> Self { Self { shards: std::array::from_fn(|_| LocalCounters::default()) } }
        }
        impl Counters {
            fn local(&self) -> &LocalCounters { &self.shards[shard_index()] }
            fn snapshot(&self, active_series: usize) -> MetricStats {
                let mut snapshot = MetricStats { active_series, ..Default::default() };
                for shard in &self.shards { $(snapshot.$name = snapshot.$name.wrapping_add(shard.$name.load(Relaxed));)* }
                snapshot
            }
            fn quiescent(&self) -> bool { self.shards.iter().all(|s| s.active.load(SeqCst) == 0) }
        }
    }
}
counters! { recorded_pushes, dropped_capacity, dropped_invalid, skipped_closed, skipped_exemplars, exported_batches, failed_exports, export_warnings }

#[derive(Clone, Copy)]
struct Example {
    trace_id: [u8; 16],
    span_id: [u8; 8],
    ts: u64,
    value: u64,
}
struct Histogram {
    buckets: [AtomicU64; BOUNDS.len() + 1],
    sum: AtomicU64,
    // Contention can skip an exemplar, never its histogram measurement.
    exemplar: Mutex<Option<Example>>,
}
impl Default for Histogram {
    fn default() -> Self {
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            sum: AtomicU64::new(0),
            exemplar: Mutex::new(None),
        }
    }
}
impl Histogram {
    fn observe(&self, value: u64, example: Option<Example>, counters: &LocalCounters) {
        self.sum.fetch_add(value, Relaxed);
        self.buckets[BOUNDS.partition_point(|bound| *bound < value)].fetch_add(1, Relaxed);
        if let Some(mut example) = example {
            example.value = value;
            if let Ok(mut slot) = self.exemplar.try_lock() {
                *slot = Some(example);
            } else {
                counters.skipped_exemplars.fetch_add(1, Relaxed);
            }
        }
    }
    fn snapshot(&self, series: &Series, anchor: &Anchor) -> Option<proto::HistogramDataPoint> {
        // Take the exemplar first: its observation is already in the aggregates.
        let example = self.exemplar.lock().unwrap().take();
        let bucket_counts: Vec<_> = self.buckets.iter().map(|b| b.load(Relaxed)).collect();
        let count = bucket_counts.iter().sum();
        if count == 0 {
            return None;
        }
        let sum = self.sum.load(Relaxed) as f64 / 1e9;
        let end = anchor.nanos(gstreamer::get_timestamp().nseconds());
        Some(proto::HistogramDataPoint {
            attributes: series.attributes.clone(),
            start_time_unix_nano: series.start,
            time_unix_nano: end,
            count,
            sum: Some(sum),
            bucket_counts,
            explicit_bounds: BOUNDS.iter().map(|b| *b as f64 / 1e9).collect(),
            exemplars: example
                .into_iter()
                .map(|e| proto::Exemplar {
                    time_unix_nano: anchor.nanos(e.ts),
                    span_id: e.span_id.to_vec(),
                    trace_id: e.trace_id.to_vec(),
                    value: Some(proto::exemplar::Value::AsDouble(e.value as f64 / 1e9)),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        })
    }
}

pub(crate) struct Series {
    attributes: Vec<KeyValue>,
    start: u64,
    histograms: [Histogram; 4],
    observations: AtomicU64,
}
struct Inner {
    config: Config,
    series: Mutex<HashMap<Vec<String>, Arc<Series>>>,
    counters: Counters,
    capacity_epoch: AtomicU64,
    accepting: AtomicBool,
    stop: AtomicBool,
    stopped: OnceLock<Result<(), FlushError>>,
    anchor: Anchor,
}
/// HTTP 200 may carry a warning even when every point was accepted.
/// Any nonzero rejection count (including invalid negative values) fails the
/// snapshot; the next cumulative snapshot is independent, not a batch retry.
pub(crate) fn check_response(body: &[u8]) -> Result<bool, FlushError> {
    let response =
        ExportMetricsServiceResponse::decode(body).map_err(|_| FlushError::ExportFailed)?;
    match response.partial_success {
        Some(partial) if partial.rejected_data_points != 0 => Err(FlushError::ExportFailed),
        Some(partial) => Ok(!partial.error_message.is_empty()),
        None => Ok(false),
    }
}

enum Command {
    Flush(Sender<Result<(), FlushError>>),
    Wake,
}
pub(crate) struct Metrics {
    inner: Arc<Inner>,
    command: Sender<Command>,
    done: Receiver<()>,
}
impl Metrics {
    pub fn new(config: Config, resource: &Resource) -> Result<Self, String> {
        let http = HttpExporter::new("METRICS")?;
        Self::start(config, resource, move |body| {
            let response = http.send_response(body.into())?;
            check_response(&response)
        })
    }
    fn start(
        config: Config,
        resource: &Resource,
        send: impl FnMut(Vec<u8>) -> Result<bool, FlushError> + Send + 'static,
    ) -> Result<Self, String> {
        let inner = Arc::new(Inner {
            config,
            series: Mutex::new(HashMap::new()),
            counters: Counters::default(),
            capacity_epoch: AtomicU64::new(1),
            accepting: AtomicBool::new(true),
            stop: AtomicBool::new(false),
            stopped: OnceLock::new(),
            anchor: Anchor {
                hook_ts: gstreamer::get_timestamp().nseconds(),
                wall: SystemTime::now(),
            },
        });
        let (command, commands) = bounded(1);
        let (finished, done) = bounded::<()>(0);
        let worker = inner.clone();
        let resource: ResourceAttributesWithSchema = resource.into();
        thread::Builder::new()
            .name("mars-instrument-metrics".into())
            .spawn(move || {
                let _finished = finished; // Dropping disconnects and wakes every waiter.
                worker_loop(&worker, resource, commands, send);
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            inner,
            command,
            done,
        })
    }
    pub fn cpu_sampled(&self, sequence: u64, pad: usize) -> bool {
        let rate = self.inner.config.cpu_sample_rate;
        if rate == 1 {
            return true;
        }
        // Same SplitMix64 selection as mars-metrics; avoids periodic aliasing.
        let mut n = (sequence ^ pad as u64).wrapping_add(0x9e37_79b9_7f4a_7c15);
        n = (n ^ (n >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        n = (n ^ (n >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        (n ^ (n >> 31)).is_multiple_of(rate)
    }
    pub fn series(&self, site: &Site) -> Option<Arc<Series>> {
        if !self.inner.accepting.load(Relaxed) {
            return None;
        }
        if let Some(series) = site.metrics.get() {
            return Some(series.clone());
        }
        // Full-capacity descriptors retry only after space is reclaimed. Avoid
        // allocations and registry contention on every subsequently dropped call.
        let epoch = self.inner.capacity_epoch.load(Relaxed);
        if site.metric_rejected_epoch.load(Relaxed) == epoch {
            self.inner
                .counters
                .local()
                .dropped_capacity
                .fetch_add(1, Relaxed);
            return None;
        }
        // Only cold descriptors enter this lock. No GObject calls, exporters,
        // or application callbacks run while it is held.
        let mut entries = self.inner.series.lock().unwrap();
        if let Some(series) = site.metrics.get() {
            return Some(series.clone());
        }
        let key: Vec<_> = site
            .attributes
            .iter()
            .map(|kv| kv.value.to_string().chars().take(256).collect::<String>())
            .collect();
        let series = if let Some(series) = entries.get(&key) {
            series.clone()
        } else {
            if entries.len() >= self.inner.config.max_series {
                site.metric_rejected_epoch.store(epoch, Relaxed);
                self.inner
                    .counters
                    .local()
                    .dropped_capacity
                    .fetch_add(1, Relaxed);
                return None;
            }
            let attributes =
                Attributes::from(site.attributes.iter().zip(&key).map(|(kv, value)| {
                    opentelemetry::KeyValue::new(kv.key.clone(), value.clone())
                }))
                .0;
            let series = Arc::new(Series {
                attributes,
                start: self
                    .inner
                    .anchor
                    .nanos(gstreamer::get_timestamp().nseconds()),
                observations: AtomicU64::new(0),
                histograms: std::array::from_fn(|_| Histogram::default()),
            });
            entries.insert(key, series.clone());
            series
        };
        let _ = site.metrics.set(series.clone());
        Some(series)
    }
    pub fn observe(&self, timing: Timing, end: u64, cpu: Option<u64>, token: Option<&Token>) {
        let inner = &self.inner;
        let counters = inner.counters.local();
        counters.active.fetch_add(1, SeqCst);
        if !inner.accepting.load(SeqCst) {
            counters.skipped_closed.fetch_add(1, Relaxed);
        } else if !timing.valid {
            counters.dropped_invalid.fetch_add(1, Relaxed);
        } else if let Some(series) = &timing.series {
            let wall = end.saturating_sub(timing.start);
            let exclusive = wall.saturating_sub(timing.child_wall);
            let example = token.filter(|t| t.sampled()).map(|t| Example {
                trace_id: t.trace_id.to_bytes(),
                span_id: t.span_id.to_bytes(),
                ts: end,
                value: 0,
            });
            series.histograms[0].observe(wall, example, counters);
            series.histograms[1].observe(exclusive, example, counters);
            if let Some(cpu) = timing.exclusive_cpu(cpu) {
                series.histograms[2].observe(cpu, example, counters);
                series.histograms[3].observe(exclusive.saturating_sub(cpu), example, counters);
            }
            series.observations.fetch_add(1, Release);
            counters.recorded_pushes.fetch_add(1, Relaxed);
        }
        counters.active.fetch_sub(1, Release);
    }
    pub fn stats(&self) -> MetricStats {
        self.inner
            .counters
            .snapshot(self.inner.series.lock().unwrap().len())
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
        self.inner.accepting.store(false, SeqCst);
        self.inner.stop.store(true, Release);
        let _ = self.command.try_send(Command::Wake);
    }
    pub fn shutdown(&self, timeout: Duration) -> Result<(), FlushError> {
        self.request_stop();
        if self
            .done
            .recv_timeout(timeout)
            .is_err_and(|e| e.is_timeout())
        {
            return Err(FlushError::Timeout);
        }
        self.inner
            .stopped
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

fn worker_loop(
    inner: &Inner,
    resource: ResourceAttributesWithSchema,
    commands: Receiver<Command>,
    mut send: impl FnMut(Vec<u8>) -> Result<bool, FlushError>,
) {
    let _suppression = opentelemetry::Context::enter_telemetry_suppressed_scope();
    let mut failed = false;
    let mut deadline = Instant::now() + inner.config.interval;
    loop {
        if inner.stop.load(std::sync::atomic::Ordering::Acquire) {
            while !inner.counters.quiescent() {
                thread::yield_now();
            }
            failed |= export(inner, &resource, &mut send).is_err();
            let _ = inner.stopped.set(if failed {
                Err(FlushError::ExportFailed)
            } else {
                Ok(())
            });
            break;
        }
        match commands.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Command::Flush(reply)) => {
                failed |= export(inner, &resource, &mut send).is_err();
                let _ = reply.send(if failed {
                    Err(FlushError::ExportFailed)
                } else {
                    Ok(())
                });
                failed = false;
            }
            Ok(Command::Wake) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                failed |= export(inner, &resource, &mut send).is_err();
            }
        }
        deadline = Instant::now() + inner.config.interval;
    }
}
fn export(
    inner: &Inner,
    resource: &ResourceAttributesWithSchema,
    send: &mut impl FnMut(Vec<u8>) -> Result<bool, FlushError>,
) -> Result<(), FlushError> {
    let entries: Vec<_> = inner
        .series
        .lock()
        .unwrap()
        .iter()
        .map(|(key, series)| {
            let retired = Arc::strong_count(series) == 1;
            let version = series
                .observations
                .load(std::sync::atomic::Ordering::Acquire);
            (key.clone(), series.clone(), retired, version)
        })
        .collect();
    let mut result = Ok(());
    // Include health once per snapshot, also when no pipeline series remain.
    let mut chunks: Vec<_> = entries.chunks(EXPORT_CHUNK).collect();
    if chunks.is_empty() {
        chunks.push(&[]);
    }
    for (index, chunk) in chunks.into_iter().enumerate() {
        let mut metrics = if index == 0 {
            let families = gst_mars_metrics_core::monitoring::snapshot();
            crate::shared_metrics_wire::otlp(
                &families,
                &Default::default(),
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64,
            )
        } else {
            Vec::new()
        };
        for index in 0..NAMES.len() {
            let points: Vec<_> = chunk
                .iter()
                .filter_map(|(_, s, _, _)| s.histograms[index].snapshot(s, &inner.anchor))
                .collect();
            if !points.is_empty() {
                metrics.push(proto::Metric {
                    name: NAMES[index].into(),
                    description: DESCRIPTIONS[index].into(),
                    unit: "s".into(),
                    data: Some(Data::Histogram(proto::Histogram {
                        data_points: points,
                        aggregation_temporality: proto::AggregationTemporality::Cumulative as i32,
                    })),
                    ..Default::default()
                });
            }
        }
        if metrics.is_empty() {
            let mut entries = inner.series.lock().unwrap();
            for (key, series, retired, version) in chunk {
                if *retired
                    && Arc::strong_count(series) == 2
                    && series
                        .observations
                        .load(std::sync::atomic::Ordering::Acquire)
                        == *version
                {
                    entries.remove(key);
                    inner.capacity_epoch.fetch_add(1, Relaxed);
                }
            }
            continue;
        }
        crate::shared_metrics_wire::record_count(&metrics);
        let request = ExportMetricsServiceRequest {
            resource_metrics: vec![proto::ResourceMetrics {
                resource: Some(ProtoResource {
                    attributes: resource.attributes.0.clone(),
                    ..Default::default()
                }),
                schema_url: resource.schema_url.clone().unwrap_or_default(),
                scope_metrics: vec![proto::ScopeMetrics {
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
        if let Ok(warned) = send(request.encode_to_vec()) {
            inner
                .counters
                .local()
                .export_warnings
                .fetch_add(u64::from(warned), Relaxed);
            inner
                .counters
                .local()
                .exported_batches
                .fetch_add(1, Relaxed);
            // A successful final snapshot releases descriptors no longer held by
            // pad metadata or in-flight calls. Failed exports remain bounded and
            // retry on the next cumulative snapshot, without a growing queue.
            let mut entries = inner.series.lock().unwrap();
            for (key, series, retired, version) in chunk {
                if *retired
                    && Arc::strong_count(series) == 2
                    && series
                        .observations
                        .load(std::sync::atomic::Ordering::Acquire)
                        == *version
                {
                    entries.remove(key);
                    inner.capacity_epoch.fetch_add(1, Relaxed);
                }
            }
        } else {
            inner.counters.local().failed_exports.fetch_add(1, Relaxed);
            result = Err(FlushError::ExportFailed);
        }
    }
    result
}

/// Timing lives on the same frame as the trace token. Direct children contribute
/// their inclusive time, including every sequential tee branch. CPU clocks are
/// needed for a sampled call and its immediate children only (as in mars-metrics).
pub(crate) struct Timing {
    pub series: Option<Arc<Series>>,
    pub start: u64,
    pub start_cpu: Option<u64>,
    pub sample_cpu: bool,
    pub valid: bool,
    pub child_wall: u64,
    pub child_cpu: u64,
    pub child_cpu_valid: bool,
}
impl Timing {
    pub fn new(
        series: Option<Arc<Series>>,
        start: u64,
        start_cpu: Option<u64>,
        sample_cpu: bool,
    ) -> Self {
        Self {
            series,
            start,
            start_cpu,
            sample_cpu,
            valid: true,
            child_wall: 0,
            child_cpu: 0,
            child_cpu_valid: true,
        }
    }
    pub fn finish_child(&mut self, child: &Self, end: u64, cpu: Option<u64>) {
        self.child_wall = self
            .child_wall
            .saturating_add(end.saturating_sub(child.start));
        if self.sample_cpu {
            if let Some(cpu) = child
                .start_cpu
                .and_then(|start| cpu.map(|end| end.saturating_sub(start)))
            {
                self.child_cpu = self.child_cpu.saturating_add(cpu);
            } else {
                self.child_cpu_valid = false;
            }
        }
    }
    fn exclusive_cpu(&self, end: Option<u64>) -> Option<u64> {
        self.start_cpu
            .zip(end)
            .filter(|_| self.sample_cpu && self.child_cpu_valid)
            .map(|(start, end)| end.saturating_sub(start).saturating_sub(self.child_cpu))
    }
}
pub(crate) fn cpu_time() -> Option<u64> {
    #[cfg(unix)]
    {
        let mut t = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: clock_gettime writes into a valid stack-allocated timespec.
        (unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut t) } == 0).then(|| {
            (t.tv_sec as u64)
                .saturating_mul(1_000_000_000)
                .saturating_add(t.tv_nsec as u64)
        })
    }
    #[cfg(not(unix))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::tests::record;
    use opentelemetry::trace::{SpanId, TraceId};

    #[test]
    fn direct_children_are_subtracted_even_when_sequential_or_unsampled() {
        let mut parent = Timing::new(None, 0, Some(0), true);
        let first = Timing::new(None, 10, Some(5), false);
        parent.finish_child(&first, 30, Some(15));
        let second = Timing::new(None, 40, Some(20), false);
        parent.finish_child(&second, 65, Some(35));
        assert_eq!(parent.child_wall, 45);
        assert_eq!(parent.exclusive_cpu(Some(50)), Some(25));
        let mut child = Timing::new(None, 0, Some(0), false);
        // Unsampled grandchildren need no clocks; their absence must not
        // invalidate the inclusive CPU of their parent for our subtraction.
        child.finish_child(&Timing::new(None, 1, None, false), 4, None);
        assert!(child.child_cpu_valid);
        parent.finish_child(&Timing::new(None, 70, None, false), 80, None);
        assert_eq!(parent.exclusive_cpu(Some(60)), None);
    }

    #[test]
    fn cumulative_metrics_survive_failure_and_bound_then_release_series() {
        gstreamer::init().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let output = requests.clone();
        let fail = Arc::new(AtomicBool::new(true));
        let failed = fail.clone();
        let metrics = Metrics::start(
            Config {
                max_series: 1,
                ..Default::default()
            },
            &Resource::builder_empty().build(),
            move |body| {
                output
                    .lock()
                    .unwrap()
                    .push(ExportMetricsServiceRequest::decode(body.as_slice()).unwrap());
                if failed.swap(false, SeqCst) {
                    Err(FlushError::ExportFailed)
                } else {
                    Ok(false)
                }
            },
        )
        .unwrap();
        let site = record(0).site;
        let series = metrics.series(&site).unwrap();
        let mut token = record(0).token;
        token.trace_id = TraceId::from(42);
        token.span_id = SpanId::from(7);
        let mut timing = Timing::new(Some(series), 10, Some(10), true);
        timing.child_wall = 5;
        timing.child_cpu = 2;
        metrics.observe(timing, 30, Some(20), Some(&token));
        assert_eq!(
            metrics.flush(Duration::from_secs(2)),
            Err(FlushError::ExportFailed)
        );
        metrics.observe(
            Timing::new(metrics.series(&site), 30, None, false),
            50,
            None,
            None,
        );
        metrics.flush(Duration::from_secs(2)).unwrap();
        let data = requests.lock().unwrap();
        let Data::Histogram(histogram) = data.last().unwrap().resource_metrics[0].scope_metrics[0]
            .metrics[0]
            .data
            .as_ref()
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(histogram.data_points[0].count, 2);
        assert_eq!(histogram.data_points[0].sum, Some(40e-9));
        assert_eq!(
            histogram.data_points[0].bucket_counts.iter().sum::<u64>(),
            2
        );
        drop(data);
        let mut other = record(1).site;
        Arc::get_mut(&mut other).unwrap().attributes[0].value = "other".into();
        assert!(metrics.series(&other).is_none());
        assert_eq!(metrics.stats().dropped_capacity, 1);
        drop(site);
        metrics.flush(Duration::from_secs(2)).unwrap();
        assert_eq!(metrics.stats().active_series, 0);
        assert!(metrics.series(&other).is_some());
        metrics.shutdown(Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn series_reused_during_export_keeps_observations_after_that_snapshot() {
        gstreamer::init().unwrap();
        let (entered, enter) = bounded(1);
        let (release, wait) = bounded(1);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let output = requests.clone();
        let mut first = true;
        let metrics = Metrics::start(
            Config::default(),
            &Resource::builder_empty().build(),
            move |body| {
                output
                    .lock()
                    .unwrap()
                    .push(ExportMetricsServiceRequest::decode(body.as_slice()).unwrap());
                if first {
                    first = false;
                    entered.send(()).unwrap();
                    wait.recv_timeout(Duration::from_secs(2)).unwrap();
                }
                Ok(false)
            },
        )
        .unwrap();
        let site = record(0).site;
        metrics.observe(
            Timing::new(metrics.series(&site), 0, None, false),
            20,
            None,
            None,
        );
        drop(site);
        assert_eq!(
            metrics.flush(Duration::from_millis(20)),
            Err(FlushError::Timeout)
        );
        enter.recv_timeout(Duration::from_secs(2)).unwrap();
        let replacement = record(0).site;
        metrics.observe(
            Timing::new(metrics.series(&replacement), 0, None, false),
            20,
            None,
            None,
        );
        drop(replacement);
        release.send(()).unwrap();
        metrics.flush(Duration::from_secs(2)).unwrap();
        let requests = requests.lock().unwrap();
        let Data::Histogram(histogram) =
            requests.last().unwrap().resource_metrics[0].scope_metrics[0].metrics[0]
                .data
                .as_ref()
                .unwrap()
        else {
            panic!()
        };
        assert_eq!(
            histogram.data_points[0].count, 2,
            "the old snapshot must not retire subsequent observations"
        );
        assert_eq!(metrics.stats().active_series, 0);
        metrics.shutdown(Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn concurrent_producers_keep_exact_final_counts_and_sums() {
        gstreamer::init().unwrap();
        let metrics = Metrics::start(
            Config::default(),
            &Resource::builder_empty().build(),
            |_| Ok(false),
        )
        .unwrap();
        let site = record(0).site;
        let series = metrics.series(&site).unwrap();
        thread::scope(|scope| {
            for _ in 0..8 {
                let metrics = &metrics;
                let series = &series;
                scope.spawn(move || {
                    for _ in 0..10000 {
                        metrics.observe(
                            Timing::new(Some(series.clone()), 0, None, false),
                            20,
                            None,
                            None,
                        );
                    }
                });
            }
            for _ in 0..5 {
                metrics.flush(Duration::from_secs(2)).unwrap();
            }
        });
        let point = series.histograms[0]
            .snapshot(&series, &metrics.inner.anchor)
            .unwrap();
        assert_eq!(point.count, 80000);
        assert_eq!(point.sum, Some(80000.0 * 20e-9));
        assert_eq!(metrics.stats().recorded_pushes, 80000);
        metrics.shutdown(Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn shutdown_deadline_survives_stalled_export_and_concurrent_updates() {
        gstreamer::init().unwrap();
        let (entered, enter) = bounded(1);
        let (release, wait) = bounded(1);
        let metrics = Metrics::start(
            Config::default(),
            &Resource::builder_empty().build(),
            move |_| {
                let _ = entered.try_send(());
                let _ = wait.recv_timeout(Duration::from_secs(2));
                Ok(false)
            },
        )
        .unwrap();
        let site = record(0).site;
        let series = metrics.series(&site).unwrap();
        metrics.observe(
            Timing::new(Some(series.clone()), 0, None, false),
            10,
            None,
            None,
        );
        assert_eq!(
            metrics.flush(Duration::from_millis(20)),
            Err(FlushError::Timeout)
        );
        enter.recv_timeout(Duration::from_secs(2)).unwrap();
        for _ in 0..1000 {
            metrics.observe(
                Timing::new(Some(series.clone()), 0, None, false),
                10,
                None,
                None,
            );
        }
        assert_eq!(metrics.stats().recorded_pushes, 1001);
        let start = Instant::now();
        assert_eq!(
            metrics.shutdown(Duration::from_millis(20)),
            Err(FlushError::Timeout)
        );
        assert!(start.elapsed() < Duration::from_secs(1));
        release.send(()).unwrap();
        // Both the in-flight snapshot and the final snapshot can complete.
        release.send(()).unwrap();
        metrics.shutdown(Duration::from_secs(2)).unwrap();
    }
}
