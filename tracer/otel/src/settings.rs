//! Explicit signal selection. Exporter addresses never enable a signal.
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MetricExporter {
    Otlp,
    Prometheus,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Settings {
    pub metrics: bool,
    pub traces: bool,
    pub profiles: bool,
    pub logs: bool,
    pub exporter: MetricExporter,
    pub port: u16,
    pub interval: Duration,
}

impl Settings {
    pub fn parse(
        params: Option<&str>,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Option<Self>, String> {
        let structure = params
            .filter(|s| !s.trim().is_empty())
            .map(|s| {
                format!("mars-instrument,{s}")
                    .parse::<gstreamer::Structure>()
                    .map_err(|e| format!("invalid tracer parameters: {e}"))
            })
            .transpose()?;
        if let Some(s) = &structure {
            for (key, _) in s.iter() {
                if !matches!(
                    key.as_str(),
                    "signals" | "metrics-exporter" | "metrics-port"
                ) {
                    return Err(format!("unknown tracer parameter: {key}"));
                }
            }
        }
        let value = |key: &str, variable: &str| -> Result<Option<String>, String> {
            if let Some(s) = &structure {
                if s.has_field(key) {
                    return s
                        .get::<String>(key)
                        .map(Some)
                        .map_err(|_| format!("{key} must be a quoted string"));
                }
            }
            Ok(env(variable))
        };
        let Some(signals) = value("signals", "MARS_SIGNALS")? else {
            if structure.is_some() || env("MARS_METRICS_EXPORTER").is_some() {
                return Err("set signals (or MARS_SIGNALS) when selecting exporters".into());
            }
            return Ok(None);
        };
        let mut settings = Self {
            metrics: false,
            traces: false,
            profiles: false,
            logs: false,
            exporter: MetricExporter::Otlp,
            port: 9188,
            interval: Duration::from_secs(10),
        };
        for signal in signals.split(',').map(str::trim) {
            let flag = match signal {
                "metrics" => &mut settings.metrics,
                "traces" => &mut settings.traces,
                "profiles" => &mut settings.profiles,
                "logs" => &mut settings.logs,
                _ => {
                    return Err(format!(
                        "unknown signal {signal:?}; use metrics,traces,profiles,logs"
                    ))
                }
            };
            if *flag {
                return Err(format!("duplicate signal: {signal}"));
            }
            *flag = true;
        }
        // Disabled exporters do not read or validate their settings.
        if settings.metrics {
            settings.exporter = match value("metrics-exporter", "MARS_METRICS_EXPORTER")?.as_deref()
            {
                None | Some("otlp") => MetricExporter::Otlp,
                Some("prometheus") => MetricExporter::Prometheus,
                Some(v) => return Err(format!("unknown metrics exporter: {v}")),
            };
            if settings.exporter == MetricExporter::Prometheus {
                settings.port = if let Some(s) = &structure {
                    if s.has_field("metrics-port") {
                        let port = s
                            .get::<i32>("metrics-port")
                            .map_err(|_| "metrics-port must be an integer")?;
                        u16::try_from(port).map_err(|_| "metrics-port must be 1..=65535")?
                    } else {
                        port(&env)?
                    }
                } else {
                    port(&env)?
                };
                if settings.port == 0 {
                    return Err("metrics-port must be 1..=65535".into());
                }
            } else {
                let ms = env("MARS_METRIC_INTERVAL_MS")
                    .or_else(|| env("GST_INSTRUMENT_METRIC_INTERVAL_MS"))
                    .unwrap_or_else(|| "10000".into())
                    .parse::<u64>()
                    .map_err(|_| "invalid MARS_METRIC_INTERVAL_MS")?;
                if !(1..=60000).contains(&ms) {
                    return Err("MARS_METRIC_INTERVAL_MS must be 1..=60000".into());
                }
                settings.interval = Duration::from_millis(ms);
            }
        }
        Ok(Some(settings))
    }
}
fn port(env: &impl Fn(&str) -> Option<String>) -> Result<u16, String> {
    env("MARS_METRICS_PORT")
        .unwrap_or_else(|| "9188".into())
        .parse()
        .map_err(|_| "MARS_METRICS_PORT must be 1..=65535".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selection_precedence_and_disabled_settings() {
        gstreamer::init().unwrap();
        let s = Settings::parse(Some("signals=\"traces,profiles\""), |key| {
            Some(
                match key {
                    "MARS_SIGNALS" => "metrics",
                    _ => "invalid",
                }
                .into(),
            )
        })
        .unwrap()
        .unwrap();
        assert!(s.traces && s.profiles && !s.metrics && !s.logs);
        assert!(Settings::parse(None, |_| None).unwrap().is_none());
        let s = Settings::parse(
            Some("signals=metrics,metrics-exporter=prometheus,metrics-port=9234"),
            |_| None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(s.port, 9234);
        assert_eq!(s.exporter, MetricExporter::Prometheus);
        for params in [
            "signals=\"metrics,metrics\"",
            "signals=typo",
            "signal=metrics",
            "signals=metrics,metrics-port=-1,metrics-exporter=prometheus",
        ] {
            assert!(Settings::parse(Some(params), |_| None).is_err(), "{params}");
        }
    }
}
