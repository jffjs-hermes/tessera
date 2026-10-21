//! S0 content kinds: the typed payloads of the six content kinds.
//!
//! Each content kind (Tuning, Skill, Ability, Status, Item, Quest) is **one
//! registered part** whose payload carries that kind's typed data. A
//! blueprint remains the unit of identity, inheritance and patching; the kind
//! part is the unit of validation and interpretation. This module owns the
//! schema — the structs, their serde validation and their deterministic
//! containers — and nothing else: it exposes **no rules, policy or tuning
//! semantics** (those are Luau's script territory per `docs/architecture.md`;
//! later increments add them). Consuming the parts is done through the
//! existing `PartRegistry`/`part_typed` seam, so inheritance and patch
//! behavior are exactly the loader's own.
//!
//! Registration: [`register_all`] declares the six part names in the single
//! `PartRegistry` the blueprint loader validates against (called from
//! [`crate::world::World::new`]), with `StorageClass::Dynamic`, matching how
//! other mod-defined content parts are stored.
//!
//! Determinism (see `docs/determinism.md`): every container here is a
//! `BTreeMap` (sorted iteration) or a `Vec` in declared order (quest stages);
//! there is no `HashMap`/`HashSet` anywhere in this module.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::ecs::{Part, PartError, PartRegistry, PartTypeId, StorageClass};

// ---------------------------------------------------------------------------
// The six kind payloads
// ---------------------------------------------------------------------------

/// `parts.Ability`: identity of an active/passive/reactive ability.
/// `kind` and `target` are closed enums; later increments extend the
/// vocabulary as a schema change, never silently.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ability {
    pub name: String,
    pub kind: AbilityKind,
    pub target: AbilityTarget,
    /// Free-form tags (`["melee", "stagger"]`); order is authored and kept.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// How an ability is used. `active` spends an action/charges; `passive`/
/// `reactive` never do (Slice Readiness: the flag is kept, active is the
/// slice path).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AbilityKind {
    Active,
    Passive,
    Reactive,
}

/// What an ability can be pointed at. The vocabulary here is the minimum
/// that makes the Shield Bash proof work; later increments add line/area
/// targeting with a schema change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AbilityTarget {
    /// No targeting — applies to the actor using the ability.
    User,
    /// The user or any actor on the 8 tiles around the user.
    Adjacent,
}

/// `parts.Skill`: identity of a skill plus its rank progression data.
/// Rank tables are declared order (`Vec`); ranks are small integers, and the
/// TOML integer-key table grammar does not survive the loader's raw
/// `toml::Value` round-trip, so per-rank data uses explicit `{ rank, ... }`
/// entries instead of `{ 1 = ..., 2 = ... }` maps.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Skill {
    pub name: String,
    pub family: String,
    /// Hidden skills (used for mechanics) don't show in a progression UI.
    #[serde(default)]
    pub hidden: bool,
    /// Rank -> point cost, in declared rank order.
    #[serde(default)]
    pub rank_cost: Vec<RankCost>,
    /// Rank -> ability ids granted at that rank, in declared rank order.
    #[serde(default)]
    pub grants: Vec<RankGrants>,
}

/// One entry of `Skill.rank_cost`: reaching `rank` costs `cost` points.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RankCost {
    pub rank: u32,
    pub cost: i64,
}

/// One entry of `Skill.grants`: earning `rank` grants the listed ability ids.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RankGrants {
    pub rank: u32,
    #[serde(default)]
    pub abilities: Vec<String>,
}

/// `parts.Status`: identity, duration and stacking policy of a status.
/// Runtime behavior (application, duration bookkeeping, modifiers) is later
/// increments; this is the schema only.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Status {
    pub name: String,
    /// Duration in game-time units.
    pub duration: i64,
    pub stacking: Stacking,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// How repeated applications of the same status id combine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stacking {
    /// Reapplying refreshes the duration of the existing instance.
    Refresh,
    /// Reapplying adds duration to the existing instance.
    Stack,
    /// Reapplying creates a separate instance (capped, FIFO).
    Independent,
}

/// `parts.Item`: identity and base item data. Equipment slots and
/// weapon/armor payloads are S2's contract; `slot` stays a string until S2
/// fixes the vocabulary.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Item {
    pub name: String,
    pub slot: String,
    pub weight: i64,
    pub price: i64,
}

/// `parts.Quest`: identity plus the ordered stage list. Each stage is reached
/// when its event trigger expression fires; stage semantics are later
/// increments'.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Quest {
    pub name: String,
    /// Stages in authored order — order is meaningful for progression.
    #[serde(default)]
    pub stages: Vec<QuestStage>,
}

/// One quest stage, reached when `trigger` fires.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestStage {
    /// Event trigger expression for this stage.
    pub trigger: String,
}

// ---------------------------------------------------------------------------
// Tuning
// ---------------------------------------------------------------------------

/// `parts.Tuning`: a named table of integer-only tuning values. The payload
/// *is* the map: `parts.Tuning = { base_hit = 50, crit_pct = { pct = 150 } }`.
///
/// Deterministic by construction: the `BTreeMap` iterates values in sorted
/// key order. Reads (the later `engine.tuning()` binding and the override
/// layer) are pure lookups over this container; this module only types and
/// validates the authored payload.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(transparent)]
pub struct Tuning {
    /// The named values, sorted by name. Any key whose value is a valid
    /// [`TuningValue`] is accepted; anything else is a load/deserialize error.
    pub values: BTreeMap<String, TuningValue>,
}

/// One tuning value: an integer, a percent-of-integer, or an integer list.
/// No floats anywhere (determinism rule — no floating point for gameplay
/// outcomes).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum TuningValue {
    /// `base_hit = 50`
    Int(i64),
    /// `crit_pct = { pct = 150 }` — a percent-of-integer value. Negative
    /// percents are legal (`recharge_pct = -8` style deltas).
    Percent(Percent),
    /// `speed_table = [50, 90, 140]`
    IntList(Vec<i64>),
}

/// The percent form of a tuning value (`{ pct = <int> }`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Percent {
    pub pct: i64,
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// The kind part name each id prefix requires, in fixed order (the order
/// naming-lint diagnostics are emitted in, so they are deterministic).
pub(crate) const KIND_PARTS: &[(&str, &str)] = &[
    ("Tuning_", "Tuning"),
    ("Skill_", "Skill"),
    ("Ability_", "Ability"),
    ("Status_", "Status"),
    ("Item_", "Item"),
    ("Quest_", "Quest"),
];

/// Register all six S0 kind parts in `registry` as `Dynamic` parts with a
/// serde factory. The blueprint loader validates content against exactly this
/// registry, so after this call `parts.Ability` & co. are known part names
/// and `part_typed` deserializes their payloads into the types above.
///
/// Fixed names can never collide at startup; calling it twice on the same
/// registry is a [`PartError::DuplicatePartType`] (single source of truth).
pub fn register_all(registry: &mut PartRegistry) -> Result<(), PartError> {
    register::<Tuning>(registry, "Tuning")?;
    register::<Skill>(registry, "Skill")?;
    register::<Ability>(registry, "Ability")?;
    register::<Status>(registry, "Status")?;
    register::<Item>(registry, "Item")?;
    register::<Quest>(registry, "Quest")?;
    Ok(())
}

/// A `PartFactory` deserializing the raw `toml::Value` payload into `T`.
fn build<T>(value: &toml::Value) -> Result<Box<dyn Part>, PartError>
where
    T: serde::de::DeserializeOwned + std::fmt::Debug + Send + Sync + 'static,
{
    let t: T = value
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| PartError::BuildFailed {
            name: std::any::type_name::<T>().to_string(),
            msg: e.to_string(),
        })?;
    Ok(Box::new(t))
}

fn register<T>(registry: &mut PartRegistry, name: &str) -> Result<(), PartError>
where
    T: serde::de::DeserializeOwned + std::fmt::Debug + Send + Sync + 'static,
{
    registry.register(PartTypeId::new(name), StorageClass::Dynamic, build::<T>)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deserialize an inline payload table (`{ ... }`) into `T` through the
    /// same `toml::Value` round-trip the loader's `part_typed` seam performs:
    /// parse to a `Value` under a dummy anchor key (a TOML document cannot
    /// *start* with an inline table), then extract that payload value and
    /// convert it into `T` — exactly the shape `parts.<Name>` reaches the
    /// part factory in.
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
    fn all_six_kind_parts_register_sorted_and_dynamic() {
        let mut r = PartRegistry::new();
        register_all(&mut r).unwrap();
        let names: Vec<&str> = r.iter_storage().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            names,
            vec!["Ability", "Item", "Quest", "Skill", "Status", "Tuning"]
        );
        for name in names {
            assert_eq!(
                r.storage(&PartTypeId::new(name)),
                Some(StorageClass::Dynamic),
                "{name}"
            );
        }
        // Duplicate registration is rejected (single source of truth; the
        // first registration in register_all order is "Tuning").
        assert!(matches!(
            register_all(&mut r),
            Err(PartError::DuplicatePartType(id)) if id.as_str() == "Tuning"
        ));
    }

    #[test]
    fn ability_payload_parses_and_rejects_unknowns() {
        let a: Ability = parse(
            r#"{ name = "Shield Bash", kind = "active", target = "adjacent", tags = ["melee", "stagger"] }"#,
        )
        .unwrap();
        assert_eq!(a.name, "Shield Bash");
        assert_eq!(a.kind, AbilityKind::Active);
        assert_eq!(a.target, AbilityTarget::Adjacent);
        assert_eq!(a.tags, vec!["melee".to_string(), "stagger".to_string()]);
        // missing optional field defaults
        let a: Ability = parse(r#"{ name = "X", kind = "passive", target = "user" }"#).unwrap();
        assert!(a.tags.is_empty());
        // unknown field
        let err =
            parse::<Ability>(r#"{ name = "X", kind = "active", target = "adjacent", bogus = 1 }"#)
                .unwrap_err();
        assert!(err.to_string().contains("bogus"), "{err}");
        // unknown enum variant
        let err =
            parse::<Ability>(r#"{ name = "X", kind = "magic", target = "adjacent" }"#).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("magic"), "{text}");
        assert!(text.contains("active"), "{text}");
    }

    #[test]
    fn skill_payload_parses_rank_tables_in_declared_order() {
        let s: Skill = parse(
            r#"{ name = "Blade", family = "weapon", hidden = true,
                 rank_cost = [ { rank = 1, cost = 10 }, { rank = 2, cost = 12 } ],
                 grants = [ { rank = 1, abilities = ["Ability_ShieldBash"] } ] }"#,
        )
        .unwrap();
        assert_eq!(s.name, "Blade");
        assert!(s.hidden);
        assert_eq!(s.rank_cost.len(), 2);
        assert_eq!(s.rank_cost[1].rank, 2);
        assert_eq!(s.rank_cost[1].cost, 12);
        assert_eq!(
            s.grants[0].abilities,
            vec!["Ability_ShieldBash".to_string()]
        );
        // empty rank tables default
        let s: Skill = parse(r#"{ name = "X", family = "f" }"#).unwrap();
        assert!(s.rank_cost.is_empty() && s.grants.is_empty());
        // wrong type inside a rank entry fails
        let err = parse::<Skill>(r#"{ name = "X", family = "f", rank_cost = [ { rank = 0 } ] }"#)
            .unwrap_err();
        assert!(err.to_string().contains("cost"), "{err}");
    }

    #[test]
    fn status_payload_parses_each_stacking_policy() {
        let st: Status = parse(
            r#"{ name = "Staggered", duration = 100, stacking = "refresh", tags = ["harmful", "control"] }"#,
        )
        .unwrap();
        assert_eq!(st.stacking, Stacking::Refresh);
        assert_eq!(st.duration, 100);
        let st: Status = parse(r#"{ name = "X", duration = 5, stacking = "stack" }"#).unwrap();
        assert_eq!(st.stacking, Stacking::Stack);
        let st: Status =
            parse(r#"{ name = "X", duration = 5, stacking = "independent" }"#).unwrap();
        assert_eq!(st.stacking, Stacking::Independent);
        let err =
            parse::<Status>(r#"{ name = "X", duration = 5, stacking = "expire" }"#).unwrap_err();
        assert!(err.to_string().contains("expire"), "{err}");
    }

    #[test]
    fn item_payload_parses_and_requires_all_fields() {
        let it: Item = parse(r#"{ name = "Rock", slot = "misc", weight = 1, price = 0 }"#).unwrap();
        assert_eq!(it.weight, 1);
        let err = parse::<Item>(r#"{ name = "Rock", slot = "misc" }"#).unwrap_err();
        assert!(err.to_string().contains("weight"), "{err}");
    }

    #[test]
    fn quest_payload_parses_ordered_stages() {
        let q: Quest = parse(
            r#"{ name = "First Blood",
                 stages = [ { trigger = "on_kill" }, { trigger = "on_return" } ] }"#,
        )
        .unwrap();
        assert_eq!(q.stages.len(), 2);
        assert_eq!(q.stages[1].trigger, "on_return");
        // a stage without a trigger is invalid
        let err = parse::<Quest>(r#"{ name = "X", stages = [ { text = "hi" } ] }"#).unwrap_err();
        assert!(err.to_string().contains("trigger"), "{err}");
        // stages default empty
        let q: Quest = parse(r#"{ name = "X" }"#).unwrap();
        assert!(q.stages.is_empty());
    }

    #[test]
    fn tuning_supports_all_three_value_forms() {
        let t: Tuning = parse(
            r#"{ base_hit = 50, crit_pct = { pct = 150 }, speed_table = [50, 90, 140], recharge_pct = { pct = -8 } }"#,
        )
        .unwrap();
        assert_eq!(t.values.get("base_hit"), Some(&TuningValue::Int(50)));
        assert_eq!(
            t.values.get("crit_pct"),
            Some(&TuningValue::Percent(Percent { pct: 150 }))
        );
        assert_eq!(
            t.values.get("speed_table"),
            Some(&TuningValue::IntList(vec![50, 90, 140]))
        );
        assert_eq!(
            t.values.get("recharge_pct"),
            Some(&TuningValue::Percent(Percent { pct: -8 }))
        );
    }

    #[test]
    fn tuning_values_are_integer_only_and_fail_closed() {
        // float value
        let err = parse::<Tuning>("{ base_hit = 50.5 }").unwrap_err();
        assert!(err.to_string().contains("TuningValue"), "{err}");
        // string value
        let err = parse::<Tuning>(r#"{ base_hit = "fifty" }"#).unwrap_err();
        assert!(err.to_string().contains("TuningValue"), "{err}");
        // float inside a list
        let err = parse::<Tuning>("{ speed_table = [1, 2.5] }").unwrap_err();
        assert!(err.to_string().contains("TuningValue"), "{err}");
        // unknown table shape (not the pct form)
        let err = parse::<Tuning>(r#"{ thing = { unit = "pct", value = 10 } }"#).unwrap_err();
        assert!(err.to_string().contains("TuningValue"), "{err}");
        // array of tables
        let err = parse::<Tuning>("{ thing = [ { a = 1 } ] }").unwrap_err();
        assert!(err.to_string().contains("TuningValue"), "{err}");
    }

    #[test]
    fn tuning_containers_iterate_in_sorted_key_order() {
        // Declared in scrambled order; the BTreeMap container must surface
        // values sorted regardless of authoring order (determinism rule 3).
        let t: Tuning = parse("{ zeta = 1, alpha = 2, Mango = 3, banana = 4 }").unwrap();
        let keys: Vec<&String> = t.values.keys().collect();
        // Byte order: uppercase sorts before lowercase.
        assert_eq!(keys, vec!["Mango", "alpha", "banana", "zeta"]);
    }

    #[test]
    fn registry_builds_each_kind_through_the_real_factory() {
        let mut r = PartRegistry::new();
        register_all(&mut r).unwrap();
        let cfg: toml::Value =
            parse(r#"{ name = "Shield Bash", kind = "active", target = "adjacent" }"#).unwrap();
        let part = r.build(&PartTypeId::new("Ability"), &cfg).unwrap();
        let a = part.as_ref().as_any().downcast_ref::<Ability>().unwrap();
        assert_eq!(a.name, "Shield Bash");
        // wrong-typed payload surfaces as BuildFailed through the registry
        let cfg: toml::Value = parse(r#"{ tile = "creature.goblin" }"#).unwrap();
        let err = match r.build(&PartTypeId::new("Ability"), &cfg) {
            Ok(_) => panic!("wrong-typed payload must fail"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("name"), "{err}");
    }
}
