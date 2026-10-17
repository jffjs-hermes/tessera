//! Registered part types and the registry binding TOML part names to Rust types.
//!
//! On load the blueprint loader validates every part name against the registry
//! (an unknown part name is a load error) but keeps the raw `toml::Value`
//! payload until spawning, so a part registered later can still parse the same
//! file. The registry is the typed deserialization seam: it turns a stored value
//! into a concrete [`Part`] via [`PartRegistry::parse`].
//!
//! Deterministic: a `BTreeMap` keyed by part name, so iteration is stable and no
//! `HashMap`/`HashSet` appears anywhere in the part store (determinism rule 3).

use std::any::Any;
use std::collections::BTreeMap;
use std::fmt;

use serde::de::DeserializeOwned;

/// A dynamically-typed part once deserialized from a blueprint.
///
/// Concrete part types implement `DeserializeOwned` (omitted fields fall back to
/// serde defaults; unknown fields should be rejected with
/// `#[serde(deny_unknown_fields)]`) and this trait so a `Box<dyn Part>` can be
/// downcast back to its concrete type.
pub trait Part: fmt::Debug + Send + Sync + 'static {
    fn as_any(&self) -> &dyn Any;
}

/// Deserialize a `toml::Value` payload into a `Box<dyn Part>`.
type PartFactory = fn(&toml::Value) -> Result<Box<dyn Part>, String>;

/// Maps registered part names to their concrete Rust types.
#[derive(Default)]
pub struct PartRegistry {
    parts: BTreeMap<&'static str, PartFactory>,
}

impl PartRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a concrete part type under `name`. Engine-known parts (and, at
    /// M4+, mod-supplied parts) announce themselves to the loader this way.
    pub fn register<P>(&mut self, name: &'static str)
    where
        P: Part + DeserializeOwned,
    {
        self.parts.insert(
            name,
            |value: &toml::Value| -> Result<Box<dyn Part>, String> {
                let part: P = value
                    .clone()
                    .try_into()
                    .map_err(|e: toml::de::Error| e.to_string())?;
                Ok(Box::new(part))
            },
        );
    }

    pub fn contains(&self, name: &str) -> bool {
        self.parts.contains_key(name)
    }

    /// Deserialize a stored `toml::Value` into the registered part's concrete type.
    pub fn parse(&self, name: &str, value: &toml::Value) -> Result<Box<dyn Part>, String> {
        let factory = self
            .parts
            .get(name)
            .ok_or_else(|| format!("no registered part type for '{name}'"))?;
        factory(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq, Default)]
    #[serde(deny_unknown_fields)]
    struct Stats {
        #[serde(default)]
        strength: u8,
        #[serde(default)]
        agility: u8,
    }

    impl Part for Stats {
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[test]
    fn register_and_parse_roundtrip() {
        let mut r = PartRegistry::new();
        r.register::<Stats>("Stats");
        assert!(r.contains("Stats"));
        assert!(!r.contains("Nope"));

        let value: toml::Value = toml::from_str("strength = 7").unwrap();
        let part = r.parse("Stats", &value).unwrap();
        assert_eq!(
            part.as_any().downcast_ref::<Stats>(),
            Some(&Stats {
                strength: 7,
                agility: 0
            })
        );
    }

    #[test]
    fn unknown_part_and_bad_payload_error() {
        let mut r = PartRegistry::new();
        r.register::<Stats>("Stats");

        assert!(
            r.parse("Nope", &toml::Value::Table(Default::default()))
                .is_err()
        );

        let value: toml::Value = toml::from_str("strength = \"high\"").unwrap();
        let err = r.parse("Stats", &value).unwrap_err();
        assert!(err.contains("strength"), "{err}");
    }
}
