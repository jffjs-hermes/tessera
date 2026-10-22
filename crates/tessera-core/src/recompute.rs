//! The native side of the stats recompute pipeline (–).
//!
//! The engine dispatches [`RecomputeDerived`] at every declared dirtying
//! point; the scripted `rules/stats.luau` handler computes the five formula
//! *bases* and writes them through the whitelisted `set_part` path; this
//! module owns the native **finalize** seam that runs after that write lands
//! and before control returns to the scheduler:
//!
//! 1. apply live status modifiers per stat (the fixed order — every `add`
//!    across live instances, then every `pct` — via native `Modifiers` math,
//!    ),
//! 2. apply the invariant clamps (`max_hp >= 1`, `speed >= 20`,
//!    `accuracy`/`dodge`/`ac`/`defense` >= 0, ),
//! 3. clamp `Health.hp` to `min(hp, max_hp)` .
//!
//! It also owns the native cache-only read helper [`derived_or_energy_speed`]
//! : every hot-loop speed read goes through `Derived.speed` when the
//! entity carries the cache, and falls back to the legacy `ActorEnergy.speed`
//! for entities without `Attributes` (none after the stats base data change, but
//! mods() may spawn bare parts). No read path calls into Luau.

use crate::ecs::{EntityBroker, PartTypeId};
use crate::effects::{Derived, PART_DERIVED, PART_STATUS_EFFECTS, StatusEffects};
use crate::events::{Health, PART_HEALTH, SimContext};
use crate::stats::{Attributes, DerivedStat, PART_ATTRIBUTES};
use crate::turns::{ActorEnergy, PART_ENERGY};

/// The post-modifier invariant floors . `speed` keeps the
/// prototype's `max(sp, 20)` floor — the bound that keeps a degenerate
/// modifier stack from stalling the scheduler; the Agility band clamp is the
/// formula's own and already applied in `rules/stats.luau`.
pub fn invariant_clamp(stat: DerivedStat, value: i64) -> i64 {
    match stat {
        DerivedStat::MaxHp => value.max(1),
        DerivedStat::Speed => value.max(20),
        DerivedStat::Accuracy | DerivedStat::Dodge | DerivedStat::Ac | DerivedStat::Defense => {
            value.max(0)
        }
        // Reserved-and-uncomputed stats never reach the finalize seam in stats.
        DerivedStat::DamageDealt
        | DerivedStat::DamageTaken
        | DerivedStat::Healing
        | DerivedStat::Block => value,
    }
}

/// Apply a live status store's modifiers to one derived base, in the fixed fixed
/// order : every `add` across live instances, then every
/// `pct` (integer math, truncation toward zero — the fixed `Modifiers::apply`
/// convention, not Luau floor division). Iteration is deterministic: status
/// id sorted, then instance application order. Returns the post-modifier
/// value (unclamped).
pub fn apply_status_modifiers(base: i64, effects: &StatusEffects, stat: &str) -> i64 {
    let mut value = base;
    // Phase one: every add, in (status id, instance) order — adds commute,
    // the fixed order guarantees a deterministic sum even when several
    // statuses modify the same stat.
    for status in effects.statuses() {
        for instance in effects.instances(status) {
            if let Some(m) = instance.modifiers.get(stat) {
                value += m.add;
            }
        }
    }
    // Phase two: every pct, same deterministic order. Each pct step applies
    // to the running value (two 90% stacks: 120 -> 108 -> 97), matching the
    //  worked example exactly.
    for status in effects.statuses() {
        for instance in effects.instances(status) {
            if let Some(m) = instance.modifiers.get(stat)
                && m.pct != 0
            {
                value = value.saturating_mul(m.pct) / 100;
            }
        }
    }
    value
}

/// True when `entity` carries the native `Attributes` part — i.e. it is a
/// stat-ful entity whose `Derived` cache the pipeline owns .
pub fn carries_attributes(broker: &EntityBroker, entity: hecs::Entity) -> bool {
    broker
        .typed_part::<Attributes>(entity, &PartTypeId::new(PART_ATTRIBUTES))
        .is_some()
}

/// The native finalize seam: after the rules script wrote its formula bases
/// into `Derived`, read them, apply live status modifiers and the invariant
/// clamps, write the final cache back (key-validated), and enforce the
/// `Health.hp <= max_hp` invariant .
///
/// An entity without a `Derived` part (no script wrote — e.g. no rules
/// handler registered, or a legacy entity) is left untouched: the absent
/// cache is the detectable "not yet recomputed" state, and the caller decides
/// whether that is an error.
pub fn finalize_derived(ctx: &mut SimContext, entity: hecs::Entity) {
    let (bases, statuses) = {
        let broker = ctx.broker.read().unwrap_or_else(|p| p.into_inner());
        let Some(derived) = broker.typed_part::<Derived>(entity, &PartTypeId::new(PART_DERIVED))
        else {
            return;
        };
        (
            derived.values.clone(),
            broker
                .typed_part::<StatusEffects>(entity, &PartTypeId::new(PART_STATUS_EFFECTS))
                .cloned()
                .unwrap_or_default(),
        )
    };

    let mut final_values: std::collections::BTreeMap<String, i64> =
        std::collections::BTreeMap::new();
    for stat in DerivedStat::COMPUTED {
        let key = stat.as_str();
        let base = bases.get(key).copied().unwrap_or(0);
        let with_mods = apply_status_modifiers(base, &statuses, key);
        final_values.insert(key.to_string(), invariant_clamp(stat, with_mods));
    }
    let Ok(derived) = Derived::from_values(final_values.clone()) else {
        // The bases were key-validated on write; a failure here is a logic
        // error we do not paper over — leave the cache absent (detectable).
        return;
    };

    // The single writer of `Derived` (outside the script's own write): the
    // finalize seam. Replace the cache in one committed write.
    let max_hp = final_values.get("max_hp").copied();
    {
        let mut broker = ctx.broker.write().unwrap_or_else(|p| p.into_inner());
        let _ = broker.insert_part(
            entity,
            PART_DERIVED.to_string(),
            Box::new(derived) as Box<dyn crate::ecs::Part>,
        );
        // HP clamp invariant: a shrink in max (level drain, tuning sweep)
        // must never leave current HP above the cap. The script never touches
        // `Health`; native owns the invariant.
        if let Some(h) = broker.typed_part_mut::<Health>(entity, &PartTypeId::new(PART_HEALTH))
            && let Some(max_hp) = max_hp
        {
            h.hp =
                h.hp.min(max_hp.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32);
        }
    }
}

/// The single native hot-loop speed read : `Derived.speed`
/// when the entity carries the cache, else the legacy `ActorEnergy.speed`
/// (which nothing writes in stats; it remains the fallback for entities without
/// `Attributes`). Clamped to the sanity ceiling like every scheduler speed.
pub fn derived_or_energy_speed(ctx: &SimContext, entity: hecs::Entity) -> u32 {
    let broker = ctx.broker.read().unwrap_or_else(|p| p.into_inner());
    if let Some(derived) = broker.typed_part::<Derived>(entity, &PartTypeId::new(PART_DERIVED))
        && let Some(speed) = derived.values.get("speed")
        && *speed > 0
    {
        return (*speed).min(i64::from(u32::MAX)) as u32;
    }
    broker
        .typed_part::<ActorEnergy>(entity, &PartTypeId::new(PART_ENERGY))
        .map(|a| a.speed)
        .unwrap_or(crate::turns::BASE_SPEED)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::Modifiers;
    use crate::kinds::Stacking;

    fn mods(entries: &[(&str, i64, i64)]) -> Modifiers {
        // (stat, add, pct) triples through the serde seam, like real content.
        let mut toml = String::from("[m]\n");
        for (stat, add, pct) in entries {
            if *pct == 0 {
                toml.push_str(&format!("{stat} = {add}\n"));
            } else {
                toml.push_str(&format!("{stat} = {{ add = {add}, pct = {pct} }}\n"));
            }
        }
        let value: toml::Value = toml::from_str(&toml).expect("modifier toml");
        value
            .get("m")
            .expect("table")
            .clone()
            .try_into()
            .expect("modifiers")
    }

    /// One live status instance per entry list (`Independent` stacking so
    /// repeated calls coexist — the deterministic instance order is
    /// application order).
    fn effects_with(instances: &[&[(&str, i64, i64)]]) -> StatusEffects {
        let mut effects = StatusEffects::new();
        for entries in instances {
            effects.apply(
                "Status_Test",
                100,
                Stacking::Independent,
                &[],
                mods(entries),
            );
        }
        effects
    }

    #[test]
    fn modifier_pipeline_applies_every_add_then_every_pct() {
        // 120 base, +10 add then 90% pct in one modifier entry:
        // (120 + 10) * 90 / 100 = 117. If the pct ran first the result would
        // be 118 — the fixed add-then-pct order is asserted.
        let effects = effects_with(&[&[("speed", 10, 90)]]);
        assert_eq!(apply_status_modifiers(120, &effects, "speed"), 117);
    }

    #[test]
    fn two_percent_stacks_truncate_between_steps() {
        // The  worked example: Agility 20 base 120, two 90% stacks ->
        // 120 * 90 // 100 = 108, then 108 * 90 // 100 = 97 (the doc.s exact
        // numbers; truncation between pct steps is load-bearing).
        let effects = effects_with(&[&[("speed", 0, 90)], &[("speed", 0, 90)]]);
        assert_eq!(apply_status_modifiers(120, &effects, "speed"), 97);
    }

    #[test]
    fn unmodified_base_passes_through_and_other_stats_are_untouched() {
        let effects = effects_with(&[&[("speed", 0, 90)]]);
        assert_eq!(apply_status_modifiers(120, &effects, "speed"), 108);
        assert_eq!(apply_status_modifiers(4, &effects, "accuracy"), 4);
    }

    #[test]
    fn invariant_clamp_floors_are_exact() {
        assert_eq!(invariant_clamp(DerivedStat::MaxHp, 0), 1);
        assert_eq!(invariant_clamp(DerivedStat::MaxHp, -5), 1);
        assert_eq!(invariant_clamp(DerivedStat::MaxHp, 36), 36);
        assert_eq!(invariant_clamp(DerivedStat::Speed, 19), 20);
        assert_eq!(invariant_clamp(DerivedStat::Speed, 140), 140);
        for stat in [
            DerivedStat::Accuracy,
            DerivedStat::Dodge,
            DerivedStat::Defense,
            DerivedStat::Ac,
        ] {
            assert_eq!(invariant_clamp(stat, -3), 0, "{stat:?}");
            assert_eq!(invariant_clamp(stat, 21), 21, "{stat:?}");
        }
    }

    #[test]
    fn derived_from_values_validates_keys_fail_closed() {
        use std::collections::BTreeMap;
        let mut ok = BTreeMap::new();
        for stat in DerivedStat::COMPUTED {
            ok.insert(stat.as_str().to_string(), 1);
        }
        assert!(Derived::from_values(ok).is_ok());

        let mut unknown = BTreeMap::new();
        unknown.insert("hp".to_string(), 36);
        let err = Derived::from_values(unknown).unwrap_err();
        assert!(err.contains("unknown derived stat key 'hp'"), "{err}");

        let mut reserved = BTreeMap::new();
        reserved.insert("damage_dealt".to_string(), 5);
        let err = Derived::from_values(reserved).unwrap_err();
        assert!(err.contains("reserved"), "{err}");
    }

    #[test]
    fn carries_attributes_requires_the_native_part() {
        let mut broker = EntityBroker::new();
        let e = broker.spawn();
        assert!(!carries_attributes(&broker, e));
        broker
            .insert_part(
                e,
                PART_ATTRIBUTES.to_string(),
                Box::new(Attributes {
                    might: 10,
                    agility: 10,
                    toughness: 10,
                    will: 10,
                    intellect: 10,
                }),
            )
            .expect("insert Attributes");
        assert!(carries_attributes(&broker, e));
    }
}
