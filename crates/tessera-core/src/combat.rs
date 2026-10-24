//! The attack pipeline: damage-type vocabulary, the cached `Combat` part, the
//! wielded-weapon profile, and the fixed-order integer resolution that turns an
//! [`Attack`] into `BeforeHit` -> `Hit`/`Missed`/`Blocked` -> `TakeDamage`
//! -> `Death`.
//!
//! # Boundary
//!
//! Every tunable *input* in the resolution is a value some other owner already
//! computed and cached: `Derived.accuracy`/`Derived.defense` (the rules script's
//! recompute) and the [`Combat`] cache (block chance plus per-type resistance
//! and threshold, also written through the script `set_part` path). This module
//! owns only the arithmetic and its order — it never calls a script on a read
//! path and never invents a policy number. Tuning values arrive as a
//! [`CombatTuning`] snapshot taken at install time from `Tuning_Combat`.
//!
//! # Draw order (the replay contract)
//!
//! One attack draws at most three values from the named `"combat"` stream, in
//! this exact order: (1) the hit d100, (2) the block d100 *only when* the hit
//! landed, the defender's block chance is positive and the damage type is
//! physical, (3) one uniform integer over the weapon's `[lo, hi]` *only when*
//! the hit landed and was not blocked. Misses and blocks therefore consume a
//! fixed prefix and nothing more, so replaying a log or adding a later roll
//! elsewhere cannot shift combat.
//!
//! # Determinism
//!
//! Integer-only (`i64`), floor division (`div_euclid`), `BTreeMap` caches, no
//! `HashMap`/`HashSet`, no floats. The resolving handlers consume randomness
//! only from the context's named stream, never from the parent stream.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::events::{Attack, BeforeHit, Blocked, HandlerAction, Hit, Missed, PART_HEALTH, SimContext, TakeDamage, EARLY, NORMAL};
use crate::rng::SimRng;
use crate::script_part::PartValue;
use crate::stats::{AttributeName, Attributes, Bonuses, PART_ATTRIBUTES, PART_BONUSES};
use crate::events::Health;
use crate::ecs::PartTypeId;
use crate::kinds::TuningValue;

/// `parts.Combat`: the cached combat inputs the rules script writes.
pub const PART_COMBAT: &str = "Combat";
/// `parts.Weapon`: the wielded weapon's resolved profile.
pub const PART_WEAPON: &str = "Weapon";
/// The named RNG stream every combat draw comes from.
pub const COMBAT_STREAM: &str = "combat";
/// The tuning table the combat policy values are read from at install time.
pub const TUNING_TABLE: &str = "Tuning_Combat";

/// The damage-type vocabulary. A closed native enum: a content typo is a load
/// error, never a silently-zero resistance lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DamageType {
    /// The compatibility tag for damage that carries no elemental identity
    /// (the pre-pipeline base hit). Behaves as zero resistance and zero
    /// threshold, and is never blocked.
    Generic,
    Slash,
    Pierce,
    Blunt,
    Fire,
    Cold,
    Lightning,
    Poison,
    Holy,
    Shadow,
}

impl DamageType {
    /// Every damage type, in declaration order.
    pub const ALL: [DamageType; 10] = [
        DamageType::Generic,
        DamageType::Slash,
        DamageType::Pierce,
        DamageType::Blunt,
        DamageType::Fire,
        DamageType::Cold,
        DamageType::Lightning,
        DamageType::Poison,
        DamageType::Holy,
        DamageType::Shadow,
    ];

    /// The canonical lowercase spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            DamageType::Generic => "generic",
            DamageType::Slash => "slash",
            DamageType::Pierce => "pierce",
            DamageType::Blunt => "blunt",
            DamageType::Fire => "fire",
            DamageType::Cold => "cold",
            DamageType::Lightning => "lightning",
            DamageType::Poison => "poison",
            DamageType::Holy => "holy",
            DamageType::Shadow => "shadow",
        }
    }

    /// Parse a canonical damage type (lowercase, exact). Unknown names are
    /// `None`, so callers fail closed instead of guessing.
    ///
    /// Same fixed `from_str` name as the attribute vocabulary; the std trait's
    /// method is deliberately shadowed.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(name: &str) -> Option<DamageType> {
        match name {
            "generic" => Some(DamageType::Generic),
            "slash" => Some(DamageType::Slash),
            "pierce" => Some(DamageType::Pierce),
            "blunt" => Some(DamageType::Blunt),
            "fire" => Some(DamageType::Fire),
            "cold" => Some(DamageType::Cold),
            "lightning" => Some(DamageType::Lightning),
            "poison" => Some(DamageType::Poison),
            "holy" => Some(DamageType::Holy),
            "shadow" => Some(DamageType::Shadow),
            _ => None,
        }
    }

    /// Whether a shield can block this type: the three physical types only.
    /// Magic is never blocked.
    pub fn is_physical(self) -> bool {
        matches!(self, DamageType::Slash | DamageType::Pierce | DamageType::Blunt)
    }
}

impl Serialize for DamageType {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for DamageType {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        DamageType::from_str(&name).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "unknown damage type '{name}' (expected one of: {})",
                DamageType::ALL.map(DamageType::as_str).join(", ")
            ))
        })
    }
}

/// `parts.Combat`: the cached per-entity combat inputs. Never authored content
/// and never saved — the rules script writes it through the validated
/// `set_part` path and the load-path recompute rebuilds it together with
/// `Derived`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Combat {
    /// Block chance in percent; `0` means no shield (or nothing to block with).
    #[serde(default)]
    pub block: i64,
    /// Per-type resistance in percent, summed by the writer.
    #[serde(default)]
    pub resist: BTreeMap<DamageType, i64>,
    /// Per-type flat damage reduction, `>= 0`.
    #[serde(default)]
    pub threshold: BTreeMap<DamageType, i64>,
}

impl Combat {
    /// Resistance against `dtype`, `0` when the writer did not list the type.
    pub fn resist_of(&self, dtype: DamageType) -> i64 {
        self.resist.get(&dtype).copied().unwrap_or(0)
    }

    /// Flat threshold against `dtype`, `0` when the writer did not list it.
    pub fn threshold_of(&self, dtype: DamageType) -> i64 {
        self.threshold.get(&dtype).copied().unwrap_or(0)
    }

    /// Build the payload from a scripted `set_part` field map. The scripted
    /// shape is flat — `block`, `resist_<type>`, `threshold_<type>` — because
    /// the script value boundary carries no nested tables. Non-integer values,
    /// unknown keys and unknown damage types are all rejected.
    pub fn from_fields(fields: BTreeMap<String, PartValue>) -> Result<Combat, String> {
        let mut combat = Combat::default();
        for (key, value) in fields {
            let PartValue::Int(n) = value else {
                return Err(format!("Combat field '{key}' must be an integer, wrote {value:?}"));
            };
            match key.as_str() {
                "block" => combat.block = n,
                _ => {
                    if let Some(rest) = key.strip_prefix("resist_") {
                        let dtype = DamageType::from_str(rest)
                            .ok_or_else(|| format!("unknown damage type '{rest}' in '{key}'"))?;
                        combat.resist.insert(dtype, n);
                    } else if let Some(rest) = key.strip_prefix("threshold_") {
                        let dtype = DamageType::from_str(rest)
                            .ok_or_else(|| format!("unknown damage type '{rest}' in '{key}'"))?;
                        if n < 0 {
                            return Err(format!("threshold for '{rest}' must be >= 0, wrote {n}"));
                        }
                        combat.threshold.insert(dtype, n);
                    } else {
                        return Err(format!("unknown Combat field '{key}'"));
                    }
                }
            }
        }
        if combat.block < 0 {
            return Err(format!("block must be >= 0, wrote {}", combat.block));
        }
        Ok(combat)
    }
}

/// `parts.Weapon`: the wielded weapon's resolved melee profile. Values are the
/// final numbers the resolver uses (a weapon blueprint's own payload lands here
/// when the equipment layer attaches the wielded item).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Weapon {
    /// Lowest damage die face.
    pub lo: i64,
    /// Highest damage die face.
    pub hi: i64,
    /// Damage type this weapon deals.
    pub dtype: DamageType,
    /// The attribute whose modifier is added to damage; `None` adds nothing.
    pub stat: Option<AttributeName>,
    /// Reach in Chebyshev cells; `1` is an adjacent target.
    pub reach: i32,
}

impl Weapon {
    /// The bare-handed profile: one point of generic damage, reach 1, no stat
    /// modifier. This is the compatibility path for actors that carry no
    /// [`Weapon`] part, so the pipeline still runs (and the demo still fights)
    /// where no equipment source exists yet.
    pub const UNARMED: Weapon = Weapon {
        lo: 1,
        hi: 1,
        dtype: DamageType::Generic,
        stat: None,
        reach: 1,
    };
}

/// Wire form of [`Weapon`], so cross-field invariants can be validated on load.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WeaponWire {
    lo: i64,
    hi: i64,
    dtype: DamageType,
    #[serde(default)]
    stat: Option<AttributeName>,
    #[serde(default = "default_reach")]
    reach: i32,
}

fn default_reach() -> i32 {
    1
}

impl<'de> Deserialize<'de> for Weapon {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let w = WeaponWire::deserialize(deserializer)?;
        if w.lo < 0 {
            return Err(serde::de::Error::custom(format!("weapon lo must be >= 0, found {}", w.lo)));
        }
        if w.hi < w.lo {
            return Err(serde::de::Error::custom(format!(
                "weapon hi ({}) must be >= lo ({})",
                w.hi, w.lo
            )));
        }
        if w.reach < 1 {
            return Err(serde::de::Error::custom(format!(
                "weapon reach must be >= 1, found {}",
                w.reach
            )));
        }
        Ok(Weapon {
            lo: w.lo,
            hi: w.hi,
            dtype: w.dtype,
            stat: w.stat,
            reach: w.reach,
        })
    }
}

/// The combat policy numbers, snapshotted from `Tuning_Combat` at install time.
/// Values are authored data (script-owned policy); this struct only carries
/// them to the native resolver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CombatTuning {
    pub base_hit: i64,
    pub hit_min: i64,
    pub hit_max: i64,
    pub crit_threshold: i64,
    pub crit_percent: i64,
    pub res_floor: i64,
    pub res_ceiling: i64,
    pub min_damage: i64,
}

impl Default for CombatTuning {
    /// The base table's values, used when no content library is installed.
    fn default() -> Self {
        CombatTuning {
            base_hit: 50,
            hit_min: 5,
            hit_max: 95,
            crit_threshold: 5,
            crit_percent: 150,
            res_floor: -100,
            res_ceiling: 90,
            min_damage: 1,
        }
    }
}

impl CombatTuning {
    /// Read the snapshot from a merged `Tuning_Combat` value map. Every key is
    /// required and must be an integer; a missing key is an error rather than a
    /// silent default.
    pub fn from_values(values: &BTreeMap<String, TuningValue>) -> Result<CombatTuning, String> {
        let mut required = |key: &str| -> Result<i64, String> {
            match values.get(key) {
                Some(TuningValue::Int(n)) => Ok(*n),
                Some(other) => Err(format!("Tuning_Combat.{key} must be an integer, found {other:?}")),
                None => Err(format!("Tuning_Combat.{key} is missing")),
            }
        };
        let tuning = CombatTuning {
            base_hit: required("base_hit")?,
            hit_min: required("hit_min")?,
            hit_max: required("hit_max")?,
            crit_threshold: required("crit_threshold")?,
            crit_percent: required("crit_percent")?,
            res_floor: required("res_floor")?,
            res_ceiling: required("res_ceiling")?,
            min_damage: required("min_damage")?,
        };
        if tuning.hit_min > tuning.hit_max {
            return Err(format!(
                "Tuning_Combat.hit_min ({}) must be <= hit_max ({})",
                tuning.hit_min, tuning.hit_max
            ));
        }
        Ok(tuning)
    }
}

/// The attacker's cached resolution inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttackInputs {
    /// `Derived.accuracy` (0 when the attacker carries no cache).
    pub accuracy: i64,
    /// The weapon's stat value; the modifier `(value - 10) / 2` is added.
    pub stat_value: i64,
    /// Authored flat damage bonus (`Bonuses.damage_flat`).
    pub flat_damage: i64,
    /// The wielded profile.
    pub weapon: Weapon,
}

/// The defender's cached resolution inputs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DefenseInputs {
    /// `Derived.defense` (0 when the defender carries no cache).
    pub defense: i64,
    /// The defender's [`Combat`] cache.
    pub combat: Combat,
}

/// The outcome of one resolution, after the rolls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    Missed {
        roll: i64,
        hit_pct: i64,
    },
    Blocked {
        roll: i64,
        hit_pct: i64,
        block_pct: i64,
    },
    Hit {
        roll: i64,
        hit_pct: i64,
        crit: bool,
        amount: i64,
    },
}

/// The randomness seam the resolver draws through. Production implements it for
/// [`SimRng`] over the named combat stream; tests implement it with a scripted
/// sequence to pin the *order* of the draws.
pub trait CombatRoller {
    /// A d100: uniform in `1..=100`.
    fn d100(&mut self) -> i64;
    /// A uniform integer in `lo..=hi` (`hi >= lo`).
    fn uniform(&mut self, lo: i64, hi: i64) -> i64;
}

impl CombatRoller for SimRng {
    fn d100(&mut self) -> i64 {
        i64::from(self.below(100)) + 1
    }

    fn uniform(&mut self, lo: i64, hi: i64) -> i64 {
        let span = (hi - lo + 1).max(1) as u32;
        lo + i64::from(self.below(span))
    }
}

/// The attribute modifier: `(value - 10) / 2`, floored (so a below-average
/// attribute gives a negative modifier rather than truncating to zero).
pub fn attribute_modifier(value: i64) -> i64 {
    (value - 10).div_euclid(2)
}

/// The clamped chance to hit, from the two cached stat values.
pub fn hit_chance(tuning: &CombatTuning, accuracy: i64, defense: i64) -> i64 {
    (tuning.base_hit + accuracy - defense).clamp(tuning.hit_min, tuning.hit_max)
}

/// The full damage chain for one landed hit, in the fixed order: weapon roll,
/// stat modifier, flat bonus, crit multiplier, resistance, threshold, floor.
pub fn damage_amount(
    tuning: &CombatTuning,
    base_roll: i64,
    stat_value: i64,
    flat_damage: i64,
    crit: bool,
    resist: i64,
    threshold: i64,
) -> i64 {
    let mut damage = base_roll + attribute_modifier(stat_value) + flat_damage;
    if crit {
        damage = damage * tuning.crit_percent / 100;
    }
    // Immunity is its own case and is checked *before* the range clamp: a
    // resistance of exactly 100 zeroes the hit and skips threshold and floor.
    if resist == 100 {
        return 0;
    }
    let resist = resist.clamp(tuning.res_floor, tuning.res_ceiling);
    damage = damage * (100 - resist).div_euclid(100) - threshold;
    damage.max(tuning.min_damage)
}

/// Resolve one attack: the hit roll, the conditional block roll, the
/// conditional damage roll, and the damage chain. Draw order is the replay
/// contract documented at the module level.
pub fn resolve_attack<R: CombatRoller>(
    tuning: &CombatTuning,
    attacker: &AttackInputs,
    defender: &DefenseInputs,
    rolls: &mut R,
) -> Resolution {
    let hit_pct = hit_chance(tuning, attacker.accuracy, defender.defense);
    resolve_attack_at(tuning, hit_pct, attacker, defender, rolls)
}

/// [`resolve_attack`] with the chance to hit already settled (a `BeforeHit`
/// handler may have adjusted it), so the resolver never re-derives it.
pub fn resolve_attack_at<R: CombatRoller>(
    tuning: &CombatTuning,
    hit_pct: i64,
    attacker: &AttackInputs,
    defender: &DefenseInputs,
    rolls: &mut R,
) -> Resolution {
    let roll = rolls.d100();
    if roll > hit_pct {
        return Resolution::Missed { roll, hit_pct };
    }
    let dtype = attacker.weapon.dtype;
    let block_pct = defender.combat.block;
    if block_pct > 0 && dtype.is_physical() {
        let block_roll = rolls.d100();
        if block_roll <= block_pct {
            return Resolution::Blocked {
                roll,
                hit_pct,
                block_pct,
            };
        }
    }
    let crit = roll <= tuning.crit_threshold.min(hit_pct);
    let base = rolls.uniform(attacker.weapon.lo, attacker.weapon.hi);
    let amount = damage_amount(
        tuning,
        base,
        attacker.stat_value,
        attacker.flat_damage,
        crit,
        defender.combat.resist_of(dtype),
        defender.combat.threshold_of(dtype),
    );
    Resolution::Hit {
        roll,
        hit_pct,
        crit,
        amount,
    }
}

// ---------------------------------------------------------------------------
// The installed pipeline
// ---------------------------------------------------------------------------

/// Register the combat parts (`Combat`, `Weapon`) as `Dynamic` parts with a
/// serde factory, so `parts.Combat` / `parts.Weapon` are known part names and
/// `part_typed` deserializes their payloads into the types above.
pub fn register_all(registry: &mut crate::ecs::PartRegistry) -> Result<(), crate::ecs::PartError> {
    crate::kinds::register::<Combat>(registry, PART_COMBAT)?;
    crate::kinds::register::<Weapon>(registry, PART_WEAPON)?;
    Ok(())
}

/// Read a typed part, or `None`.
fn part<T: crate::ecs::Part + Clone>(ctx: &SimContext, entity: hecs::Entity, name: &str) -> Option<T> {
    ctx.broker
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .typed_part::<T>(entity, &PartTypeId::new(name))
        .cloned()
}

fn is_alive(ctx: &SimContext, entity: hecs::Entity) -> bool {
    ctx.broker
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .is_alive(entity)
}

/// The defender's Chebyshev distance from the attacker, or `None` when either
/// actor carries no `Position`.
fn chebyshev(ctx: &SimContext, a: hecs::Entity, b: hecs::Entity) -> Option<i32> {
    let pa = ctx.position(a)?;
    let pb = ctx.position(b)?;
    Some((pa.x - pb.x).abs().max((pa.y - pb.y).abs()))
}

/// Resolve the attacker's wielded profile: its [`Weapon`] part, or
/// [`Weapon::UNARMED`] when it carries none.
pub fn wielded_weapon(ctx: &SimContext, attacker: hecs::Entity) -> Weapon {
    part::<Weapon>(ctx, attacker, PART_WEAPON).unwrap_or(Weapon::UNARMED)
}

/// Install the attack pipeline on `ctx`.
///
/// Two handlers, in the bus's fixed `(priority, seq)` order:
///
/// 1. `EARLY` legality: the attacker must be alive and the defender must carry
///    a `Health` part with `hp > 0`; when both actors have a position, the
///    defender must be inside the weapon's reach. Any failure cancels the
///    attack before a single event fires or a single draw is taken.
/// 2. `NORMAL` resolution: compute the cached hit chance, dispatch `BeforeHit`
///    (a cancel there vetoes the attack), resolve the rolls on the named
///    `"combat"` stream, fire exactly one of `Missed`/`Blocked`/`Hit`, and on a
///    hit dispatch `TakeDamage` with the weapon's damage type.
///
/// The `TakeDamage` applicator (health decrement, the single `Death` emit, the
/// `hp` floor at 0) stays where it is: this module never writes health.
pub fn install_combat(ctx: &mut SimContext, tuning: CombatTuning) {
    ctx.register::<Attack>(EARLY, |ctx, ev| {
        if !is_alive(ctx, ev.attacker) {
            return HandlerAction::Cancel;
        }
        let Some(health) = part::<Health>(ctx, ev.defender, PART_HEALTH) else {
            return HandlerAction::Cancel;
        };
        if health.hp <= 0 {
            return HandlerAction::Cancel;
        }
        let reach = wielded_weapon(ctx, ev.attacker).reach;
        if let Some(distance) = chebyshev(ctx, ev.attacker, ev.defender)
            && distance > reach
        {
            return HandlerAction::Cancel;
        }
        HandlerAction::Continue
    });

    ctx.register::<Attack>(NORMAL, move |ctx, ev| {
        let weapon = wielded_weapon(ctx, ev.attacker);
        let stat_value = match (weapon.stat, part::<Attributes>(ctx, ev.attacker, PART_ATTRIBUTES)) {
            (Some(AttributeName::Might), Some(a)) => a.might,
            (Some(AttributeName::Agility), Some(a)) => a.agility,
            (Some(AttributeName::Toughness), Some(a)) => a.toughness,
            (Some(AttributeName::Will), Some(a)) => a.will,
            (Some(AttributeName::Intellect), Some(a)) => a.intellect,
            _ => 0,
        };
        let attacker = AttackInputs {
            accuracy: part::<crate::effects::Derived>(ctx, ev.attacker, crate::effects::PART_DERIVED)
                .and_then(|d| d.values.get("accuracy").copied())
                .unwrap_or(0),
            stat_value,
            flat_damage: part::<Bonuses>(ctx, ev.attacker, PART_BONUSES)
                .map(|b| b.damage_flat)
                .unwrap_or(0),
            weapon: weapon.clone(),
        };
        let defender = DefenseInputs {
            defense: part::<crate::effects::Derived>(ctx, ev.defender, crate::effects::PART_DERIVED)
                .and_then(|d| d.values.get("defense").copied())
                .unwrap_or(0),
            combat: part::<Combat>(ctx, ev.defender, PART_COMBAT).unwrap_or_default(),
        };
        let mut hit_pct = hit_chance(&tuning, attacker.accuracy, defender.defense);

        // BeforeHit: a mutator-phase hook (dodge abilities later) may veto or
        // adjust the chance before any roll is taken.
        match ctx.dispatch(BeforeHit {
            attacker: ev.attacker,
            defender: ev.defender,
            hit_pct,
        }) {
            Ok(crate::events::EventOutcome::Cancelled { .. }) => return HandlerAction::Cancel,
            Ok(crate::events::EventOutcome::Modified(ev)) => hit_pct = ev.hit_pct,
            Ok(crate::events::EventOutcome::Handled) => {}
            Err(_) => return HandlerAction::Cancel,
        }

        let inputs = attacker;
        let stream = ctx.stream(COMBAT_STREAM);
        let outcome = resolve_attack_at(&tuning, hit_pct, &inputs, &defender, stream);
        drop(stream);

        match outcome {
            Resolution::Missed { roll, hit_pct } => {
                let _ = ctx.dispatch(Missed {
                    attacker: ev.attacker,
                    defender: ev.defender,
                    roll,
                    hit_pct,
                });
            }
            Resolution::Blocked {
                roll,
                hit_pct,
                block_pct,
            } => {
                let _ = ctx.dispatch(Blocked {
                    attacker: ev.attacker,
                    defender: ev.defender,
                    roll,
                    hit_pct,
                    block_pct,
                });
            }
            Resolution::Hit {
                roll,
                hit_pct,
                crit,
                amount,
            } => {
                let _ = ctx.dispatch(Hit {
                    attacker: ev.attacker,
                    defender: ev.defender,
                    roll,
                    hit_pct,
                    crit,
                });
                let _ = ctx.dispatch(TakeDamage {
                    target: ev.defender,
                    amount: amount.clamp(0, i64::from(i32::MAX)) as i32,
                    source: crate::events::DamageSource::Attack,
                    dtype: weapon.dtype.as_str().to_string(),
                });
            }
        }
        HandlerAction::Continue
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A roller that records every request, so draw *order* is assertable.
    struct Scripted {
        d100: Vec<i64>,
        uniform: Vec<i64>,
        calls: Vec<String>,
    }

    impl Scripted {
        fn new(d100: &[i64], uniform: &[i64]) -> Self {
            Self {
                d100: d100.iter().rev().copied().collect(),
                uniform: uniform.iter().rev().copied().collect(),
                calls: Vec::new(),
            }
        }
    }

    impl CombatRoller for Scripted {
        fn d100(&mut self) -> i64 {
            self.calls.push("d100".into());
            self.d100.pop().expect("scripted d100 exhausted")
        }

        fn uniform(&mut self, lo: i64, hi: i64) -> i64 {
            self.calls.push(format!("uniform({lo},{hi})"));
            self.uniform.pop().expect("scripted uniform exhausted")
        }
    }

    fn tuning() -> CombatTuning {
        CombatTuning::default()
    }

    fn parse<T: serde::de::DeserializeOwned>(toml_text: &str) -> Result<T, toml::de::Error> {
        let anchored = format!("payload = {toml_text}");
        let value: toml::Value = toml::from_str(&anchored)?;
        value.get("payload").expect("anchored").clone().try_into()
    }

    #[test]
    fn damage_type_round_trips_and_rejects_unknown() {
        for dt in DamageType::ALL {
            assert_eq!(DamageType::from_str(dt.as_str()), Some(dt));
        }
        assert_eq!(DamageType::from_str("acid"), None);
        assert_eq!(DamageType::from_str("Slash"), None);
        assert!(DamageType::Slash.is_physical());
        assert!(!DamageType::Fire.is_physical());
        assert!(!DamageType::Generic.is_physical());
        let err = parse::<DamageType>("\"acid\"").unwrap_err();
        assert!(err.to_string().contains("acid"), "{err}");
        let dt: DamageType = parse("\"pierce\"").unwrap();
        assert_eq!(dt, DamageType::Pierce);
    }

    #[test]
    fn combat_part_validates_its_payload() {
        let c: Combat = parse("{ block = 19, resist = { slash = 30 }, threshold = { blunt = 2 } }").unwrap();
        assert_eq!(c.block, 19);
        assert_eq!(c.resist_of(DamageType::Slash), 30);
        assert_eq!(c.resist_of(DamageType::Pierce), 0);
        assert_eq!(c.threshold_of(DamageType::Blunt), 2);
        // Unknown keys fail closed, including unknown damage types.
        let err = parse::<Combat>("{ block = 1, shield = 2 }").unwrap_err();
        assert!(err.to_string().contains("shield"), "{err}");
        let err = parse::<Combat>("{ resist = { acid = 10 } }").unwrap_err();
        assert!(err.to_string().contains("acid"), "{err}");
    }

    #[test]
    fn combat_from_fields_accepts_the_flat_script_shape() {
        let fields = BTreeMap::from([
            ("block".to_string(), PartValue::Int(19)),
            ("resist_slash".to_string(), PartValue::Int(30)),
            ("threshold_pierce".to_string(), PartValue::Int(0)),
        ]);
        let c = Combat::from_fields(fields).unwrap();
        assert_eq!(c.block, 19);
        assert_eq!(c.resist_of(DamageType::Slash), 30);
        // Non-integer field.
        let err = Combat::from_fields(BTreeMap::from([(
            "block".to_string(),
            PartValue::Str("high".into()),
        )]))
        .unwrap_err();
        assert!(err.contains("integer"), "{err}");
        // Unknown damage type and unknown key.
        let err = Combat::from_fields(BTreeMap::from([(
            "resist_acid".to_string(),
            PartValue::Int(1),
        )]))
        .unwrap_err();
        assert!(err.contains("acid"), "{err}");
        let err = Combat::from_fields(BTreeMap::from([(
            "dodge".to_string(),
            PartValue::Int(1),
        )]))
        .unwrap_err();
        assert!(err.contains("dodge"), "{err}");
    }

    #[test]
    fn weapon_rejects_broken_ranges_and_types() {
        let w: Weapon = parse("{ lo = 4, hi = 8, dtype = \"slash\", stat = \"might\" }").unwrap();
        assert_eq!(w.lo, 4);
        assert_eq!(w.hi, 8);
        assert_eq!(w.stat, Some(AttributeName::Might));
        assert_eq!(w.reach, 1);
        for bad in [
            "{ lo = 4, hi = 2, dtype = \"slash\" }",
            "{ lo = -1, hi = 2, dtype = \"slash\" }",
            "{ lo = 1, hi = 2, dtype = \"acid\" }",
            "{ lo = 1, hi = 2, dtype = \"slash\", stat = \"luck\" }",
            "{ lo = 1, hi = 2, dtype = \"slash\", reach = 0 }",
            "{ lo = 1, hi = 2, dtype = \"slash\", speed = 3 }",
        ] {
            assert!(parse::<Weapon>(bad).is_err(), "{bad} accepted");
        }
    }

    #[test]
    fn hit_chance_matches_the_worked_examples() {
        let t = tuning();
        // Knight (accuracy 4) against the goblin (defense 12).
        assert_eq!(hit_chance(&t, 4, 12), 42);
        // Goblin (8) against the Knight (defense 25).
        assert_eq!(hit_chance(&t, 8, 25), 33);
        // Goblin (8) against the wizard (defense 5).
        assert_eq!(hit_chance(&t, 8, 5), 53);
        // Rogue (16) against the goblin (12).
        assert_eq!(hit_chance(&t, 16, 12), 54);
        // Clamped at both ends.
        assert_eq!(hit_chance(&t, 200, 0), 95);
        assert_eq!(hit_chance(&t, 0, 200), 5);
    }

    #[test]
    fn damage_chain_matches_the_worked_examples() {
        let t = tuning();
        // Knight: longsword 4..8, Might 14 (mod 2), flat +1 -> 7..11.
        let knight: Vec<i64> = (4..=8).map(|b| damage_amount(&t, b, 14, 1, false, 0, 0)).collect();
        assert_eq!(knight, vec![7, 8, 9, 10, 11]);
        // Crit: 7*150//100 = 10 ... 11*150//100 = 16.
        let crits: Vec<i64> = (4..=8).map(|b| damage_amount(&t, b, 14, 1, true, 0, 0)).collect();
        assert_eq!(crits, vec![10, 12, 13, 15, 16]);
        // Goblin short blade 3..6 against mail (slash 30): 3->2, 4->2, 5->3, 6->4.
        let mailed: Vec<i64> = (3..=6).map(|b| damage_amount(&t, b, 13, 0, false, 30, 0)).collect();
        assert_eq!(mailed, vec![2, 2, 3, 4]);
        // Threshold subtracts after resistance; the floor then applies.
        assert_eq!(damage_amount(&t, 5, 13, 0, false, 30, 3), 1);
        // Resistance 100 is immunity: no threshold, no floor.
        assert_eq!(damage_amount(&t, 9, 14, 1, false, 100, 5), 0);
        // Resistance is clamped into the tuning range.
        assert_eq!(damage_amount(&t, 10, 10, 0, false, 95, 0), 1);
        assert_eq!(damage_amount(&t, 10, 10, 0, false, -100, 0), 20);
    }

    #[test]
    fn attribute_modifier_floors_for_below_average_values() {
        assert_eq!(attribute_modifier(10), 0);
        assert_eq!(attribute_modifier(14), 2);
        assert_eq!(attribute_modifier(9), -1);
        assert_eq!(attribute_modifier(1), -5);
    }

    #[test]
    fn draw_order_is_hit_then_block_then_damage() {
        let t = tuning();
        let attacker = AttackInputs {
            accuracy: 16,
            stat_value: 15,
            flat_damage: 0,
            weapon: Weapon {
                lo: 3,
                hi: 6,
                dtype: DamageType::Slash,
                stat: Some(AttributeName::Agility),
                reach: 1,
            },
        };
        let mut defender = DefenseInputs {
            defense: 12,
            combat: Combat::default(),
        };
        defender.combat.block = 19;

        // A miss takes exactly one draw and never rolls damage.
        let mut rolls = Scripted::new(&[80], &[]);
        let outcome = resolve_attack(&t, &attacker, &defender, &mut rolls);
        assert!(matches!(outcome, Resolution::Missed { roll: 80, hit_pct: 54 }));
        assert_eq!(rolls.calls, vec!["d100"]);

        // A blocked hit takes two draws and never rolls damage.
        let mut rolls = Scripted::new(&[10, 5], &[]);
        let outcome = resolve_attack(&t, &attacker, &defender, &mut rolls);
        assert!(matches!(
            outcome,
            Resolution::Blocked {
                roll: 10,
                hit_pct: 54,
                block_pct: 19
            }
        ));
        assert_eq!(rolls.calls, vec!["d100", "d100"]);

        // A landed hit takes the damage draw over the weapon range.
        let mut rolls = Scripted::new(&[10, 90], &[5]);
        let outcome = resolve_attack(&t, &attacker, &defender, &mut rolls);
        assert!(matches!(outcome, Resolution::Hit { amount: 7, .. }));
        assert_eq!(rolls.calls, vec!["d100", "d100", "uniform(3,6)"]);

        // A magic hit is never blocked: no block draw at all.
        let mut magic = attacker.clone();
        magic.weapon.dtype = DamageType::Fire;
        let mut rolls = Scripted::new(&[10], &[5]);
        let outcome = resolve_attack(&t, &magic, &defender, &mut rolls);
        assert!(matches!(outcome, Resolution::Hit { amount: 7, .. }));
        assert_eq!(rolls.calls, vec!["d100", "uniform(3,6)"]);

        // No shield (block 0) means no block draw either.
        defender.combat.block = 0;
        let mut rolls = Scripted::new(&[10], &[5]);
        let _ = resolve_attack(&t, &attacker, &defender, &mut rolls);
        assert_eq!(rolls.calls, vec!["d100", "uniform(3,6)"]);
    }

    #[test]
    fn crit_uses_the_best_five_percent_of_the_hit_range() {
        let t = tuning();
        let attacker = AttackInputs {
            accuracy: 4,
            stat_value: 14,
            flat_damage: 1,
            weapon: Weapon {
                lo: 4,
                hi: 8,
                dtype: DamageType::Slash,
                stat: Some(AttributeName::Might),
                reach: 1,
            },
        };
        let defender = DefenseInputs {
            defense: 12,
            ..Default::default()
        };
        let mut rolls = Scripted::new(&[5], &[4]);
        let outcome = resolve_attack(&t, &attacker, &defender, &mut rolls);
        assert!(matches!(
            outcome,
            Resolution::Hit {
                crit: true,
                amount: 10,
                ..
            }
        ));
        let mut rolls = Scripted::new(&[6], &[4]);
        let outcome = resolve_attack(&t, &attacker, &defender, &mut rolls);
        assert!(matches!(outcome, Resolution::Hit { crit: false, .. }));
    }

    #[test]
    fn tuning_snapshot_requires_every_key() {
        let mut values = BTreeMap::new();
        values.insert("base_hit".to_string(), TuningValue::Int(50));
        let err = CombatTuning::from_values(&values).unwrap_err();
        assert!(err.contains("hit_min"), "{err}");
        for (key, value) in [
            ("hit_min", 5),
            ("hit_max", 95),
            ("crit_threshold", 5),
            ("crit_percent", 150),
            ("res_floor", -100),
            ("res_ceiling", 90),
            ("min_damage", 1),
        ] {
            values.insert(key.to_string(), TuningValue::Int(value));
        }
        let t = CombatTuning::from_values(&values).unwrap();
        assert_eq!(t, CombatTuning::default());
    }
}
