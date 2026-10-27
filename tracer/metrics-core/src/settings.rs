//! Immutable collector configuration, installed before registering hooks.
use crate::configuration::{Overrides, Spec};
use std::sync::OnceLock;

pub fn specs<T, M>() -> Vec<Spec>
where
    T: glib::prelude::StaticType
        + Default
        + glib::translate::FromGlib<i32>
        + glib::translate::IntoGlib<GlibType = i32>,
    M: glib::prelude::StaticType
        + Default
        + glib::translate::FromGlib<i32>
        + glib::translate::IntoGlib<GlibType = i32>,
{
    vec![
        Spec::enumeration::<M>(
            "monitoring",
            &["MARS_MONITORING"],
            "core",
            "Instrumentation health: auto inherits (default core), none, core, all",
        ),
        Spec::number(
            "metrics-sample-rate",
            1_048_576,
            &[
                "MARS_METRIC_SAMPLE_RATE",
                "GST_PROM_LATENCY_HISTOGRAM_SAMPLE_RATE",
            ],
            "16",
            "One in N calls enters timing histograms; 0 inherits (default 16)",
        ),
        Spec::string(
            "metrics-include",
            &["MARS_METRIC_INCLUDE", "GST_PROM_LATENCY_INCLUDE"],
            Some("*"),
            "Comma-separated element-name globs; unset inherits (default *)",
        ),
        Spec::number(
            "metrics-queue-interval-ms",
            60000,
            &[
                "MARS_METRIC_QUEUE_INTERVAL_MS",
                "GST_PROM_LATENCY_QUEUE_SAMPLE_INTERVAL_MS",
            ],
            "100",
            "Queue observation interval; 0 inherits (default 100 ms)",
        ),
        Spec::enumeration::<T>(
            "metrics-io-counts",
            &["MARS_METRIC_IO_COUNTS", "GST_PROM_LATENCY_IO_COUNTS"],
            "false",
            "Per-pad buffer counters; auto inherits (default false)",
        ),
        Spec::enumeration::<T>(
            "metrics-pts-age",
            &["MARS_METRIC_PTS_AGE", "GST_PROM_LATENCY_PTS_AGE"],
            "false",
            "Buffer PTS age measurements; auto inherits (default false)",
        ),
    ]
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub sample_rate: u64,
    pub include: Vec<String>,
    pub queue_interval_ms: u64,
    pub io_counts: bool,
    pub pts_age: bool,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            sample_rate: 16,
            include: vec!["*".into()],
            queue_interval_ms: 100,
            io_counts: false,
            pts_age: false,
        }
    }
}
impl Config {
    pub fn resolve(
        overrides: &Overrides,
        specs: &[Spec],
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let read = |name| overrides.read_named(specs, name, &env).map(|v| v.unwrap());
        Ok(Self {
            sample_rate: read("metrics-sample-rate")?.parse().unwrap(),
            include: read("metrics-include")?
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect(),
            queue_interval_ms: read("metrics-queue-interval-ms")?.parse().unwrap(),
            io_counts: read("metrics-io-counts")? == "true",
            pts_age: read("metrics-pts-age")? == "true",
        })
    }
}
static CONFIG: OnceLock<Config> = OnceLock::new();
pub fn configure(config: Config) -> Result<(), String> {
    if let Some(current) = CONFIG.get() {
        return if current == &config {
            Ok(())
        } else {
            Err("metric collector already configured differently".into())
        };
    }
    CONFIG
        .set(config)
        .map_err(|_| "metric collector already configured".into())
}
pub(crate) fn current() -> &'static Config {
    CONFIG.get_or_init(Config::default)
}
