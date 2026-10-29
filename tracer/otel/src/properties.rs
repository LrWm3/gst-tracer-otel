//! Shared public construction properties; resolution happens only for enabled signals.
use crate::tracer::Mode;
use gst_mars_metrics_core::configuration::{Overrides, Spec};
use std::sync::LazyLock;

// This plugin and mars-metrics statically link metrics-core. Use distinct GType
// names so inspecting both plugin classes in one process never registers an
// enum twice. The public Rust enums have the same values and behavior.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, glib::Enum)]
#[enum_type(name = "MarsOtelToggle")]
#[repr(i32)]
pub enum Toggle {
    #[default]
    Auto,
    #[enum_value(nick = "false")]
    Off,
    #[enum_value(nick = "true")]
    On,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, glib::Enum)]
#[enum_type(name = "MarsOtelMonitoringSetting")]
#[repr(i32)]
pub enum MonitoringSetting {
    #[default]
    Auto,
    None,
    Core,
    All,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, glib::Enum)]
#[enum_type(name = "MarsMetricsExporter")]
#[repr(i32)]
pub enum MetricsExporter {
    #[default]
    Auto,
    Otlp,
    Prometheus,
}

pub(crate) static INSTRUMENT: LazyLock<Vec<Spec>> = LazyLock::new(instrument_specs);

fn instrument_specs() -> Vec<Spec> {
    let mut specs = vec![
        Spec::string(
            "signals",
            &["MARS_SIGNALS"],
            None,
            "Comma-separated metrics,traces,profiles,logs; unset inherits or uses compatibility behavior",
        ),
        Spec::enumeration::<MetricsExporter>(
            "metrics-exporter",
            &["MARS_METRICS_EXPORTER"],
            "otlp",
            "Metric delivery: auto inherits (default otlp), otlp, prometheus",
        ),
        Spec::number(
            "metrics-port",
            65535,
            &["MARS_METRICS_PORT"],
            "9188",
            "Prometheus listen port; 0 inherits (default 9188)",
        ),
        Spec::number(
            "metrics-interval-ms",
            60000,
            &["MARS_METRIC_INTERVAL_MS", "GST_INSTRUMENT_METRIC_INTERVAL_MS"],
            "10000",
            "OTLP metric export interval; 0 inherits (default 10000 ms)",
        ),
    ];
    specs.extend(gst_mars_metrics_core::settings::specs::<
        Toggle,
        MonitoringSetting,
    >());
    specs
}
pub(crate) static EXPORT: LazyLock<Vec<Spec>> = LazyLock::new(export_specs);

fn export_specs() -> Vec<Spec> {
    vec![
        Spec::number_with_default(
            "trace-max-age-ms",
            u32::MAX,
            "5000",
            "Stop starting spans when a local journey reaches this age; 0 disables (default 5000 ms)",
        ),
        Spec::number_with_default(
            "trace-max-spans",
            u32::MAX,
            "64",
            "Maximum spans admitted per local journey across branches, including root; 0 disables (default 64)",
        ),
        Spec::number_with_default(
            "trace-root-min-period-us",
            u32::MAX,
            "16000",
            "Minimum interval between sampled local roots per pad, in microseconds; 0 disables (default 16000)",
        ),
        Spec::number(
            "trace-queue-capacity",
            1_048_576,
            &["GST_OTEL_QUEUE_CAPACITY"],
            "8192",
            "Bounded trace queue size; 0 inherits (default 8192)",
        ),
        Spec::number(
            "trace-batch-size",
            1_048_576,
            &["GST_OTEL_BATCH_SIZE"],
            "512",
            "Maximum trace batch size, no greater than queue capacity; 0 inherits",
        ),
        Spec::number(
            "trace-export-interval-ms",
            60000,
            &["GST_OTEL_EXPORT_INTERVAL_MS"],
            "200",
            "Trace export interval; 0 inherits (default 200 ms)",
        ),
        Spec::number(
            "trace-export-concurrency",
            4,
            &["GST_OTEL_EXPORT_CONCURRENCY"],
            "1",
            "Concurrent trace export requests; 0 inherits (default 1)",
        ),
    ]
}
pub(crate) static LIFECYCLE: LazyLock<Vec<Spec>> = LazyLock::new(lifecycle_specs);

fn lifecycle_specs() -> Vec<Spec> {
    vec![Spec::number(
        "flush-timeout-ms",
        60000,
        &["GST_OTEL_FLUSH_TIMEOUT_MS"],
        "10000",
        "Default lifecycle drain deadline; 0 inherits (default 10000 ms)",
    )]
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Settings {
    pub profiles: crate::profilesettings::Settings,
    pub overrides: Overrides,
}
impl Settings {
    pub fn properties(mode: Mode) -> Vec<glib::ParamSpec> {
        // GObject installs an owner and ID on each ParamSpec. A ref-counted
        // clone cannot be installed on another class; build fresh specs here.
        let mut properties = crate::profilesettings::Settings::properties();
        if mode != Mode::Profiles {
            properties.extend(lifecycle_specs().into_iter().map(|s| s.property));
            properties.extend(export_specs().into_iter().map(|s| s.property));
        }
        if mode == Mode::Instrument {
            properties.extend(instrument_specs().into_iter().map(|s| s.property));
        }
        properties
    }
    pub fn set_property(&mut self, value: &glib::Value, pspec: &glib::ParamSpec) {
        if pspec.name().starts_with("profile-") {
            self.profiles.set_property(value, pspec);
        } else {
            self.overrides.set_property(value, pspec);
        }
    }
    pub fn property(&self, pspec: &glib::ParamSpec) -> glib::Value {
        if pspec.name().starts_with("profile-") {
            self.profiles.property(pspec)
        } else {
            self.overrides.property(pspec)
        }
    }
    pub fn take_parameters(
        &mut self,
        mode: Mode,
        params: &mut gstreamer::StructureRef,
    ) -> Result<(), String> {
        self.profiles.take_parameters(params)?;
        if mode != Mode::Profiles {
            self.overrides.take_parameters(params, &LIFECYCLE)?;
            self.overrides.take_parameters(params, &EXPORT)?;
        }
        if mode == Mode::Instrument {
            self.overrides.take_parameters(params, &INSTRUMENT)?;
        }
        if let Some((key, _)) = params.iter().next() {
            return Err(format!("unknown tracer parameter: {key}"));
        }
        Ok(())
    }
    pub fn flush_timeout(&self) -> Result<std::time::Duration, String> {
        let ms = self
            .overrides
            .read(&LIFECYCLE[0], &|key| std::env::var(key).ok())?
            .unwrap()
            .parse::<u64>()
            .unwrap();
        Ok(std::time::Duration::from_millis(ms))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glib::prelude::*;

    #[test]
    fn journey_limits_have_defaults_and_zero_is_an_explicit_opt_out() {
        gstreamer::init().unwrap();
        for (name, default) in [("trace-max-age-ms", 5000u32), ("trace-max-spans", 64u32)] {
            let spec = EXPORT.iter().find(|s| s.property.name() == name).unwrap();
            assert_eq!(spec.property.default_value().get::<u32>().unwrap(), default);
            let mut settings = Settings::default();
            assert_eq!(
                settings.overrides.read(spec, &|_| None).unwrap().unwrap(),
                default.to_string()
            );
            settings.set_property(&0u32.to_value(), &spec.property);
            assert_eq!(settings.property(&spec.property).get::<u32>().unwrap(), 0);
            assert_eq!(
                settings.overrides.read(spec, &|_| None).unwrap().as_deref(),
                Some("0")
            );
            let mut params = format!("mars-instrument,{name}=3")
                .parse::<gstreamer::Structure>()
                .unwrap();
            settings
                .take_parameters(Mode::Instrument, &mut params)
                .unwrap();
            assert_eq!(
                settings.overrides.read(spec, &|_| None).unwrap().as_deref(),
                Some("3")
            );
            let mut params = format!("mars-instrument,{name}=0")
                .parse::<gstreamer::Structure>()
                .unwrap();
            settings
                .take_parameters(Mode::Instrument, &mut params)
                .unwrap();
            assert_eq!(
                settings.overrides.read(spec, &|_| None).unwrap().as_deref(),
                Some("0")
            );
            let mut params = format!("mars-instrument,{name}=-1")
                .parse::<gstreamer::Structure>()
                .unwrap();
            assert!(settings
                .take_parameters(Mode::Instrument, &mut params)
                .is_err());
        }
    }

    #[test]
    fn root_period_defaults_to_16ms_and_accepts_explicit_zero() {
        gstreamer::init().unwrap();
        let spec = EXPORT
            .iter()
            .find(|s| s.property.name() == "trace-root-min-period-us")
            .unwrap();
        assert_eq!(spec.property.default_value().get::<u32>().unwrap(), 16_000);
        let mut settings = Settings::default();
        assert_eq!(
            settings.overrides.read(spec, &|_| None).unwrap().as_deref(),
            Some("16000")
        );
        settings.set_property(&0u32.to_value(), &spec.property);
        assert_eq!(settings.property(&spec.property).get::<u32>().unwrap(), 0);
        assert_eq!(
            settings.overrides.read(spec, &|_| None).unwrap().as_deref(),
            Some("0")
        );
        let mut params = "mars-instrument,trace-root-min-period-us=23000"
            .parse::<gstreamer::Structure>()
            .unwrap();
        settings
            .take_parameters(Mode::Instrument, &mut params)
            .unwrap();
        assert_eq!(
            settings.overrides.read(spec, &|_| None).unwrap().as_deref(),
            Some("23000")
        );
        let mut params = "mars-instrument,trace-root-min-period-us=0"
            .parse::<gstreamer::Structure>()
            .unwrap();
        settings
            .take_parameters(Mode::Instrument, &mut params)
            .unwrap();
        assert_eq!(
            settings.overrides.read(spec, &|_| None).unwrap().as_deref(),
            Some("0")
        );
        let mut params = "mars-instrument,trace-root-min-period-us=-1"
            .parse::<gstreamer::Structure>()
            .unwrap();
        assert!(settings
            .take_parameters(Mode::Instrument, &mut params)
            .is_err());
    }
}
