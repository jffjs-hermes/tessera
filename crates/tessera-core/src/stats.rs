//! The typed attribute, level and bonus parts, plus the derived-stat
//! vocabulary.
//!
//! This module owns the schema — the structs, their serde validation and
//! their deterministic containers — and nothing else: no formulas, no
//! recompute pipeline, no status math. Those are later increments; the
//! load-time invariants here (bounds, closed vocabularies) are the native
//! side of the engine/script boundary.
//!
//! Registration: [`register_all`] declares the three part names in the single
//! `PartRegistry` the blueprint loader validates against (called from
//! [`crate::world::World::new`]), with `StorageClass::Dynamic`, matching how
//! the content kinds and the effect parts are stored.
//!
//! Determinism (see `docs/determinism.md`): every container here is a
//! `BTreeMap` or a fixed-order struct; [`AttributeName`] iteration is spelled
//! out in byte order so per-attribute data is never processed in a
//! platform-dependent order.

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::combat::DamageType;
use crate::ecs::{PartError, PartRegistry};

/// `parts.Attributes`: the five native attribute values.
pub const PART_ATTRIBUTES: &str = "Attributes";
/// `parts.Level`: the persistent character level.
pub const PART_LEVEL: &str = "Level";
/// `parts.Bonuses`: flat additive bonuses sourced by later gear/skill layers.
pub const PART_BONUSES: &str = "Bonuses";

/// Load-time attribute bounds (design hard cap: 30).
pub const ATTRIBUTE_MIN: i64 = 1;
pub const ATTRIBUTE_MAX: i64 = 30;
/// Load-time level bounds (slice cap; the design cap is 20 and the xp flow
/// enforces the same bound).
pub const LEVEL_MIN: i64 = 1;
pub const LEVEL_MAX: i64 = 20;

/// The five attribute names, as a fixed native enum. Attribute names are
/// schema, never data: a misspelled field is already a serde load error via
/// `deny_unknown_fields`, and this enum gives code a closed vocabulary to
/// iterate attributes deterministically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AttributeName {
    Might,
    Agility,
    Toughness,
    Will,
    Intellect,
}

impl AttributeName {
    /// All five attributes in declaration order.
    pub const ALL: [AttributeName; 5] = [
        AttributeName::Might,
        AttributeName::Agility,
        AttributeName::Toughness,
        AttributeName::Will,
        AttributeName::Intellect,
    ];

    /// The five attributes in canonical byte order (`BTreeMap` order):
    /// Agility, Intellect, Might, Toughness, Will. Use this when iterating
    /// per-attribute data that keys by name, so output is deterministic.
    pub const SORTED: [AttributeName; 5] = [
        AttributeName::Agility,
        AttributeName::Intellect,
        AttributeName::Might,
        AttributeName::Toughness,
        AttributeName::Will,
    ];

    /// The canonical lowercase spelling of this attribute name.
    pub fn as_str(self) -> &'static str {
        match self {
            AttributeName::Might => "might",
            AttributeName::Agility => "agility",
            AttributeName::Toughness => "toughness",
            AttributeName::Will => "will",
            AttributeName::Intellect => "intellect",
        }
    }

    /// Parse a canonical attribute name (lowercase, exact). Unknown names are
    /// `None` — callers fail closed rather than guessing.
    ///
    /// The `from_str` name is the fixed vocabulary the stats design notes pin;
    /// it deliberately shadows the std trait's associated method, so the
    /// trait lint is allowed here.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(name: &str) -> Option<AttributeName> {
        match name {
            "might" => Some(AttributeName::Might),
            "agility" => Some(AttributeName::Agility),
            "toughness" => Some(AttributeName::Toughness),
            "will" => Some(AttributeName::Will),
            "intellect" => Some(AttributeName::Intellect),
            _ => None,
        }
    }
}

/// `parts.Attributes`: five required integer values, each a load-time
/// invariant in `[1, 30]`. Out-of-range values are serde load errors (with
/// the file/id context the loader adds), the same fail-closed style as every
/// other part.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attributes {
    #[serde(deserialize_with = "attribute_field")]
    pub might: i64,
    #[serde(deserialize_with = "attribute_field")]
    pub agility: i64,
    #[serde(deserialize_with = "attribute_field")]
    pub toughness: i64,
    #[serde(deserialize_with = "attribute_field")]
    pub will: i64,
    #[serde(deserialize_with = "attribute_field")]
    pub intellect: i64,
}

/// `parts.Level`: one required integer value, a load-time invariant in
/// `[1, 20]`. A separate part so a level change never restates the five
/// attributes (whole-part replace semantics).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Level {
    #[serde(deserialize_with = "level_field")]
    pub level: i64,
}

/// `parts.Bonuses`: flat additive bonuses for the derived stats, sourced by
/// gear/skill layers that land later. All fields default to 0 so a partial
/// table (`parts.Bonuses = { accuracy = 3 }`) loads as zeros elsewhere.
///
/// The per-type `resist`/`threshold` maps are the *innate* sources of the
/// combat cache (race, skills): the rules script adds them to whatever the
/// equipped items contribute, so an equipped item is never written back here.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Bonuses {
    #[serde(default)]
    pub accuracy: i64,
    #[serde(default)]
    pub dodge: i64,
    #[serde(default)]
    pub ac: i64,
    #[serde(default)]
    pub damage_flat: i64,
    #[serde(default)]
    pub block: i64,
    /// Innate per-type resistance in percent.
    #[serde(default)]
    pub resist: std::collections::BTreeMap<DamageType, i64>,
    /// Innate per-type flat damage reduction.
    #[serde(default)]
    pub threshold: std::collections::BTreeMap<DamageType, i64>,
}

/// One attribute value, validated to `[ATTRIBUTE_MIN, ATTRIBUTE_MAX]`.
fn attribute_field<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = i64::deserialize(deserializer)?;
    if (ATTRIBUTE_MIN..=ATTRIBUTE_MAX).contains(&value) {
        Ok(value)
    } else {
        Err(D::Error::custom(format!(
            "attribute must be in [{ATTRIBUTE_MIN}, {ATTRIBUTE_MAX}], found {value}"
        )))
    }
}

/// One level value, validated to `[LEVEL_MIN, LEVEL_MAX]`.
fn level_field<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = i64::deserialize(deserializer)?;
    if (LEVEL_MIN..=LEVEL_MAX).contains(&value) {
        Ok(value)
    } else {
        Err(D::Error::custom(format!(
            "level must be in [{LEVEL_MIN}, {LEVEL_MAX}], found {value}"
        )))
    }
}

/// The fixed derived-stat vocabulary. `max_hp`, `accuracy`, `dodge`, `ac`,
/// `defense`, `speed` are the computed set; the remaining four stay
/// reserved-and-uncomputed until later layers populate them (they are valid
/// modifier targets today, but the `Derived` cache may not carry them).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DerivedStat {
    MaxHp,
    Accuracy,
    Dodge,
    Ac,
    Defense,
    Speed,
    DamageDealt,
    DamageTaken,
    Healing,
    Block,
}

impl DerivedStat {
    /// All ten stat names in declaration order.
    pub const ALL: [DerivedStat; 10] = [
        DerivedStat::MaxHp,
        DerivedStat::Accuracy,
        DerivedStat::Dodge,
        DerivedStat::Ac,
        DerivedStat::Defense,
        DerivedStat::Speed,
        DerivedStat::DamageDealt,
        DerivedStat::DamageTaken,
        DerivedStat::Healing,
        DerivedStat::Block,
    ];

    /// The computed set: names the `Derived` cache may carry today.
    pub const COMPUTED: [DerivedStat; 6] = [
        DerivedStat::MaxHp,
        DerivedStat::Accuracy,
        DerivedStat::Dodge,
        DerivedStat::Ac,
        DerivedStat::Defense,
        DerivedStat::Speed,
    ];

    /// The canonical lowercase spelling of this stat name.
    pub fn as_str(self) -> &'static str {
        match self {
            DerivedStat::MaxHp => "max_hp",
            DerivedStat::Accuracy => "accuracy",
            DerivedStat::Dodge => "dodge",
            DerivedStat::Ac => "ac",
            DerivedStat::Defense => "defense",
            DerivedStat::Speed => "speed",
            DerivedStat::DamageDealt => "damage_dealt",
            DerivedStat::DamageTaken => "damage_taken",
            DerivedStat::Healing => "healing",
            DerivedStat::Block => "block",
        }
    }

    /// Parse a canonical stat name (lowercase, exact). Unknown names are
    /// `None` — callers fail closed rather than guessing.
    ///
    /// Same fixed `from_str` name as [`AttributeName::from_str`]; the std
    /// trait's method is deliberately shadowed (see above).
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(name: &str) -> Option<DerivedStat> {
        match name {
            "max_hp" => Some(DerivedStat::MaxHp),
            "accuracy" => Some(DerivedStat::Accuracy),
            "dodge" => Some(DerivedStat::Dodge),
            "ac" => Some(DerivedStat::Ac),
            "defense" => Some(DerivedStat::Defense),
            "speed" => Some(DerivedStat::Speed),
            "damage_dealt" => Some(DerivedStat::DamageDealt),
            "damage_taken" => Some(DerivedStat::DamageTaken),
            "healing" => Some(DerivedStat::Healing),
            "block" => Some(DerivedStat::Block),
            _ => None,
        }
    }

    /// True when this stat is in the computed set (carriable by `Derived`).
    pub fn is_computed(self) -> bool {
        DerivedStat::COMPUTED.contains(&self)
    }
}

/// Register the three stat parts as `Dynamic` parts with a serde factory, so
/// `parts.Attributes` / `parts.Level` / `parts.Bonuses` are known part names
/// and `part_typed` deserializes their payloads into the types above.
///
/// Fixed names can never collide; calling twice on the same registry is a
/// [`PartError::DuplicatePartType`].
pub fn register_all(registry: &mut PartRegistry) -> Result<(), PartError> {
    crate::kinds::register::<Attributes>(registry, PART_ATTRIBUTES)?;
    crate::kinds::register::<Bonuses>(registry, PART_BONUSES)?;
    crate::kinds::register::<Level>(registry, PART_LEVEL)?;
    Ok(())
}

impl Serialize for AttributeName {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for AttributeName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        AttributeName::from_str(&name).ok_or_else(|| {
            D::Error::custom(format!(
                "unknown attribute '{name}' (expected one of: {})",
                AttributeName::SORTED.map(AttributeName::as_str).join(", ")
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deserialize an inline payload table (`{ ... }`) into `T` through the
    /// same `toml::Value` round-trip the loader's `part_typed` seam performs.
    fn parse<T>(toml_text: &str) -> Result<T, toml::de::Error>
    where
        T: serde::de::DeserializeOwned,
    {
        let anchored = format!("payload = {toml_text}");
        let value: toml::Value = toml::from_str(&anchored)?;
        let payload = value
            .get("payload")
            .expect("anchored document always carries the payload key")
            .clone();
        payload.try_into()
    }

    #[test]
    fn attributes_parse_at_the_bounds_and_in_between() {
        let a: Attributes =
            parse(r#"{ might = 1, agility = 30, toughness = 14, will = 8, intellect = 11 }"#)
                .unwrap();
        assert_eq!(a.might, 1);
        assert_eq!(a.agility, 30);
        assert_eq!(a.toughness, 14);
        assert_eq!(a.will, 8);
        assert_eq!(a.intellect, 11);
    }

    #[test]
    fn attributes_reject_out_of_range_and_unknown_fields() {
        // Below and above the bound, each field name in turn. The base is the
        // other four fields at 10 so no key is stated twice.
        for (field, bad) in [
            ("might", 0i64),
            ("agility", 31),
            ("toughness", -3),
            ("will", 99),
            ("intellect", 0),
        ] {
            let rest = AttributeName::ALL
                .iter()
                .filter(|a| a.as_str() != field)
                .map(|a| format!("{} = 10", a.as_str()))
                .collect::<Vec<String>>()
                .join(", ");
            let err = parse::<Attributes>(&format!("{{ {rest}, {field} = {bad} }}")).unwrap_err();
            let text = err.to_string();
            assert!(text.contains(field), "field {field}: {text}");
            assert!(text.contains("[1, 30]"), "field {field}: {text}");
        }
        // Missing fields are required (no default attributes).
        let err = parse::<Attributes>(r#"{ might = 5 }"#).unwrap_err();
        assert!(err.to_string().contains("agility"), "{err}");
        // Unknown field name fails closed (typos are load errors).
        let err = parse::<Attributes>(
            r#"{ might = 10, agility = 10, toughness = 10, will = 10, intellect = 10, charisma = 18 }"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("charisma"), "{err}");
    }

    #[test]
    fn level_parses_at_the_bounds_and_rejects_out_of_range() {
        let l: Level = parse(r#"{ level = 1 }"#).unwrap();
        assert_eq!(l.level, 1);
        let l: Level = parse(r#"{ level = 20 }"#).unwrap();
        assert_eq!(l.level, 20);
        for bad in [0i64, 21, -1, 100] {
            let err = parse::<Level>(&format!("{{ level = {bad} }}")).unwrap_err();
            assert!(err.to_string().contains("[1, 20]"), "level {bad}: {err}");
        }
        let err = parse::<Level>(r#"{ level = 3, xp = 50 }"#).unwrap_err();
        assert!(err.to_string().contains("xp"), "{err}");
    }

    #[test]
    fn bonuses_default_to_zero_and_reject_unknowns() {
        // Default constant is all zeros.
        assert_eq!(
            Bonuses::default(),
            Bonuses {
                accuracy: 0,
                dodge: 0,
                ac: 0,
                damage_flat: 0,
                block: 0,
                resist: Default::default(),
                threshold: Default::default(),
            }
        );
        // Empty table deserializes to zeros.
        let b: Bonuses = parse(r#"{ }"#).unwrap();
        assert_eq!(b, Bonuses::default());
        // Partial tables fill zeros elsewhere.
        let b: Bonuses = parse(r#"{ accuracy = 3 }"#).unwrap();
        assert_eq!(
            b,
            Bonuses {
                accuracy: 3,
                ..Default::default()
            }
        );
        let b: Bonuses = parse(r#"{ ac = 2 }"#).unwrap();
        assert_eq!(b.ac, 2);
        assert_eq!(b.accuracy, 0);
        // Innate per-type resistances load and reject unknown types.
        let b: Bonuses = parse(r#"{ resist = { slash = 10 }, threshold = { pierce = 2 } }"#).unwrap();
        assert_eq!(b.resist.get(&crate::combat::DamageType::Slash), Some(&10));
        assert_eq!(b.threshold.get(&crate::combat::DamageType::Pierce), Some(&2));
        let err = parse::<Bonuses>(r#"{ resist = { acid = 10 } }"#).unwrap_err();
        assert!(err.to_string().contains("acid"), "{err}");
        // Unknown field fails closed.
        let err = parse::<Bonuses>(r#"{ accuracy = 3, evasion = 1 }"#).unwrap_err();
        assert!(err.to_string().contains("evasion"), "{err}");
    }

    #[test]
    fn attribute_name_round_trips_and_sorts_in_byte_order() {
        for attr in AttributeName::ALL {
            assert_eq!(AttributeName::from_str(attr.as_str()), Some(attr));
        }
        assert_eq!(AttributeName::from_str("strength"), None);
        assert_eq!(AttributeName::from_str("AGILITY"), None);
        assert_eq!(AttributeName::from_str(""), None);

        // Byte order of the canonical names (BTreeMap order).
        let sorted: Vec<&str> = AttributeName::SORTED.iter().map(|a| a.as_str()).collect();
        assert_eq!(
            sorted,
            vec!["agility", "intellect", "might", "toughness", "will"]
        );
    }

    #[test]
    fn stats_parts_register_sorted_and_dynamic() {
        let mut r = PartRegistry::new();
        register_all(&mut r).unwrap();
        let names: Vec<&str> = r.iter_storage().map(|(id, _)| id.as_str()).collect();
        assert_eq!(names, vec!["Attributes", "Bonuses", "Level"]);
        for name in names {
            assert_eq!(
                r.storage(&crate::ecs::PartTypeId::new(name)),
                Some(crate::ecs::StorageClass::Dynamic),
                "{name}"
            );
        }
        assert!(matches!(
            register_all(&mut r),
            Err(PartError::DuplicatePartType(id)) if id.as_str() == "Attributes"
        ));
    }
}
