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
        let read = |name| {
            let spec = specs.iter().find(|s| s.property.name() == name).unwrap();
            // The original GST_PROM_LATENCY controls were deliberately permissive.
            // Keep their parsing separate from the strict properties/MARS namespace.
            if !overrides.contains(name) && env(spec.environment[0]).is_none() {
                if let Some(value) = spec.environment.get(1).and_then(|key| env(key)) {
                    match name {
                        "metrics-sample-rate" | "metrics-queue-interval-ms" => {
                            return Ok(value
                                .parse::<u64>()
                                .ok()
                                .filter(|n| *n > 0)
                                .map(|n| n.to_string())
                                .unwrap_or_else(|| spec.fallback.unwrap().into()));
                        }
                        "metrics-io-counts" | "metrics-pts-age" => {
                            return Ok(matches!(
                                value.to_ascii_lowercase().as_str(),
                                "true" | "1" | "yes"
                            )
                            .to_string());
                        }
                        _ => {}
                    }
                }
            }
            overrides.read(spec, &env).map(|v| v.unwrap())
        };
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::configuration::{MonitoringSetting, Toggle};

    #[test]
    fn legacy_numbers_keep_their_range_and_fallbacks() {
        let specs = specs::<Toggle, MonitoringSetting>();
        for (text, rate, interval) in [
            ("120000", 120000, 120000),
            ("18446744073709551615", u64::MAX, u64::MAX),
            ("0", 16, 100),
            ("invalid", 16, 100),
        ] {
            let config = Config::resolve(&Overrides::default(), &specs, |key| {
                key.starts_with("GST_PROM_LATENCY_").then(|| text.into())
            })
            .unwrap();
            assert_eq!(config.sample_rate, rate);
            assert_eq!(config.queue_interval_ms, interval);
        }
    }

    #[test]
    fn legacy_booleans_remain_permissive_and_properties_win() {
        gstreamer::init().unwrap();
        let specs = specs::<Toggle, MonitoringSetting>();
        for (text, expected) in [("YES", true), ("1", true), ("anything-else", false)] {
            let config = Config::resolve(&Overrides::default(), &specs, |key| {
                key.starts_with("GST_PROM_LATENCY_").then(|| text.into())
            })
            .unwrap();
            assert_eq!(config.io_counts, expected);
            assert_eq!(config.pts_age, expected);
        }
        let env = |key: &str| {
            (key.starts_with("MARS_METRIC_") || key.starts_with("GST_PROM_LATENCY_"))
                .then(|| "invalid".into())
        };
        assert!(Config::resolve(&Overrides::default(), &specs, env).is_err());
        let mut overrides = Overrides::default();
        let mut params = "test,metrics-sample-rate=16,metrics-queue-interval-ms=100,metrics-include=\"*\",metrics-io-counts=false,metrics-pts-age=false"
            .parse::<gstreamer::Structure>().unwrap();
        overrides.take_parameters(&mut params, &specs).unwrap();
        assert_eq!(
            Config::resolve(&overrides, &specs, env).unwrap(),
            Config::default()
        );
        // A valid new-namespace value takes precedence over a large legacy value.
        let config = Config::resolve(&Overrides::default(), &specs, |key| match key {
            "MARS_METRIC_SAMPLE_RATE" => Some("32".into()),
            "GST_PROM_LATENCY_HISTOGRAM_SAMPLE_RATE" => Some(u64::MAX.to_string()),
            _ => None,
        })
        .unwrap();
        assert_eq!(config.sample_rate, 32);
        for key in [
            "MARS_METRIC_SAMPLE_RATE",
            "MARS_METRIC_QUEUE_INTERVAL_MS",
            "MARS_METRIC_IO_COUNTS",
            "MARS_METRIC_PTS_AGE",
        ] {
            assert!(
                Config::resolve(&Overrides::default(), &specs, |name| (name == key)
                    .then(|| "invalid".into()))
                .is_err()
            );
        }
    }
}
