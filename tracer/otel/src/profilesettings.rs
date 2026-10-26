//! Profile sources are selected at tracer construction, never from the environment.
use glib::prelude::*;

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
