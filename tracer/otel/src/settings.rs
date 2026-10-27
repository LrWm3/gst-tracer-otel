//! Explicit signal selection. Exporter addresses never enable a signal.
use std::time::Duration;

pub(crate) fn parameters(params: Option<&str>) -> Result<Option<gstreamer::Structure>, String> {
    params
        .filter(|s| !s.trim().is_empty())
        .map(|s| {
            format!("mars-instrument,{s}")
                .parse::<gstreamer::Structure>()
                .map_err(|e| format!("invalid tracer parameters: {e}"))
        })
        .transpose()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MetricExporter {
    Otlp,
    Prometheus,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Settings {
    pub monitoring: gst_mars_metrics_core::monitoring::Monitoring,
    pub metrics: bool,
    pub traces: bool,
    pub profiles: bool,
    pub logs: bool,
    pub exporter: MetricExporter,
    pub port: u16,
    pub interval: Duration,
    pub collector: gst_mars_metrics_core::settings::Config,
}

impl Settings {
    #[cfg(test)]
    pub fn parse(
        params: Option<&str>,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Option<Self>, String> {
        Self::from_structure(parameters(params)?.as_deref(), env)
    }

    #[cfg(test)]
    pub fn from_structure(
        structure: Option<&gstreamer::StructureRef>,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Option<Self>, String> {
        let mut properties = crate::properties::Settings::default();
        if let Some(structure) = structure {
            properties
                .take_parameters(crate::tracer::Mode::Instrument, &mut structure.to_owned())?;
        }
        Self::resolve(&properties.overrides, env)
    }

    pub fn resolve(
        overrides: &gst_mars_metrics_core::configuration::Overrides,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Option<Self>, String> {
        let read = |name| overrides.read_named(&crate::properties::INSTRUMENT, name, &env);
        let Some(signals) = read("signals")? else {
            if crate::properties::INSTRUMENT.iter().any(|s| {
                s.property.name().starts_with("metrics-") && overrides.contains(s.property.name())
            }) || env("MARS_METRICS_EXPORTER").is_some()
            {
                return Err("set signals (or MARS_SIGNALS) when selecting exporters".into());
            }
            return Ok(None);
        };
        let mut settings = Self {
            monitoring: gst_mars_metrics_core::monitoring::Monitoring::None,
            metrics: false,
            traces: false,
            profiles: false,
            logs: false,
            exporter: MetricExporter::Otlp,
            port: 9188,
            interval: Duration::from_secs(10),
            collector: gst_mars_metrics_core::settings::Config::default(),
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
        if settings.metrics {
            settings.monitoring = read("monitoring")?.unwrap().parse()?;
            settings.exporter = match read("metrics-exporter")?.as_deref() {
                Some("otlp") => MetricExporter::Otlp,
                Some("prometheus") => MetricExporter::Prometheus,
                _ => unreachable!(),
            };
            if settings.exporter == MetricExporter::Prometheus {
                settings.port = read("metrics-port")?.unwrap().parse().unwrap();
            } else {
                settings.interval =
                    Duration::from_millis(read("metrics-interval-ms")?.unwrap().parse().unwrap());
            }
            settings.collector = gst_mars_metrics_core::settings::Config::resolve(
                overrides,
                &crate::properties::INSTRUMENT,
                env,
            )?;
        }
        Ok(Some(settings))
    }
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

    #[test]
    fn monitoring_defaults_to_core_and_never_enables_metrics() {
        use gst_mars_metrics_core::monitoring::Monitoring;
        gstreamer::init().unwrap();
        assert_eq!(
            Settings::parse(Some("signals=metrics"), |_| None)
                .unwrap()
                .unwrap()
                .monitoring,
            Monitoring::Core
        );
        for (value, expected) in [
            ("none", Monitoring::None),
            ("core", Monitoring::Core),
            ("all", Monitoring::All),
        ] {
            assert_eq!(
                Settings::parse(
                    Some(&format!("signals=metrics,monitoring={value}")),
                    |key| (key == "MARS_MONITORING").then(|| "invalid".into())
                )
                .unwrap()
                .unwrap()
                .monitoring,
                expected
            );
        }
        assert!(Settings::parse(Some("signals=metrics,monitoring=invalid"), |_| None).is_err());
        let disabled = Settings::parse(Some("signals=traces,monitoring=all"), |_| None)
            .unwrap()
            .unwrap();
        assert!(!disabled.metrics);
        assert_eq!(disabled.monitoring, Monitoring::None);
    }
}
