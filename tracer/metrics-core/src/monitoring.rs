//! Fixed-cardinality instrumentation health; collection never enables an exporter.
use crate::proto;
use std::{
    str::FromStr,
    sync::{
        atomic::{AtomicU64, Ordering::Relaxed},
        OnceLock,
    },
    time::Instant,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Monitoring {
    None,
    #[default]
    Core,
    All,
}
impl FromStr for Monitoring {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "none" => Ok(Self::None),
            "core" => Ok(Self::Core),
            "all" => Ok(Self::All),
            _ => Err("monitoring must be none, core, or all".into()),
        }
    }
}
static LEVEL: OnceLock<Monitoring> = OnceLock::new();
static EXTRA: OnceLock<fn() -> Vec<proto::MetricFamily>> = OnceLock::new();
static SYMBOLS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
pub fn symbol_cache(hits: usize, misses: usize, evicted: usize, entries: usize, bytes: usize) {
    if level() != Monitoring::All {
        return;
    }
    for (i, value) in [hits, misses, evicted].into_iter().enumerate() {
        SYMBOLS[i].fetch_add(value as u64, Relaxed);
    }
    SYMBOLS[3].store(entries as u64, Relaxed);
    SYMBOLS[4].store(bytes as u64, Relaxed);
}
pub fn configure(level: Monitoring) {
    let _ = LEVEL.set(level);
}
pub fn level() -> Monitoring {
    LEVEL.get().copied().unwrap_or(Monitoring::None)
}
pub fn set_extra(snapshot: fn() -> Vec<proto::MetricFamily>) {
    let _ = EXTRA.set(snapshot);
}

#[derive(Clone, Copy, Debug)]
pub enum Stage {
    Traces,
    Metrics,
    Logs,
    Profiles,
    Collection,
}
impl Stage {
    fn name(self) -> &'static str {
        match self {
            Self::Traces => "trace_export",
            Self::Metrics => "metric_export",
            Self::Logs => "log_export",
            Self::Profiles => "profile_processing",
            Self::Collection => "metric_collection",
        }
    }
}
const STAGES: [Stage; 5] = [
    Stage::Traces,
    Stage::Metrics,
    Stage::Logs,
    Stage::Profiles,
    Stage::Collection,
];
const BOUNDS: [f64; 6] = [0.0001, 0.001, 0.01, 0.1, 1.0, 10.0];
struct WorkStats {
    calls: AtomicU64,
    nanos: AtomicU64,
    buckets: [AtomicU64; 7],
    bytes: AtomicU64,
    records: AtomicU64,
    high_water: AtomicU64,
}
static WORK: [WorkStats; 5] = [const {
    WorkStats {
        calls: AtomicU64::new(0),
        nanos: AtomicU64::new(0),
        buckets: [const { AtomicU64::new(0) }; 7],
        bytes: AtomicU64::new(0),
        records: AtomicU64::new(0),
        high_water: AtomicU64::new(0),
    }
}; 5];
pub struct Work {
    start: Instant,
    stage: Stage,
}
impl Drop for Work {
    fn drop(&mut self) {
        let elapsed = self.start.elapsed();
        let stats = &WORK[self.stage as usize];
        stats.calls.fetch_add(1, Relaxed);
        stats
            .nanos
            .fetch_add(elapsed.as_nanos().min(u64::MAX as u128) as u64, Relaxed);
        let bucket = BOUNDS
            .iter()
            .position(|b| elapsed.as_secs_f64() <= *b)
            .unwrap_or(BOUNDS.len());
        stats.buckets[bucket].fetch_add(1, Relaxed);
    }
}
pub fn work(stage: Stage, bytes: usize, records: usize) -> Option<Work> {
    if level() != Monitoring::All {
        return None;
    }
    record_size(stage, bytes, records);
    Some(Work {
        start: Instant::now(),
        stage,
    })
}
pub fn record_size(stage: Stage, bytes: usize, records: usize) {
    if level() == Monitoring::All {
        WORK[stage as usize].bytes.fetch_add(bytes as u64, Relaxed);
        WORK[stage as usize]
            .records
            .fetch_add(records as u64, Relaxed);
    }
}
pub fn queue(stage: Stage, count: impl FnOnce() -> usize) {
    if level() == Monitoring::All {
        WORK[stage as usize]
            .high_water
            .fetch_max(count() as u64, Relaxed);
    }
}

fn labels(values: &[(&str, &str)]) -> Vec<proto::LabelPair> {
    values
        .iter()
        .map(|(name, value)| {
            let mut l = proto::LabelPair::new();
            l.set_name((*name).into());
            l.set_value((*value).into());
            l
        })
        .collect()
}
fn add(
    families: &mut Vec<proto::MetricFamily>,
    name: &str,
    help: &str,
    kind: proto::MetricType,
    metric: proto::Metric,
) {
    if let Some(family) = families.iter_mut().find(|f| f.name() == name) {
        family.mut_metric().push(metric);
    } else {
        let mut family = proto::MetricFamily::new();
        family.set_name(name.into());
        family.set_help(help.into());
        family.set_field_type(kind);
        family.set_metric(vec![metric]);
        families.push(family);
    }
}
pub fn sample(
    families: &mut Vec<proto::MetricFamily>,
    name: &str,
    help: &str,
    gauge: bool,
    value: u64,
    tags: &[(&str, &str)],
) {
    let mut metric = proto::Metric::new();
    metric.set_label(labels(tags));
    let kind = if gauge {
        let mut v = proto::Gauge::new();
        v.set_value(value as f64);
        metric.set_gauge(v);
        proto::MetricType::GAUGE
    } else {
        let mut v = proto::Counter::new();
        v.set_value(value as f64);
        metric.set_counter(v);
        proto::MetricType::COUNTER
    };
    add(families, name, help, kind, metric);
}

pub(crate) fn append(families: &mut Vec<proto::MetricFamily>) {
    let level = level();
    if level == Monitoring::None {
        return;
    }
    let series = families.iter().map(|f| f.get_metric().len()).sum::<usize>();
    sample(
        families,
        "mars_instrument_collector_pushes_total",
        "Push calls observed by the full metrics collector",
        false,
        crate::recorded_pushes(),
        &[],
    );
    sample(
        families,
        "mars_instrument_collector_series",
        "Active application metric series",
        true,
        series as u64,
        &[],
    );
    if let Some(extra) = EXTRA.get() {
        families.extend(extra());
    }
    if level != Monitoring::All {
        return;
    }
    for (i, outcome) in ["hit", "miss", "evicted"].iter().enumerate() {
        sample(
            families,
            "mars_instrument_symbol_cache_total",
            "Profile symbol cache activity",
            false,
            SYMBOLS[i].load(Relaxed),
            &[("outcome", outcome)],
        );
    }
    sample(
        families,
        "mars_instrument_symbol_cache_entries",
        "Retained resolved instruction addresses",
        true,
        SYMBOLS[3].load(Relaxed),
        &[],
    );
    sample(
        families,
        "mars_instrument_symbol_cache_bytes",
        "Retained symbol strings and frames excluding temporary debug parsing",
        true,
        SYMBOLS[4].load(Relaxed),
        &[],
    );
    for stage in STAGES {
        let stats = &WORK[stage as usize];
        let tags = [("stage", stage.name())];
        for (name, help, gauge, value) in [
            (
                "mars_instrument_work_calls_total",
                "Completed instrumentation worker operations",
                false,
                stats.calls.load(Relaxed),
            ),
            (
                "mars_instrument_work_bytes_total",
                "Encoded bytes handled by instrumentation workers before compression",
                false,
                stats.bytes.load(Relaxed),
            ),
            (
                "mars_instrument_work_records_total",
                "Records handled by instrumentation workers",
                false,
                stats.records.load(Relaxed),
            ),
            (
                "mars_instrument_queue_high_water",
                "Maximum observed queued records excluding in-flight batches",
                true,
                stats.high_water.load(Relaxed),
            ),
        ] {
            sample(families, name, help, gauge, value, &tags);
        }
        let mut histogram = proto::Histogram::new();
        let mut count = 0;
        let buckets = BOUNDS
            .iter()
            .enumerate()
            .map(|(i, bound)| {
                count += stats.buckets[i].load(Relaxed);
                let mut b = proto::Bucket::new();
                b.set_upper_bound(*bound);
                b.set_cumulative_count(count);
                b
            })
            .collect();
        count += stats.buckets[BOUNDS.len()].load(Relaxed);
        histogram.set_sample_count(count);
        histogram.set_sample_sum(stats.nanos.load(Relaxed) as f64 / 1e9);
        histogram.set_bucket(buckets);
        let mut metric = proto::Metric::new();
        metric.set_label(labels(&tags));
        metric.set_histogram(histogram);
        add(
            families,
            "mars_instrument_work_duration_seconds",
            "Wall time spent on instrumentation worker operations",
            proto::MetricType::HISTOGRAM,
            metric,
        );
    }
}

/// Snapshot health without collecting application measurements (legacy exporter).
pub fn snapshot() -> Vec<proto::MetricFamily> {
    let mut families = Vec::new();
    append(&mut families);
    families
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn enum_has_three_values_and_core_default() {
        assert_eq!(Monitoring::default(), Monitoring::Core);
        for (name, expected) in [
            ("none", Monitoring::None),
            ("core", Monitoring::Core),
            ("all", Monitoring::All),
        ] {
            assert_eq!(name.parse::<Monitoring>().unwrap(), expected);
        }
        for value in ["true", "1", "", "verbose"] {
            assert!(value.parse::<Monitoring>().is_err());
        }
    }
}
