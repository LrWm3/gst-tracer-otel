//! Profile sources and optional worker attribution are selected at tracer construction.
use crate::properties::Toggle;
use glib::prelude::*;
use gst_mars_metrics_core::configuration::{Overrides, Spec};
use std::sync::LazyLock;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, glib::Enum)]
#[enum_type(name = "MarsProfileSymbolization")]
#[repr(i32)]
pub enum ProfileSymbolization {
    #[default]
    Auto,
    Full,
    Functions,
}

static SPECS: LazyLock<Vec<Spec>> = LazyLock::new(specs);

fn specs() -> Vec<Spec> {
    vec![
        Spec::string(
            "profile-url",
            &["MARS_PROFILE_URL", "GST_OTEL_PROFILE_URL"],
            None,
            "Pyroscope HTTP(S) URL; unset inherits, default localhost:4040 when profiles are selected",
        ),
        Spec::number(
            "profile-hz",
            1000,
            &["MARS_PROFILE_HZ", "GST_OTEL_PROFILE_HZ"],
            "100",
            "CPU samples per second, 1..1000; 0 inherits (default 100)",
        ),
        Spec::number(
            "profile-timeout-ms",
            60000,
            &["MARS_PROFILE_TIMEOUT_MS", "GST_OTEL_PROFILE_TIMEOUT_MS"],
            "10000",
            "Profile upload timeout; 0 inherits (default 10000 ms)",
        ),
        Spec::enumeration::<ProfileSymbolization>(
            "profile-symbolization",
            &["MARS_PROFILE_SYMBOLIZATION", "GST_OTEL_PROFILE_SYMBOLIZATION"],
            "full",
            "CPU symbol detail; auto inherits (default full)",
        ),
        Spec::enumeration::<Toggle>(
            "profile-elements",
            &["MARS_PROFILE_ELEMENTS", "GST_OTEL_PROFILE_ELEMENTS"],
            "true",
            "Element attribution; auto inherits (default true). Required for NVTX attribution",
        ),
        Spec::enumeration::<Toggle>(
            "profile-memory-trim", &[], "true",
            "One background allocator trim after the first pipeline reaches PLAYING (Linux/glibc profiles only). Default true: returns retained startup heap pages; our processor benchmark saved about 450 MiB running RSS for a 60-70 ms one-time allocator operation. Affects the whole process; does not reduce startup peak. Set false to disable",
        ),
        Spec::number(
            "profile-signal",
            128,
            &["MARS_PROFILE_SIGNAL", "GST_OTEL_PROFILE_SIGNAL"],
            "0",
            "CPU realtime signal number; 0 inherits (default SIGRTMIN+6)",
        ),
        #[cfg(all(feature = "gpu-profiles", target_os = "linux"))]
        Spec::string(
            "profile-gpu-adapter",
            &["MARS_GPU_ADAPTER"],
            Some("libmars-cupti.so"),
            "CUPTI adapter library; unset inherits (default libmars-cupti.so)",
        ),
        #[cfg(all(feature = "gpu-profiles", target_os = "linux"))]
        Spec::number(
            "profile-gpu-window-ms", 3_600_000, &[], "0",
            "GPU capture duration: 0 continuous (default), otherwise 10..=period milliseconds",
        ),
        #[cfg(all(feature = "gpu-profiles", target_os = "linux"))]
        Spec::number(
            "profile-gpu-period-ms", 3_600_000, &[], "10000",
            "GPU capture repetition interval in milliseconds; 0 inherits (default 10000). Only used with a nonzero window",
        ),
    ]
}

#[derive(Clone, Debug)]
pub(crate) struct Config {
    pub url: String,
    pub hz: u32,
    pub timeout_ms: u64,
    pub elements: bool,
    pub symbolization: String,
    pub signal: u32,
    pub memory_trim: bool,
    #[cfg(all(feature = "gpu-profiles", target_os = "linux"))]
    pub gpu_adapter: String,
    #[cfg(all(feature = "gpu-profiles", target_os = "linux"))]
    pub gpu_window_ms: u32,
    #[cfg(all(feature = "gpu-profiles", target_os = "linux"))]
    pub gpu_period_ms: u32,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Settings {
    pub sources: ProfileSources,
    pub nvtx_adapter: Option<String>,
    pub overrides: Overrides,
}

impl Settings {
    pub fn properties() -> Vec<glib::ParamSpec> {
        let mut properties = vec![
            ProfileSources::property(),
            #[cfg(all(feature = "profiles", target_os = "linux"))]
            glib::ParamSpecString::builder("profile-nvtx-adapter")
                .nick("NVTX attribution adapter")
                .blurb("NVTX worker attribution library; unset or empty disables it. Must match NVTX_INJECTION64_PATH set before startup")
                .construct_only()
                .build(),
        ];
        properties.extend(specs().into_iter().map(|s| s.property));
        properties
    }

    pub fn set_property(&mut self, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "profile-sources" => self.sources = value.get().unwrap(),
            "profile-nvtx-adapter" => {
                self.nvtx_adapter = value
                    .get::<Option<String>>()
                    .unwrap()
                    .filter(|s| !s.is_empty());
            }
            _ => self.overrides.set_property(value, pspec),
        }
    }

    pub fn property(&self, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "profile-sources" => self.sources.to_value(),
            "profile-nvtx-adapter" => self.nvtx_adapter.to_value(),
            _ => self.overrides.property(pspec),
        }
    }

    /// Consume shared profile parameters before the entry point parses its other settings.
    pub fn take_parameters(&mut self, params: &mut gstreamer::StructureRef) -> Result<(), String> {
        if [
            "profile-gpu-adapter",
            "profile-gpu-window-ms",
            "profile-gpu-period-ms",
        ]
        .iter()
        .any(|name| params.has_field(*name))
            && !cfg!(all(feature = "gpu-profiles", target_os = "linux"))
        {
            return Err(
                "GPU profile properties require the gpu-profiles Cargo feature on Linux".into(),
            );
        }
        self.overrides.take_parameters(params, &SPECS)?;
        if params.has_field("profile-sources") {
            self.sources = params
                .get::<String>("profile-sources")
                .map_err(|_| "profile-sources must be a quoted string")?
                .parse()?;
            params.remove_field("profile-sources");
        }
        if params.has_field("profile-nvtx-adapter") {
            if !cfg!(all(feature = "profiles", target_os = "linux")) {
                return Err(
                    "profile-nvtx-adapter requires the profiles Cargo feature on Linux".into(),
                );
            }
            self.nvtx_adapter = Some(
                params
                    .get::<String>("profile-nvtx-adapter")
                    .map_err(|_| "profile-nvtx-adapter must be a quoted string")?,
            )
            .filter(|s| !s.is_empty());
            params.remove_field("profile-nvtx-adapter");
        }
        Ok(())
    }

    pub fn resolve(&self, required: bool) -> Result<Option<Config>, String> {
        self.resolve_with(required, |key| std::env::var(key).ok())
    }

    fn resolve_with(
        &self,
        required: bool,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Option<Config>, String> {
        let read = |name| {
            self.overrides.read(
                SPECS.iter().find(|s| s.property.name() == name).unwrap(),
                &env,
            )
        };
        let url = read("profile-url")?;
        if !required && url.is_none() {
            return Ok(None);
        }
        let url = url.unwrap_or_else(|| "http://localhost:4040".into());
        let parsed = reqwest::Url::parse(&url).map_err(|e| format!("profile-url: {e}"))?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err("profile-url must be an HTTP(S) URL".into());
        }
        let cpu = self.sources.cpu();
        #[cfg(all(feature = "gpu-profiles", target_os = "linux"))]
        let (gpu_window_ms, gpu_period_ms) = {
            let window: u32 = read("profile-gpu-window-ms")?.unwrap().parse().unwrap();
            let period: u32 = read("profile-gpu-period-ms")?.unwrap().parse().unwrap();
            if self.sources.gpu() && window != 0 && (window < 10 || window > period) {
                return Err(
                    "profile-gpu-window-ms must be 0 (continuous) or 10..=profile-gpu-period-ms"
                        .into(),
                );
            }
            (window, period)
        };
        Ok(Some(Config {
            url,
            hz: if cpu {
                read("profile-hz")?.unwrap().parse().unwrap()
            } else {
                100
            },
            timeout_ms: read("profile-timeout-ms")?.unwrap().parse().unwrap(),
            elements: read("profile-elements")?.unwrap() == "true",
            memory_trim: read("profile-memory-trim")?.unwrap() == "true",
            symbolization: if cpu {
                read("profile-symbolization")?.unwrap()
            } else {
                "full".into()
            },
            signal: if cpu {
                read("profile-signal")?.unwrap().parse().unwrap()
            } else {
                0
            },
            #[cfg(all(feature = "gpu-profiles", target_os = "linux"))]
            gpu_adapter: if self.sources.gpu() {
                read("profile-gpu-adapter")?.unwrap()
            } else {
                String::new()
            },
            #[cfg(all(feature = "gpu-profiles", target_os = "linux"))]
            gpu_window_ms,
            #[cfg(all(feature = "gpu-profiles", target_os = "linux"))]
            gpu_period_ms,
        }))
    }
}

/// Sources collected when profiling is enabled on a tracer.
/// GPU variants are available only in Linux builds with `gpu-profiles` enabled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, glib::Enum)]
#[enum_type(name = "MarsProfileSources")]
#[repr(i32)]
pub enum ProfileSources {
    #[default]
    #[enum_value(name = "CPU profiles", nick = "cpu")]
    Cpu,
    #[cfg(all(feature = "gpu-profiles", target_os = "linux"))]
    #[enum_value(name = "GPU profiles", nick = "gpu")]
    Gpu,
    #[cfg(all(feature = "gpu-profiles", target_os = "linux"))]
    #[enum_value(name = "CPU and GPU profiles", nick = "cpu,gpu")]
    CpuAndGpu,
}

impl std::str::FromStr for ProfileSources {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut cpu = false;
        let mut gpu = false;
        for source in value.split(',').map(str::trim) {
            let selected = match source {
                "cpu" => &mut cpu,
                "gpu" => &mut gpu,
                _ => return Err("profile-sources must contain cpu, gpu, or cpu,gpu".into()),
            };
            if *selected {
                return Err(format!("duplicate profile source: {source}"));
            }
            *selected = true;
        }
        if gpu {
            #[cfg(all(feature = "gpu-profiles", target_os = "linux"))]
            return Ok(if cpu { Self::CpuAndGpu } else { Self::Gpu });
            #[cfg(not(all(feature = "gpu-profiles", target_os = "linux")))]
            return Err("GPU profiles require the gpu-profiles Cargo feature on Linux".into());
        }
        Ok(Self::Cpu)
    }
}

impl ProfileSources {
    pub(crate) fn cpu(self) -> bool {
        match self {
            Self::Cpu => true,
            #[cfg(all(feature = "gpu-profiles", target_os = "linux"))]
            Self::Gpu => false,
            #[cfg(all(feature = "gpu-profiles", target_os = "linux"))]
            Self::CpuAndGpu => true,
        }
    }

    pub(crate) fn gpu(self) -> bool {
        self != Self::Cpu
    }

    pub(crate) fn property() -> glib::ParamSpec {
        glib::ParamSpecEnum::builder::<Self>("profile-sources")
            .nick("Profile sources")
            .blurb("Sources collected when profiling is enabled; GPU requires a gpu-profiles build")
            .default_value(Self::Cpu)
            .construct_only()
            .build()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_properties_are_feature_gated() {
        gstreamer::init().unwrap();
        for name in ["profile-gpu-window-ms", "profile-gpu-period-ms"] {
            let supported = cfg!(all(feature = "gpu-profiles", target_os = "linux"));
            assert_eq!(
                Settings::properties().iter().any(|p| p.name() == name),
                supported
            );
            let mut params = crate::settings::parameters(Some(&format!("{name}=1000")))
                .unwrap()
                .unwrap();
            assert_eq!(
                Settings::default().take_parameters(&mut params).is_ok(),
                supported
            );
        }
    }

    #[cfg(all(feature = "gpu-profiles", target_os = "linux"))]
    #[test]
    fn capture_windows_validate_duration_and_override_direct_properties() {
        gstreamer::init().unwrap();
        let mut settings = Settings {
            sources: ProfileSources::Gpu,
            ..Default::default()
        };
        let default = settings.resolve_with(true, |_| None).unwrap().unwrap();
        assert_eq!((default.gpu_window_ms, default.gpu_period_ms), (0, 10000));
        let property = Settings::properties()
            .into_iter()
            .find(|p| p.name() == "profile-gpu-window-ms")
            .unwrap();
        settings.set_property(&200u32.to_value(), &property);
        let mut params = crate::settings::parameters(Some(
            "profile-gpu-window-ms=1000,profile-gpu-period-ms=10000",
        ))
        .unwrap()
        .unwrap();
        settings.take_parameters(&mut params).unwrap();
        let resolved = settings.resolve_with(true, |_| None).unwrap().unwrap();
        assert_eq!(
            (resolved.gpu_window_ms, resolved.gpu_period_ms),
            (1000, 10000)
        );
        for window in [1u32, 9, 10001] {
            settings.set_property(&window.to_value(), &property);
            assert!(settings.resolve_with(true, |_| None).is_err());
        }
        settings.set_property(&10000u32.to_value(), &property);
        assert!(settings.resolve_with(true, |_| None).is_ok());
        settings.set_property(&0u32.to_value(), &property);
        assert_eq!(
            settings
                .resolve_with(true, |_| None)
                .unwrap()
                .unwrap()
                .gpu_window_ms,
            0
        );
    }

    #[test]
    fn nvtx_property_needs_profiles_but_not_gpu_profiles() {
        gstreamer::init().unwrap();
        let properties = Settings::properties();
        let property = properties
            .iter()
            .find(|p| p.name() == "profile-nvtx-adapter");
        let supported = cfg!(all(feature = "profiles", target_os = "linux"));
        assert_eq!(property.is_some(), supported);
        let mut settings = Settings::default();
        let mut parameters =
            crate::settings::parameters(Some("profile-nvtx-adapter=\"/tmp/an adapter.so\""))
                .unwrap()
                .unwrap();
        let result = settings.take_parameters(&mut parameters);
        if supported {
            result.unwrap();
            assert_eq!(settings.sources, ProfileSources::Cpu);
            assert_eq!(settings.nvtx_adapter.as_deref(), Some("/tmp/an adapter.so"));
            assert_eq!(parameters.n_fields(), 0);
            let property = property.unwrap();
            assert!(property.flags().contains(glib::ParamFlags::CONSTRUCT_ONLY));
            settings.set_property(&"".to_value(), property);
            assert_eq!(
                settings.property(property).get::<Option<String>>().unwrap(),
                None
            );
        } else {
            assert!(result
                .unwrap_err()
                .contains("profiles Cargo feature on Linux"));
        }
    }

    #[test]
    fn profile_sources_match_build_capabilities() {
        assert_eq!(
            "cpu".parse::<ProfileSources>().unwrap(),
            ProfileSources::Cpu
        );
        for invalid in ["", "cpu,", "none", "typo", "cpu,cpu", "gpu,gpu"] {
            assert!(invalid.parse::<ProfileSources>().is_err(), "{invalid}");
        }
        let class = glib::EnumClass::new::<ProfileSources>();
        assert_eq!(class.values()[0].nick(), "cpu");
        #[cfg(all(feature = "gpu-profiles", target_os = "linux"))]
        {
            assert_eq!(class.values().len(), 3);
            assert_eq!(
                "gpu".parse::<ProfileSources>().unwrap(),
                ProfileSources::Gpu
            );
            for value in ["cpu,gpu", "gpu,cpu", " cpu, gpu "] {
                assert_eq!(
                    value.parse::<ProfileSources>().unwrap(),
                    ProfileSources::CpuAndGpu
                );
            }
        }
        #[cfg(not(all(feature = "gpu-profiles", target_os = "linux")))]
        {
            assert_eq!(class.values().len(), 1);
            for value in ["gpu", "cpu,gpu"] {
                assert!(value
                    .parse::<ProfileSources>()
                    .unwrap_err()
                    .contains("gpu-profiles Cargo feature"));
            }
        }
    }
}
