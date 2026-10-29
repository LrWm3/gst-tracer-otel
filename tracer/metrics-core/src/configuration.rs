//! Construction-time overrides shared by the tracer entry points.
//!
//! GObject calls construct-only setters even for omitted properties. Nullable
//! strings, numeric zero and enum `auto` therefore normally mean inherit. A
//! numeric property with a nonzero actual default instead accepts zero as a
//! value and has no environment fallback. Explicit values
//! (including false and values equal to the built-in default) always win over
//! the environment. No collector writes to the process environment.
use glib::prelude::*;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, glib::Enum)]
#[enum_type(name = "MarsToggle")]
#[repr(i32)]
pub enum Toggle {
    #[default]
    #[enum_value(nick = "auto")]
    Auto,
    #[enum_value(nick = "false")]
    Off,
    #[enum_value(nick = "true")]
    On,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, glib::Enum)]
#[enum_type(name = "MarsMonitoringSetting")]
#[repr(i32)]
pub enum MonitoringSetting {
    #[default]
    Auto,
    None,
    Core,
    All,
}

#[derive(Clone, Debug)]
pub struct Spec {
    pub property: glib::ParamSpec,
    pub environment: &'static [&'static str],
    pub fallback: Option<&'static str>,
}

impl Spec {
    /// A numeric property with an actual default and no environment fallback.
    /// Unlike inherited numeric properties, zero is an explicit value.
    pub fn number_with_default(name: &str, max: u32, default: &'static str, blurb: &str) -> Self {
        let parsed: u32 = default.parse().expect("numeric default");
        assert!(parsed != 0 && parsed <= max);
        Self {
            property: glib::ParamSpecUInt::builder(name)
                .maximum(max)
                .default_value(parsed)
                .blurb(blurb)
                .construct_only()
                .build(),
            environment: &[],
            fallback: Some(default),
        }
    }
    pub fn number(
        name: &str,
        max: u32,
        environment: &'static [&'static str],
        fallback: &'static str,
        blurb: &str,
    ) -> Self {
        Self {
            property: glib::ParamSpecUInt::builder(name)
                .maximum(max)
                .blurb(blurb)
                .construct_only()
                .build(),
            environment,
            fallback: Some(fallback),
        }
    }
    pub fn string(
        name: &str,
        environment: &'static [&'static str],
        fallback: Option<&'static str>,
        blurb: &str,
    ) -> Self {
        Self {
            property: glib::ParamSpecString::builder(name)
                .blurb(blurb)
                .construct_only()
                .build(),
            environment,
            fallback,
        }
    }
    pub fn enumeration<
        T: StaticType
            + Default
            + glib::translate::FromGlib<i32>
            + glib::translate::IntoGlib<GlibType = i32>,
    >(
        name: &str,
        environment: &'static [&'static str],
        fallback: &'static str,
        blurb: &str,
    ) -> Self {
        Self {
            property: glib::ParamSpecEnum::builder::<T>(name)
                .blurb(blurb)
                .construct_only()
                .build(),
            environment,
            fallback: Some(fallback),
        }
    }

    fn normalize(&self, text: &str, inherit: bool) -> Result<Option<String>, String> {
        let name = self.property.name();
        if let Some(spec) = self.property.downcast_ref::<glib::ParamSpecUInt>() {
            if spec.default_value() != 0 {
                let number = text
                    .parse::<u32>()
                    .map_err(|_| format!("{name} must be 0..={}", spec.maximum()))?;
                if number > spec.maximum() {
                    return Err(format!("{name} must be 0..={}", spec.maximum()));
                }
                return Ok(Some(number.to_string()));
            }
            let number = text
                .parse::<u32>()
                .map_err(|_| format!("{name} must be 1..={} (0 inherits)", spec.maximum()))?;
            if number > spec.maximum() || (!inherit && number == 0) {
                return Err(format!("{name} must be 1..={}", spec.maximum()));
            }
            return Ok((number != 0).then(|| number.to_string()));
        }
        if self.property.value_type().is_a(glib::Type::ENUM) {
            let class = glib::EnumClass::with_type(self.property.value_type()).unwrap();
            let normalized = if class.value_by_nick("true").is_some()
                && class.value_by_nick("false").is_some()
            {
                match text.to_ascii_lowercase().as_str() {
                    "true" | "1" | "yes" => "true".to_owned(),
                    "false" | "0" | "no" => "false".to_owned(),
                    _ => text.to_owned(),
                }
            } else {
                text.to_owned()
            };
            if normalized == "auto" && inherit {
                return Ok(None);
            }
            if normalized == "auto" || class.value_by_nick(&normalized).is_none() {
                return Err(format!("invalid {name}: {text}"));
            }
            return Ok(Some(normalized));
        }
        Ok(Some(text.to_owned()))
    }
}

#[derive(Clone, Debug, Default)]
pub struct Overrides(BTreeMap<String, String>);

impl Overrides {
    pub fn set_property(&mut self, value: &glib::Value, pspec: &glib::ParamSpec) {
        let text = if pspec.value_type() == String::static_type() {
            value.get::<Option<String>>().unwrap()
        } else if pspec.value_type() == u32::static_type() {
            let n = value.get::<u32>().unwrap();
            (n != 0 || pspec.default_value().get::<u32>().unwrap() != 0).then(|| n.to_string())
        } else {
            let (_, v) = glib::EnumValue::from_value(value).unwrap();
            (v.nick() != "auto").then(|| v.nick().to_owned())
        };
        if let Some(text) = text {
            self.0.insert(pspec.name().into(), text);
        } else {
            self.0.remove(pspec.name());
        }
    }

    pub fn property(&self, pspec: &glib::ParamSpec) -> glib::Value {
        let Some(value) = self.0.get(pspec.name()) else {
            return pspec.default_value().clone();
        };
        if pspec.value_type() == String::static_type() {
            value.to_value()
        } else if pspec.value_type() == u32::static_type() {
            value.parse::<u32>().unwrap().to_value()
        } else {
            glib::EnumClass::with_type(pspec.value_type())
                .unwrap()
                .to_value_by_nick(value)
                .unwrap()
        }
    }

    pub fn take_parameters(
        &mut self,
        params: &mut gstreamer::StructureRef,
        specs: &[Spec],
    ) -> Result<(), String> {
        for spec in specs {
            let name = spec.property.name();
            if !params.has_field(name) {
                continue;
            }
            let value = params.value(name).unwrap();
            let text = if let Ok(v) = value.get::<String>() {
                v
            } else if let Ok(v) = value.get::<bool>() {
                v.to_string()
            } else if let Ok(v) = value.get::<i32>() {
                v.to_string()
            } else if let Ok(v) = value.get::<u32>() {
                v.to_string()
            } else {
                return Err(format!("invalid value for {name}"));
            };
            if spec.property.value_type() == String::static_type() && value.get::<String>().is_err()
            {
                return Err(format!("{name} must be a string"));
            }
            if let Some(v) = spec.normalize(&text, true)? {
                self.0.insert(name.into(), v);
            } else {
                self.0.remove(name);
            }
            params.remove_field(name);
        }
        Ok(())
    }

    pub fn read(
        &self,
        spec: &Spec,
        env: &impl Fn(&str) -> Option<String>,
    ) -> Result<Option<String>, String> {
        if let Some(value) = self.0.get(spec.property.name()) {
            return Ok(Some(value.clone()));
        }
        for key in spec.environment {
            if let Some(value) = env(key) {
                return spec
                    .normalize(&value, false)
                    .map_err(|error| format!("{key}: {error}"));
            }
        }
        Ok(spec.fallback.map(str::to_owned))
    }

    pub fn contains(&self, name: &str) -> bool {
        self.0.contains_key(name)
    }

    pub fn read_named(
        &self,
        specs: &[Spec],
        name: &str,
        env: &impl Fn(&str) -> Option<String>,
    ) -> Result<Option<String>, String> {
        self.read(
            specs
                .iter()
                .find(|s| s.property.name() == name)
                .expect("known setting"),
            env,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glib::subclass::prelude::*;
    use std::sync::{LazyLock, Mutex};

    static SPECS: LazyLock<Vec<Spec>> = LazyLock::new(|| {
        vec![
            Spec::number(
                "sample-rate",
                1000,
                &["NEW_RATE", "OLD_RATE"],
                "100",
                "Sample rate; 0 inherits",
            ),
            Spec::enumeration::<Toggle>("enabled", &["ENABLED"], "false", "auto inherits"),
            Spec::string("destination", &["URL"], Some("default"), "unset inherits"),
        ]
    });
    mod imp {
        use super::*;
        #[derive(Default)]
        pub struct Probe(pub Mutex<Overrides>);
        #[glib::object_subclass]
        impl ObjectSubclass for Probe {
            const NAME: &'static str = "MarsConfigurationTestProbe";
            type Type = super::Probe;
        }
        impl ObjectImpl for Probe {
            fn properties() -> &'static [glib::ParamSpec] {
                static PROPS: LazyLock<Vec<glib::ParamSpec>> =
                    LazyLock::new(|| SPECS.iter().map(|s| s.property.clone()).collect());
                &PROPS
            }
            fn set_property(&self, _: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
                self.0.lock().unwrap().set_property(value, pspec);
            }
            fn property(&self, _: usize, pspec: &glib::ParamSpec) -> glib::Value {
                self.0.lock().unwrap().property(pspec)
            }
        }
    }
    glib::wrapper! { pub struct Probe(ObjectSubclass<imp::Probe>); }

    #[test]
    fn omitted_properties_inherit_but_explicit_defaults_override_environment() {
        gstreamer::init().unwrap();
        let env = |key: &str| {
            Some(
                match key {
                    "NEW_RATE" => "200",
                    "ENABLED" => "true",
                    _ => "environment",
                }
                .into(),
            )
        };
        let inherited: Probe = glib::Object::new();
        let values = inherited.imp().0.lock().unwrap();
        assert_eq!(values.read(&SPECS[0], &env).unwrap().unwrap(), "200");
        assert_eq!(values.read(&SPECS[1], &env).unwrap().unwrap(), "true");
        assert_eq!(
            values.read(&SPECS[2], &env).unwrap().unwrap(),
            "environment"
        );
        drop(values);
        let explicit: Probe = glib::Object::builder()
            .property("sample-rate", 100u32)
            .property("enabled", Toggle::Off)
            .property("destination", "default")
            .build();
        let mut values = explicit.imp().0.lock().unwrap();
        assert_eq!(values.read(&SPECS[0], &env).unwrap().unwrap(), "100");
        assert_eq!(values.read(&SPECS[1], &env).unwrap().unwrap(), "false");
        assert_eq!(values.read(&SPECS[2], &env).unwrap().unwrap(), "default");
        let mut params = "test,sample-rate=300,enabled=true,destination=params"
            .parse::<gstreamer::Structure>()
            .unwrap();
        values.take_parameters(&mut params, &SPECS).unwrap();
        assert_eq!(values.read(&SPECS[0], &env).unwrap().unwrap(), "300");
        assert_eq!(values.read(&SPECS[1], &env).unwrap().unwrap(), "true");
        assert_eq!(values.read(&SPECS[2], &env).unwrap().unwrap(), "params");
        // Explicit inheritance can also reset a constructor override.
        let mut params = "test,sample-rate=0,enabled=auto"
            .parse::<gstreamer::Structure>()
            .unwrap();
        values.take_parameters(&mut params, &SPECS).unwrap();
        assert_eq!(values.read(&SPECS[0], &env).unwrap().unwrap(), "200");
        assert_eq!(values.read(&SPECS[1], &env).unwrap().unwrap(), "true");
    }

    #[test]
    fn validation_and_environment_alias_order() {
        gstreamer::init().unwrap();
        let values = Overrides::default();
        assert_eq!(values.read(&SPECS[0], &|_| None).unwrap().unwrap(), "100");
        assert_eq!(
            values
                .read(&SPECS[0], &|key| (key == "OLD_RATE").then(|| "400".into()))
                .unwrap()
                .unwrap(),
            "400"
        );
        assert!(values
            .read(&SPECS[0], &|key| Some(
                if key == "NEW_RATE" { "invalid" } else { "400" }.into()
            ))
            .is_err());
        for input in [
            "sample-rate=-1",
            "sample-rate=1001",
            "sample-rate=bad",
            "enabled=bad",
            "destination=123",
        ] {
            let mut params = format!("test,{input}")
                .parse::<gstreamer::Structure>()
                .unwrap();
            assert!(
                Overrides::default()
                    .take_parameters(&mut params, &SPECS)
                    .is_err(),
                "{input}"
            );
        }
    }
}
