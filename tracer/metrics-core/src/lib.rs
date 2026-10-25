//! Shared metric collection for the standalone and combined Mars tracers.
pub mod background;
pub mod exemplars;
mod metric_lifecycle;
mod metricsimp;
pub mod monitoring;

pub use metricsimp::{MetricsTracerImp, CAT};
pub use prometheus::proto;

/// Optional push/pull tracing, invoked in a defined order around shared metric timing.
/// The finish callback returns IDs only when that span was admitted for export.
pub struct CaptureHooks {
    pub exemplars: bool,
    pub begin: fn(u64, &gstreamer::Pad, &gstreamer::Buffer),
    pub end: fn(u64, &gstreamer::Pad, i32) -> Option<exemplars::Exemplar>,
    pub list_begin: fn(u64, &gstreamer::Pad, &gstreamer::BufferList),
    pub list_end: fn(u64, &gstreamer::Pad, i32) -> Option<exemplars::Exemplar>,
    pub pull_begin: fn(u64, &gstreamer::Pad),
    pub pull_end:
        fn(u64, &gstreamer::Pad, i32, Option<&gstreamer::Buffer>) -> Option<exemplars::Exemplar>,
}

/// Validate the new namespace without changing permissive legacy defaults.
pub fn validate_settings() -> Result<(), String> {
    for (name, max) in [
        ("MARS_METRIC_SAMPLE_RATE", 1_048_576),
        ("MARS_METRIC_QUEUE_INTERVAL_MS", 60_000),
    ] {
        if let Ok(value) = std::env::var(name) {
            if !value.parse::<u64>().is_ok_and(|n| n > 0 && n <= max) {
                return Err(format!("{name} must be 1..={max}"));
            }
        }
    }
    for name in ["MARS_METRIC_IO_COUNTS", "MARS_METRIC_PTS_AGE"] {
        if let Ok(value) = std::env::var(name) {
            if !matches!(
                value.to_ascii_lowercase().as_str(),
                "true" | "false" | "1" | "0" | "yes" | "no"
            ) {
                return Err(format!("{name} must be true or false"));
            }
        }
    }
    Ok(())
}

static CAPTURE: std::sync::OnceLock<CaptureHooks> = std::sync::OnceLock::new();

static ACCOUNTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
#[repr(align(128))]
struct Count(std::sync::atomic::AtomicU64);
static COUNTS: [Count; 64] = [const { Count(std::sync::atomic::AtomicU64::new(0)) }; 64];
thread_local! {
    static SHARD: usize = {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 64
    };
}
pub fn enable_accounting() {
    ACCOUNTING.store(true, std::sync::atomic::Ordering::Relaxed);
}
pub fn recorded_pushes() -> u64 {
    COUNTS
        .iter()
        .map(|n| n.0.load(std::sync::atomic::Ordering::Relaxed))
        .sum()
}
pub(crate) fn record_push() {
    if ACCOUNTING.load(std::sync::atomic::Ordering::Relaxed) {
        SHARD.with(|i| {
            COUNTS[*i]
                .0
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
    }
}

pub fn set_capture_hooks(hooks: CaptureHooks) -> Result<(), &'static str> {
    CAPTURE
        .set(hooks)
        .map_err(|_| "metric capture hooks already configured")
}

/// Mars settings take precedence over their original environment aliases.
pub(crate) fn setting(name: &str, legacy: &str) -> Result<String, std::env::VarError> {
    std::env::var(name).or_else(|_| std::env::var(legacy))
}

/// Snapshot the same measurements for either export protocol.
pub fn snapshot() -> Vec<proto::MetricFamily> {
    let _work = monitoring::work(monitoring::Stage::Collection, 0, 0);
    metric_lifecycle::prune();
    let mut families = prometheus::gather();
    if monitoring::level() == monitoring::Monitoring::All {
        monitoring::record_size(
            monitoring::Stage::Collection,
            0,
            families.iter().map(|f| f.get_metric().len()).sum(),
        );
    }
    monitoring::append(&mut families);
    families
}
