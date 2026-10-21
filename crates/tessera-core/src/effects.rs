//! S0 native effect runtime: the declarative effect vocabulary, the native
//! status store, the `Derived` cache invalidation hook, and the `UseAbility`
//! resource path.
//!
//! Ownership split (see the native/script boundary in `docs/architecture.md`):
//! native owns entity
//! identity, resource accounting (charges decrement on use), status
//! application/duration bookkeeping, derived-cache storage and invalidation
//! hooks, and event dispatch invariants. Luau owns rules/policy and
//! exceptional effects, and may read/write only through the whitelisted
//! script surface. Everything here is native and deterministic; declarative
//! effect data stays data (TOML parts, registered like every other part).
//!
//! The seam surface the later Shield Bash headless path consumes:
//!
//! - parts: `Charges`, `Damage`, `Effect`, `Modifiers`, `Abilities`,
//!   `Derived`, `StatusEffects` (registered via [`register_all`]);
//! - events: [`UseAbility`](crate::events::UseAbility),
//!   [`ApplyStatus`](crate::events::ApplyStatus),
//!   [`StatusApplied`](crate::events::StatusApplied),
//!   [`StatusExpired`](crate::events::StatusExpired);
//! - handlers: [`install_native_effects`] registers the mutators/observers
//!   that resolve these events against the content library;
//! - helpers: [`grant_ability_resources`] (blueprint → entity charge pool),
//!   [`elapse_statuses`] (duration tick → `StatusExpired`), and
//!   [`invalidate_derived`] (the declared cache-invalidation hook).
//!
//! No S2 hit-resolution rules are decided here: `Damage` rolls `lo..=hi` on a
//! forked RNG stream and dispatches the existing `TakeDamage` event with the
//! dtype field named; the resolution pipeline is later work.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use hecs::Entity;
use serde::Deserialize;
use thiserror::Error;

use crate::blueprints::BlueprintLibrary;
use crate::ecs::{EntityBroker, PartError, PartRegistry, PartTypeId};
use crate::events::{
    ApplyStatus, DamageSource, HandlerAction, Move, NORMAL, Point, SimContext, StatusApplied,
    StatusExpired, TakeDamage, TileEntry, UseAbility,
};
use crate::kinds::Stacking;

// ---------------------------------------------------------------------------
// Part names
// ---------------------------------------------------------------------------

/// `parts.Abilities`: the S0 ownership stub — the ability ids an entity is
/// granted (`list = ["Ability_ShieldBash"]`). The grant seam
/// ([`grant_ability_resources`]) resolves each id's `parts.Charges` into an
/// entity charge pool.
pub const PART_ABILITIES: &str = "Abilities";
/// `parts.Charges`: `{ max, recharge }` declared on an ability blueprint, or
/// the runtime pool `{ max, recharge, charges }` stamped on its owner.
pub const PART_CHARGES: &str = "Charges";
/// `parts.Damage`: `{ lo, hi, dtype, stat, acc_bonus }` — a declarative hit.
pub const PART_DAMAGE: &str = "Damage";
/// `parts.Effect`: `{ applies = [...], push = N, script = "" }` — declarative
/// consequences of an ability use.
pub const PART_EFFECT: &str = "Effect";
/// `parts.Modifiers`: the additive/percent modifier list of a status.
pub const PART_MODIFIERS: &str = "Modifiers";
/// `parts.Derived`: the rebuild-only derived-stat cache (never computed by
/// native code; invalidation clears it, rules scripts rewrite it).
pub const PART_DERIVED: &str = "Derived";
/// The per-entity applied-status store (runtime state, not content).
pub const PART_STATUS_EFFECTS: &str = "StatusEffects";

/// Independent stacking is capped at a fixed 8 instances per status id,
/// first-expired-first-out (a deterministic cap, not an open knob).
pub const INDEPENDENT_CAP: usize = 8;

/// Fork label for the declarative `Damage` roll stream (the native
/// counterpart of the script-visible `engine.rng("combat")` stream; both
/// share the determinism rule — rolls are drawn from a forked stream, never
/// the parent).
pub const COMBAT_STREAM_LABEL: u64 = 1;

// ---------------------------------------------------------------------------
// Declarative effect parts
// ---------------------------------------------------------------------------

/// `parts.Charges`: an ability's charge pool. Blueprints author
/// `{ max = 3, recharge = 1200 }` (game-time units per charge); the native
/// grant seam stamps the entity instance with `charges = max`, and the
/// `UseAbility` resource path decrements it on use. Charge *recharge* accrual
/// (the timer) is later work — S0 owns the declaration and the decrement.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Charges {
    pub max: i64,
    pub recharge: i64,
    /// Current charges; omitted in authored blueprints (defaults to 0 there —
    /// the blueprint is a declaration, and the grant seam stamps the entity
    /// instance full).
    #[serde(default)]
    pub charges: i64,
}

impl Charges {
    /// A full runtime pool stamped from a blueprint declaration.
    pub fn at_full(max: i64, recharge: i64) -> Self {
        Self {
            max,
            recharge,
            charges: max,
        }
    }

    pub fn spend(&mut self) -> bool {
        if self.charges <= 0 {
            return false;
        }
        self.charges -= 1;
        true
    }
}

/// `parts.Damage`: a declarative hit `lo..=hi` of `dtype`, scaled by `stat`,
/// with a to-hit bonus. The roll consumes a forked RNG stream and is
/// dispatched as the existing `TakeDamage` event; accuracy/armor resolution
/// is later work.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Damage {
    pub lo: i64,
    pub hi: i64,
    /// Damage type tag carried through `TakeDamage` (`"blunt"`, ...).
    pub dtype: String,
    /// The stat the hit scales with (reserved for later resolution).
    pub stat: String,
    #[serde(default)]
    pub acc_bonus: i64,
}

/// `parts.Effect`: declarative consequences — status ids to apply (in
/// declared order), a push distance in tiles, and an optional script name for
/// exceptional effects (`""` = none; scripts *add* behavior, they never
/// replace the declarative parts).
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Effect {
    #[serde(default)]
    pub applies: Vec<String>,
    #[serde(default)]
    pub push: i64,
    #[serde(default)]
    pub script: String,
}

/// `parts.Abilities`: the S0 ownership stub — granted ability ids in declared
/// order. `UseAbility` spends the pool stamped from the matching ability
/// blueprint; multiple granted abilities key off the same pool name for now
/// (one charge pool per entity in S0; multi-pool ownership is later work).
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Abilities {
    #[serde(default)]
    pub list: Vec<String>,
}

/// `parts.Derived`: the rebuild-only derived-stat cache. Native code never
/// computes values here — it stores whatever the rules script wrote through
/// the whitelisted `set_part` path and clears the part on invalidation (see
/// [`invalidate_derived`]). Reads (native or script) never recompute.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Derived {
    pub values: BTreeMap<String, i64>,
}

// ---------------------------------------------------------------------------
// Modifiers
// ---------------------------------------------------------------------------

/// One resolver entry: an additive and/or percent step on a named stat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AddPct {
    pub add: i64,
    /// The full percent-of-value (150 = ×150%) — never a delta.
    pub pct: i64,
}

/// `parts.Modifiers`: the additive/percent modifier list (decision: two
/// operators, no expression language). Payload shapes:
///
/// ```toml
/// parts.Modifiers = { skip_turns = 1 }       # stat = <int>  →  add
/// parts.Modifiers = { speed_pct = 150 }      # stat_pct = <int>  →  pct
/// parts.Modifiers = { speed = { add = 10, pct = 150 } }
/// ```
///
/// Application order is fixed and total: every `add` first (in sorted-key
/// order), then every `pct` (integer math, truncation toward zero) — there
/// is no reordering rule to learn. `skip_turns` is a control field, not a
/// stat: it is carried and surfaced separately ([`Self::skip_turns`]) and
/// never applied to a stat base.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Modifiers {
    stats: BTreeMap<String, AddPct>,
}

/// The reserved control key inside a `Modifiers` payload.
pub const CONTROL_SKIP_TURNS: &str = "skip_turns";

impl Modifiers {
    /// Apply the fixed additive-then-percent order to `base`, integer math,
    /// truncation toward zero. Control fields (`skip_turns`) are excluded.
    pub fn apply(&self, base: i64) -> i64 {
        let mut v = base;
        for (stat, m) in &self.stats {
            if stat != CONTROL_SKIP_TURNS {
                v += m.add;
            }
        }
        for (stat, m) in &self.stats {
            if stat != CONTROL_SKIP_TURNS && m.pct != 0 {
                v = v.saturating_mul(m.pct) / 100;
            }
        }
        v
    }

    /// The resolved skip-turn count (the control field's `add`).
    pub fn skip_turns(&self) -> i64 {
        self.stats.get(CONTROL_SKIP_TURNS).map_or(0, |m| m.add)
    }

    /// Consume one skip-turn credit: decrement the control field's `add` by
    /// one (never below zero) and report whether a credit was available.
    /// This is the native decrement behind the scheduler's stagger seam —
    /// each consumed credit wastes one scheduled action.
    pub fn consume_skip_turn(&mut self) -> bool {
        let Some(m) = self.stats.get_mut(CONTROL_SKIP_TURNS) else {
            return false;
        };
        if m.add <= 0 {
            return false;
        }
        m.add -= 1;
        true
    }

    /// Sorted stat entries — deterministic iteration for aggregation.
    pub fn entries(&self) -> impl Iterator<Item = (&String, AddPct)> + '_ {
        self.stats.iter().map(|(k, v)| (k, *v))
    }
}

/// Raw per-key modifier value: `stat = 5` or `stat = { add, pct }`.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawModValue {
    Int(i64),
    Table(RawModTable),
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawModTable {
    add: i64,
    pct: i64,
}

impl<'de> Deserialize<'de> for Modifiers {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;
        let raw: BTreeMap<String, RawModValue> = BTreeMap::deserialize(deserializer)?;
        let mut stats: BTreeMap<String, AddPct> = BTreeMap::new();
        for (key, value) in raw {
            if let Some(stat) = key.strip_suffix("_pct") {
                // `stat_pct = <int>` means pct; tables under `_pct` keys are
                // a payload contradiction and fail closed.
                let pct = match value {
                    RawModValue::Int(v) => v,
                    RawModValue::Table(_) => {
                        return Err(D::Error::custom(format!(
                            "modifier key '{key}' (a `_pct` shorthand) must be an integer"
                        )));
                    }
                };
                stats.entry(stat.to_string()).or_default().pct = pct;
            } else {
                let (add, pct) = match value {
                    RawModValue::Int(v) => (v, 0),
                    RawModValue::Table(t) => (t.add, t.pct),
                };
                let entry = stats.entry(key).or_default();
                entry.add += add;
                entry.pct += pct;
            }
        }
        Ok(Modifiers { stats })
    }
}

// ---------------------------------------------------------------------------
// Status store
// ---------------------------------------------------------------------------

/// One applied status instance, snapshotted at application time (the hot
/// reload rule: in-flight instances keep their applied duration/tags/
/// modifiers; only *new* applications read the reloaded definition).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusInstance {
    /// Monotonic per-entity application sequence (deterministic tie-break and
    /// instance identity for the independent cap).
    pub seq: u64,
    pub duration_remaining: i64,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub modifiers: Modifiers,
}

/// What one [`StatusEffects::apply`] call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// A new instance was created.
    Applied,
    /// An existing instance's duration was reset to the declared duration
    /// (refresh policy).
    Refreshed,
    /// An existing instance's duration was extended by the declared duration
    /// (stack policy).
    Stacked,
    /// A separate instance was created; `evicted` names the sequence of the
    /// instance dropped by the independent cap (smallest time-to-expiry,
    /// tie-break by application order), when the cap was reached.
    Independent { evicted: Option<u64> },
}

/// The per-entity native status store: status blueprint id → applied
/// instances in application order. Deterministic by construction — every
/// container iterates sorted-key / application order, never a hash map.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StatusEffects {
    applied: BTreeMap<String, Vec<StatusInstance>>,
    next_seq: u64,
}

impl StatusEffects {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one status application under its stacking policy:
    ///
    /// - `refresh`: reapplication resets the duration of the existing
    ///   instance (first in application order);
    /// - `stack`: reapplication adds the declared duration to it;
    /// - `independent`: a separate instance is created, capped at
    ///   [`INDEPENDENT_CAP`] with first-expired-first-out eviction.
    pub fn apply(
        &mut self,
        status: &str,
        duration: i64,
        stacking: Stacking,
        tags: &[String],
        modifiers: Modifiers,
    ) -> ApplyOutcome {
        let entry = self.applied.entry(status.to_string()).or_default();
        match stacking {
            Stacking::Refresh => {
                if let Some(first) = entry.first_mut() {
                    first.duration_remaining = duration;
                    return ApplyOutcome::Refreshed;
                }
            }
            Stacking::Stack => {
                if let Some(first) = entry.first_mut() {
                    first.duration_remaining = first.duration_remaining.saturating_add(duration);
                    return ApplyOutcome::Stacked;
                }
            }
            Stacking::Independent => {}
        }

        let seq = self.next_seq;
        self.next_seq += 1;
        entry.push(StatusInstance {
            seq,
            duration_remaining: duration,
            tags: tags.to_vec(),
            modifiers,
        });

        if stacking == Stacking::Independent && entry.len() > INDEPENDENT_CAP {
            // First-expired-first-out: evict the instance with the smallest
            // time-to-expiry; ties break by application order (lowest seq).
            let victim = entry
                .iter()
                .enumerate()
                .min_by_key(|(_, i)| (i.duration_remaining, i.seq))
                .map(|(idx, _)| idx)
                .expect("entry is non-empty at cap");
            let evicted_seq = entry.remove(victim).seq;
            return ApplyOutcome::Independent {
                evicted: Some(evicted_seq),
            };
        }
        match stacking {
            Stacking::Independent => ApplyOutcome::Independent { evicted: None },
            _ => ApplyOutcome::Applied,
        }
    }

    pub fn has(&self, status: &str) -> bool {
        self.applied.get(status).is_some_and(|v| !v.is_empty())
    }

    /// Instances of one status, in application order.
    pub fn instances(&self, status: &str) -> &[StatusInstance] {
        self.applied.get(status).map_or(&[], Vec::as_slice)
    }

    /// All live status ids, sorted.
    pub fn statuses(&self) -> impl Iterator<Item = &String> + '_ {
        self.applied.keys()
    }

    /// The total skip-turn control value across this entity's instances
    /// (sum over instances; the scheduler consumes one credit per skipped
    /// action via [`Self::consume_skip_turn`]).
    pub fn skip_turns(&self, status: &str) -> i64 {
        self.instances(status)
            .iter()
            .map(|i| i.modifiers.skip_turns())
            .sum()
    }

    /// Consume one skip-turn credit from the first instance (in
    /// deterministic order: status id sorted, then application order) that
    /// still owes one. Returns whether a credit was consumed — this is the
    /// scheduler seam through which a staggered actor wastes its next
    /// scheduled action.
    pub fn consume_skip_turn(&mut self) -> bool {
        for instances in self.applied.values_mut() {
            for inst in instances {
                if inst.modifiers.consume_skip_turn() {
                    return true;
                }
            }
        }
        false
    }

    /// The script-visible snapshot of the store: status id → total
    /// skip-turn credit owed (sorted). Whitelisted script reads use this to
    /// assert presence and the stagger effect without seeing internal
    /// instance bookkeeping (duration counts are native-only).
    pub fn skip_turn_map(&self) -> BTreeMap<String, i64> {
        self.applied
            .iter()
            .map(|(status, instances)| {
                (
                    status.clone(),
                    instances.iter().map(|i| i.modifiers.skip_turns()).sum(),
                )
            })
            .collect()
    }

    /// Decrement every instance by `dt` game-time units; remove and return
    /// `(status id, seq)` for each expiration, in deterministic order
    /// (status id sorted, then application order).
    pub fn elapse(&mut self, dt: i64) -> Vec<(String, u64)> {
        let mut expired = Vec::new();
        for (status, instances) in &mut self.applied {
            let mut keep = Vec::with_capacity(instances.len());
            for mut inst in std::mem::take(instances) {
                inst.duration_remaining -= dt;
                if inst.duration_remaining <= 0 {
                    expired.push((status.clone(), inst.seq));
                } else {
                    keep.push(inst);
                }
            }
            *instances = keep;
        }
        self.applied.retain(|_, v| !v.is_empty());
        expired
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register the seven S0 effect/status parts as `Dynamic` parts with a serde
/// factory (same mechanism as the six content kinds), so blueprints carrying
/// `parts.Charges` / `parts.Damage` / `parts.Effect` / `parts.Modifiers` /
/// `parts.Abilities` / `parts.Derived` load and validate. `StatusEffects` is
/// runtime state but the name is declared here too (single source of truth
/// for part names).
pub fn register_all(registry: &mut PartRegistry) -> Result<(), PartError> {
    crate::kinds::register::<Abilities>(registry, PART_ABILITIES)?;
    crate::kinds::register::<Charges>(registry, PART_CHARGES)?;
    crate::kinds::register::<Damage>(registry, PART_DAMAGE)?;
    crate::kinds::register::<Derived>(registry, PART_DERIVED)?;
    crate::kinds::register::<Effect>(registry, PART_EFFECT)?;
    crate::kinds::register::<Modifiers>(registry, PART_MODIFIERS)?;
    crate::kinds::register::<StatusEffects>(registry, PART_STATUS_EFFECTS)?;
    Ok(())
}

/// Deserialize a part payload into `T` (the same `try_into` path the part
/// factory uses; handlers call this on a resolved blueprint's raw part).
/// `name` feeds the fail-closed diagnostic.
fn parse_part<T>(name: &str, value: &toml::Value) -> Result<T, PartError>
where
    T: serde::de::DeserializeOwned + std::fmt::Debug + Send + Sync + 'static,
{
    value
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| PartError::BuildFailed {
            name: name.to_string(),
            msg: e.to_string(),
        })
}

// ---------------------------------------------------------------------------
// Grant seam
// ---------------------------------------------------------------------------

/// Errors of the blueprint → entity grant seam.
#[derive(Debug, Error)]
pub enum GrantError {
    #[error("no blueprint '{0}'")]
    UnknownAbility(String),
    #[error("stamping '{ability}' charges onto the entity failed: {source}")]
    Stamp {
        ability: String,
        #[source]
        source: PartError,
    },
}

/// Stamp the charge pool declared in ability blueprint `ability` onto
/// `entity`, starting full (`charges = max`). Returns without touching the
/// part map when the ability declared no `parts.Charges` (a free ability).
/// This is the native resource-stamping seam the spawn path calls after
/// giving an entity its `parts.Abilities`.
pub fn grant_ability_resources(
    broker: &mut EntityBroker,
    library: &BlueprintLibrary,
    entity: Entity,
    ability: &str,
) -> Result<(), GrantError> {
    let Some(bp) = library.get(ability) else {
        return Err(GrantError::UnknownAbility(ability.to_string()));
    };
    let Some(cfg) = bp.part(PART_CHARGES) else {
        return Ok(());
    };
    let declared: Charges =
        cfg.clone()
            .try_into()
            .map_err(|e: toml::de::Error| GrantError::Stamp {
                ability: ability.to_string(),
                source: PartError::BuildFailed {
                    name: PART_CHARGES.to_string(),
                    msg: e.to_string(),
                },
            })?;
    broker
        .insert_part(
            entity,
            PART_CHARGES,
            Box::new(Charges::at_full(declared.max, declared.recharge)),
        )
        .map_err(|source| GrantError::Stamp {
            ability: ability.to_string(),
            source,
        })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Derived cache
// ---------------------------------------------------------------------------

/// The declared derived-cache invalidation hook: remove the entity's
/// `Derived` part so the next recompute (a rules script rewriting the part
/// through the whitelisted `set_part` path) starts from an empty cache.
/// Firing events: status applied/expired is the dirtying event wired by
/// [`install_native_effects`]; other dirtying events are declared the same
/// way by later increments.
pub fn invalidate_derived(ctx: &mut SimContext, entity: Entity) {
    let mut broker = ctx.broker.write().unwrap_or_else(|p| p.into_inner());
    let _ = broker.remove_part(entity, &PartTypeId::new(PART_DERIVED));
}

// ---------------------------------------------------------------------------
// Declarative resolution handlers
// ---------------------------------------------------------------------------

/// Register the native effect runtime on `ctx`: the `UseAbility` resource
/// path, the `ApplyStatus` store application, the `StatusApplied` /
/// `StatusExpired` observers that invalidate the target's `Derived` cache,
/// and — via the returned seam — duration ticking.
///
/// All handlers resolve declarative parts against `library` (the shared
/// content library). An unknown ability/status id cancels the event
/// (fail-closed: nothing happens, no follow-up events fire).
pub fn install_native_effects(ctx: &mut SimContext, library: Arc<RwLock<BlueprintLibrary>>) {
    let use_lib = library.clone();
    ctx.register::<UseAbility>(NORMAL, move |ctx, ev| {
        resolve_use_ability(ctx, ev, &use_lib)
    });
    ctx.register::<ApplyStatus>(NORMAL, move |ctx, ev| {
        resolve_apply_status(ctx, ev, &library)
    });
    ctx.register_observer::<StatusApplied>(NORMAL, |ctx, ev| invalidate_derived(ctx, ev.target));
    ctx.register_observer::<StatusExpired>(NORMAL, |ctx, ev| invalidate_derived(ctx, ev.target));
}

/// `UseAbility { user, ability, target }`:
///
/// 1. resource path — resolve the ability blueprint, spend one charge from
///    the user's pool (stamped by [`grant_ability_resources`]); out of
///    charges or no pool cancels the event;
/// 2. `parts.Damage` — roll `lo..=hi` on the forked combat stream and
///    dispatch the existing `TakeDamage` (dtype named, resolution deferred);
/// 3. `parts.Effect` — apply each status id in declared order via
///    `ApplyStatus`, then push the target `push` tiles away from the user
///    through the native `Move` seam (walkability + occupancy checked).
fn resolve_use_ability(
    ctx: &mut SimContext,
    ev: &mut UseAbility,
    library: &Arc<RwLock<BlueprintLibrary>>,
) -> HandlerAction {
    let lib = library.read().unwrap_or_else(|p| p.into_inner());
    let Some(bp) = lib.get(&ev.ability) else {
        return HandlerAction::Cancel;
    };

    // Resource path: the pool stamped from this ability's `parts.Charges`.
    // Spend inside the guard scope (the pool borrow lives as long as the
    // broker write guard); a missing or empty pool cancels the event.
    let spent = {
        let mut broker = ctx.broker.write().unwrap_or_else(|p| p.into_inner());
        let Some(pool) = broker.typed_part_mut::<Charges>(ev.user, &PartTypeId::new(PART_CHARGES))
        else {
            return HandlerAction::Cancel;
        };
        pool.spend()
    };
    if !spent {
        return HandlerAction::Cancel;
    }

    // Damage: roll on the forked combat stream, dispatch TakeDamage.
    if let Some(cfg) = bp.part(PART_DAMAGE) {
        let Ok(dmg) = parse_part::<Damage>(PART_DAMAGE, cfg) else {
            return HandlerAction::Cancel;
        };
        if dmg.lo > dmg.hi {
            return HandlerAction::Cancel;
        }
        let mut stream = ctx.rng.fork(COMBAT_STREAM_LABEL);
        let span = ((dmg.hi - dmg.lo) + 1).max(1) as u32;
        let amount = dmg.lo + i64::from(stream.below(span));
        let _ = ctx.dispatch(TakeDamage {
            target: ev.target,
            amount: i32::try_from(amount).unwrap_or(i32::MAX),
            source: DamageSource::Attack,
            dtype: dmg.dtype,
        });
    }

    // Effect: apply statuses in declared order, then push.
    if let Some(cfg) = bp.part(PART_EFFECT) {
        let Ok(eff) = parse_part::<Effect>(PART_EFFECT, cfg) else {
            return HandlerAction::Cancel;
        };
        for status in &eff.applies {
            let _ = ctx.dispatch(ApplyStatus {
                target: ev.target,
                status: status.clone(),
            });
        }
        if eff.push > 0
            && let Some(to) = resolve_push_dest(ctx, ev.user, ev.target, eff.push)
            && let Some(from) = ctx.position(ev.target)
        {
            let _ = ctx.dispatch(Move {
                actor: ev.target,
                from: Point::new(from.x, from.y),
                to,
            });
        }
    }
    HandlerAction::Continue
}

/// The farthest cell reachable pushing `target` `distance` tiles away from
/// `user` (axis-normalized direction), stopping at the first non-walkable or
/// occupied cell. `None` when the target cannot move at all (including when
/// the two entities share a cell — no direction to push).
fn resolve_push_dest(
    ctx: &SimContext,
    user: Entity,
    target: Entity,
    distance: i64,
) -> Option<Point> {
    let up = ctx.position(user)?;
    let tp = ctx.position(target)?;
    let dx = (tp.x - up.x).signum();
    let dy = (tp.y - up.y).signum();
    if dx == 0 && dy == 0 {
        return None;
    }
    let mut cur = Point::new(tp.x, tp.y);
    for _ in 0..distance {
        let next = Point::new(cur.x + dx, cur.y + dy);
        if !ctx.map.is_walkable(next.x, next.y) {
            break;
        }
        if ctx.tile_entry(next, target) != TileEntry::Clear {
            break;
        }
        cur = next;
    }
    (cur != Point::new(tp.x, tp.y)).then_some(cur)
}

/// `ApplyStatus { target, status }`: resolve the status blueprint, snapshot
/// its duration/tags/modifiers into the target's [`StatusEffects`] store
/// under the stacking policy, then fire [`StatusApplied`]. Unknown status id
/// cancels (no `StatusApplied` fires).
fn resolve_apply_status(
    ctx: &mut SimContext,
    ev: &mut ApplyStatus,
    library: &Arc<RwLock<BlueprintLibrary>>,
) -> HandlerAction {
    let (duration, stacking, tags, modifiers) = {
        let lib = library.read().unwrap_or_else(|p| p.into_inner());
        let Some(bp) = lib.get(&ev.status) else {
            return HandlerAction::Cancel;
        };
        let Some(cfg) = bp.part("Status") else {
            return HandlerAction::Cancel;
        };
        let Ok(st) = parse_part::<crate::kinds::Status>("Status", cfg) else {
            return HandlerAction::Cancel;
        };
        let modifiers = bp
            .part(PART_MODIFIERS)
            .and_then(|v| parse_part::<Modifiers>(PART_MODIFIERS, v).ok())
            .unwrap_or_default();
        (st.duration, st.stacking, st.tags, modifiers)
    };

    {
        let mut broker = ctx.broker.write().unwrap_or_else(|p| p.into_inner());
        if !broker
            .part_map(ev.target)
            .is_some_and(|m| m.contains(&PartTypeId::new(PART_STATUS_EFFECTS)))
        {
            let _ = broker.insert_part(
                ev.target,
                PART_STATUS_EFFECTS,
                Box::new(StatusEffects::new()),
            );
        }
        let Some(store) = broker
            .typed_part_mut::<StatusEffects>(ev.target, &PartTypeId::new(PART_STATUS_EFFECTS))
        else {
            return HandlerAction::Cancel;
        };
        store.apply(&ev.status, duration, stacking, &tags, modifiers);
    }

    let _ = ctx.dispatch(StatusApplied {
        target: ev.target,
        status: ev.status.clone(),
    });
    HandlerAction::Continue
}

/// Advance every status on `entity` by `dt` game-time units, firing
/// [`StatusExpired`] for each expiration (deterministic order; the scheduler
/// calls this seam on its clock — S0 wires the event, not the clock).
/// Returns the expired status ids.
pub fn elapse_statuses(ctx: &mut SimContext, entity: Entity, dt: i64) -> Vec<String> {
    let expired = {
        let mut broker = ctx.broker.write().unwrap_or_else(|p| p.into_inner());
        match broker.typed_part_mut::<StatusEffects>(entity, &PartTypeId::new(PART_STATUS_EFFECTS))
        {
            Some(store) => store.elapse(dt).into_iter().map(|(s, _)| s).collect(),
            None => Vec::new(),
        }
    };
    for status in &expired {
        let _ = ctx.dispatch(StatusExpired {
            target: entity,
            status: status.clone(),
        });
    }
    expired
}

/// The scheduler's stagger seam: consume one skip-turn credit from `entity`'s
/// status store (deterministic first-match order) and report whether the
/// entity's next scheduled action is skipped. The energy-clock scheduler
/// calls this between the turn hook and the action dispatch — a staggered
/// actor wastes its upcoming action rather than performing it.
pub fn consume_skip_turn(ctx: &mut SimContext, entity: Entity) -> bool {
    let mut broker = ctx.broker.write().unwrap_or_else(|p| p.into_inner());
    match broker.typed_part_mut::<StatusEffects>(entity, &PartTypeId::new(PART_STATUS_EFFECTS)) {
        Some(store) => store.consume_skip_turn(),
        None => false,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::blueprints::BlueprintLibrary;
    use crate::ecs::{EntityBroker, StorageClass};
    use crate::events::{EARLY, EventOutcome, LATE, Position};
    use crate::rng::SimRng;
    use crate::vfs::MemVfs;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// A registry with the six kinds plus the seven effect/status parts.
    fn full_registry() -> PartRegistry {
        let mut r = PartRegistry::new();
        crate::kinds::register_all(&mut r).unwrap();
        register_all(&mut r).unwrap();
        r
    }

    /// The smallest data set exercising the declarative path: a charged,
    /// damaging, status-applying, pushing ability and a staggered status.
    fn fixture_vfs() -> MemVfs {
        MemVfs::new().with(
            "blueprints/fx.toml",
            r##"
[Ability_ShieldBash]
parts.Ability = { name = "Shield Bash", kind = "active", target = "adjacent",
                  tags = ["melee", "stagger"] }
parts.Charges = { max = 3, recharge = 1200 }
parts.Damage  = { lo = 3, hi = 6, dtype = "blunt", stat = "might", acc_bonus = 3 }
parts.Effect  = { applies = ["Status_Staggered"], push = 1, script = "" }

[Status_Staggered]
parts.Status    = { name = "Staggered", duration = 100, stacking = "refresh",
                    tags = ["harmful", "control"] }
parts.Modifiers = { skip_turns = 1 }
"##,
        )
    }

    fn fixture_library() -> Arc<RwLock<BlueprintLibrary>> {
        let lib = BlueprintLibrary::load(&fixture_vfs(), &full_registry()).unwrap();
        Arc::new(RwLock::new(lib))
    }

    /// A context over the demo room with the native effect runtime installed.
    fn effects_context() -> SimContext {
        let mut ctx = SimContext::new(
            EntityBroker::new(),
            crate::zone::tests::demo_zone(20, 10),
            SimRng::from_seed(42),
        );
        install_native_effects(&mut ctx, fixture_library());
        ctx
    }

    fn spawn(ctx: &mut SimContext) -> Entity {
        ctx.broker
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .spawn()
    }

    fn spawn_at(ctx: &mut SimContext, x: i32, y: i32) -> Entity {
        let e = spawn(ctx);
        ctx.set_position(e, Position { x, y });
        e
    }

    fn grant(ctx: &mut SimContext, user: Entity, ability: &str) {
        let lib = fixture_library();
        grant_ability_resources(
            &mut ctx.broker.write().unwrap_or_else(|p| p.into_inner()),
            &lib.read().unwrap_or_else(|p| p.into_inner()),
            user,
            ability,
        )
        .unwrap();
    }

    // ------------------------------------------------------------------
    // part payloads
    // ------------------------------------------------------------------

    #[test]
    fn effect_parts_load_from_blueprints() {
        let lib = fixture_library();
        let lib = lib.read().unwrap_or_else(|p| p.into_inner());

        let bp = lib.get("Ability_ShieldBash").unwrap();
        let dmg: Damage = bp.part(PART_DAMAGE).unwrap().clone().try_into().unwrap();
        assert_eq!((dmg.lo, dmg.hi), (3, 6));
        assert_eq!(dmg.stat, "might");
        assert_eq!(dmg.dtype, "blunt");
        let eff: Effect = bp.part(PART_EFFECT).unwrap().clone().try_into().unwrap();
        assert_eq!(eff.applies, vec!["Status_Staggered".to_string()]);
        assert_eq!(eff.push, 1);
        assert_eq!(eff.script, "");
        let ch: Charges = bp.part(PART_CHARGES).unwrap().clone().try_into().unwrap();
        assert_eq!((ch.max, ch.recharge), (3, 1200));
        // the blueprint declaration carries no runtime charge count
        assert_eq!(ch.charges, 0);

        let sbp = lib.get("Status_Staggered").unwrap();
        let mods: Modifiers = sbp
            .part(PART_MODIFIERS)
            .unwrap()
            .clone()
            .try_into()
            .unwrap();
        assert_eq!(mods.skip_turns(), 1);
    }

    #[test]
    fn modifiers_shapes_parse_deterministically() {
        // int shorthand = add
        let m: Modifiers = toml_parse(r#"{ skip_turns = 1 }"#);
        assert_eq!(m.skip_turns(), 1);
        assert_eq!(m.entries().count(), 1);

        // `stat_pct` suffix = pct; plain key + table = both
        let m: Modifiers = toml_parse(r#"{ speed = { add = 10, pct = 150 } }"#);
        let e: Vec<_> = m.entries().collect();
        assert_eq!(
            e,
            vec![(&"speed".to_string(), AddPct { add: 10, pct: 150 })]
        );

        let m: Modifiers = toml_parse(r#"{ speed_pct = 150, accuracy = 5 }"#);
        let e: Vec<_> = m.entries().collect();
        assert_eq!(
            e,
            vec![
                (&"accuracy".to_string(), AddPct { add: 5, pct: 0 }),
                (&"speed".to_string(), AddPct { add: 0, pct: 150 }),
            ]
        );

        // a table under a `_pct` key is a payload contradiction -> fail closed
        let err: Result<Modifiers, toml::de::Error> =
            toml::from_str(r#"payload = { speed_pct = { add = 1 } }"#);
        assert!(err.is_err(), "table under `_pct` key must be rejected");
    }

    fn toml_parse<T: serde::de::DeserializeOwned>(text: &str) -> T {
        let anchored = format!("payload = {text}");
        let value: toml::Value = toml::from_str(&anchored).unwrap();
        let payload = value.get("payload").unwrap().clone();
        payload.try_into().unwrap()
    }

    // ------------------------------------------------------------------
    // additive-then-percent application order
    // ------------------------------------------------------------------

    #[test]
    fn modifier_application_is_additive_then_percent() {
        // base 20, add 10, pct 150 → (20+10)*150/100 = 45
        let m: Modifiers = toml_parse(r#"{ speed = 10, speed_pct = 150 }"#);
        assert_eq!(m.apply(20), 45);

        // no percent present → pure additive
        let m: Modifiers = toml_parse(r#"{ speed = 10, accuracy = 5 }"#);
        assert_eq!(m.apply(20), 35);

        // integer math, truncation toward zero (10*33/100 = 3.3 → 3)
        let m: Modifiers = toml_parse(r#"{ speed_pct = 33 }"#);
        assert_eq!(m.apply(10), 3);

        // percent applies to the added base, not the original (order proof:
        // (20+10)*150/100 = 45, whilst 20*150/100+10 = 40)
        let m: Modifiers = toml_parse(r#"{ speed = 10, speed_pct = 150 }"#);
        assert_eq!(m.apply(20), 45);

        // control field is read out, never applied to a stat base
        let m: Modifiers = toml_parse(r#"{ skip_turns = 1 }"#);
        assert_eq!(m.apply(100), 100);
        assert_eq!(m.skip_turns(), 1);
    }

    // ------------------------------------------------------------------
    // status store: refresh / stack / independent cap
    // ------------------------------------------------------------------

    fn mods() -> Modifiers {
        Modifiers::default()
    }

    #[test]
    fn status_refresh_resets_duration() {
        let mut store = StatusEffects::new();
        assert_eq!(
            store.apply("Status_Staggered", 100, Stacking::Refresh, &[], mods()),
            ApplyOutcome::Applied
        );
        store.elapse(40);
        assert_eq!(
            store.instances("Status_Staggered")[0].duration_remaining,
            60
        );
        // refresh: a reapplication resets the remaining duration to the full
        // declared duration, it does NOT stack on top.
        assert_eq!(
            store.apply("Status_Staggered", 100, Stacking::Refresh, &[], mods()),
            ApplyOutcome::Refreshed
        );
        let instances = store.instances("Status_Staggered");
        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].duration_remaining, 100);
    }

    #[test]
    fn status_stack_extends_duration() {
        let mut store = StatusEffects::new();
        store.apply("Status_Buff", 50, Stacking::Stack, &["buff".into()], mods());
        assert_eq!(
            store.apply("Status_Buff", 50, Stacking::Stack, &["buff".into()], mods()),
            ApplyOutcome::Stacked
        );
        let instances = store.instances("Status_Buff");
        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].duration_remaining, 100);
    }

    #[test]
    fn independent_status_capped_at_eight_fifo() {
        let mut store = StatusEffects::new();
        // Nine applications of an independent status: the first eight create
        // instances, the ninth evicts the first-to-expire (equal durations →
        // the oldest application, seq 0).
        let mut outcomes = Vec::new();
        for _ in 0..9 {
            outcomes.push(store.apply("Status_Doom", 200, Stacking::Independent, &[], mods()));
        }
        assert_eq!(outcomes[0], ApplyOutcome::Independent { evicted: None });
        assert_eq!(outcomes[8], ApplyOutcome::Independent { evicted: Some(0) });
        let instances = store.instances("Status_Doom");
        assert_eq!(instances.len(), INDEPENDENT_CAP);
        // the survivor set is the newest eight applications (seq 1..=8)
        let seqs: Vec<u64> = instances.iter().map(|i| i.seq).collect();
        assert_eq!(seqs, (1..=8).collect::<Vec<u64>>());
    }

    #[test]
    fn elapse_fires_expired_in_deterministic_order() {
        let mut ctx = effects_context();
        let e = spawn(&mut ctx);
        // Apply two statuses with different durations, then elapse past both.
        ctx.dispatch(ApplyStatus {
            target: e,
            status: "Status_Staggered".into(),
        })
        .unwrap();
        let expired = elapse_statuses(&mut ctx, e, 100);
        assert_eq!(expired, vec!["Status_Staggered".to_string()]);
        assert!(
            ctx.broker
                .read()
                .unwrap_or_else(|p| p.into_inner())
                .typed_part::<StatusEffects>(e, &PartTypeId::new(PART_STATUS_EFFECTS))
                .unwrap()
                .statuses()
                .next()
                .is_none()
        );
    }

    // ------------------------------------------------------------------
    // skip-turn consumption (the scheduler's stagger seam)
    // ------------------------------------------------------------------

    #[test]
    fn consume_skip_turn_spends_one_credit_per_call() {
        let mut store = StatusEffects::new();
        store.apply(
            "Status_Staggered",
            100,
            Stacking::Refresh,
            &[],
            toml_parse(r#"{ skip_turns = 1 }"#),
        );
        assert_eq!(store.skip_turns("Status_Staggered"), 1);

        // The first consume reports the credit and zeroes the count.
        assert!(store.consume_skip_turn());
        assert_eq!(store.skip_turns("Status_Staggered"), 0);
        // No credit left: further consumes report false and change nothing.
        assert!(!store.consume_skip_turn());
        assert_eq!(store.skip_turns("Status_Staggered"), 0);
        // The status instance itself survives (only the control credit
        // decrements) — expiry still bookkeeps normally.
        assert!(store.has("Status_Staggered"));
    }

    #[test]
    fn consume_skip_turn_is_deterministic_first_match() {
        // Two statuses both owing one skip: consumption is (status id
        // sorted, then application order) — Status_AAA before Status_BBB.
        let mut store = StatusEffects::new();
        store.apply(
            "Status_BBB",
            100,
            Stacking::Refresh,
            &[],
            toml_parse(r#"{ skip_turns = 1 }"#),
        );
        store.apply(
            "Status_AAA",
            100,
            Stacking::Refresh,
            &[],
            toml_parse(r#"{ skip_turns = 1 }"#),
        );
        assert!(store.consume_skip_turn());
        assert_eq!(store.skip_turns("Status_AAA"), 0);
        assert_eq!(
            store.skip_turns("Status_BBB"),
            1,
            "sorted order consumes AAA first"
        );
        assert!(store.consume_skip_turn());
        assert!(
            !store.consume_skip_turn(),
            "no credits left with no statuses"
        );
    }

    #[test]
    fn skip_turn_map_is_the_script_visible_snapshot() {
        let mut store = StatusEffects::new();
        store.apply(
            "Status_Staggered",
            100,
            Stacking::Refresh,
            &[],
            toml_parse(r#"{ skip_turns = 1 }"#),
        );
        store.apply("Status_Haste", 50, Stacking::Independent, &[], mods());
        let map = store.skip_turn_map();
        assert_eq!(map.get("Status_Staggered"), Some(&1));
        assert_eq!(map.get("Status_Haste"), Some(&0));
        // Consuming the credit updates the snapshot (what a script reads on
        // the next turn).
        assert!(store.consume_skip_turn());
        assert_eq!(store.skip_turn_map().get("Status_Staggered"), Some(&0));
    }

    #[test]
    fn scheduler_seam_consume_reports_false_without_a_store() {
        let mut ctx = effects_context();
        let e = spawn(&mut ctx);
        assert!(
            !consume_skip_turn(&mut ctx, e),
            "no status store -> nothing to skip"
        );
    }

    // ------------------------------------------------------------------
    // charges 3→2 (the native resource path)
    // ------------------------------------------------------------------

    #[test]
    fn use_ability_spends_native_charges_three_to_two() {
        let mut ctx = effects_context();
        let user = spawn(&mut ctx);
        grant(&mut ctx, user, "Ability_ShieldBash");
        let target = spawn_at(&mut ctx, 6, 5);

        let pool_before = ctx
            .broker
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .typed_part::<Charges>(user, &PartTypeId::new(PART_CHARGES))
            .unwrap()
            .clone();
        assert_eq!(pool_before.charges, 3);

        let out = ctx
            .dispatch(UseAbility {
                user,
                ability: "Ability_ShieldBash".into(),
                target,
            })
            .unwrap();
        assert!(!matches!(out, EventOutcome::Cancelled { .. }));

        let after = ctx
            .broker
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .typed_part::<Charges>(user, &PartTypeId::new(PART_CHARGES))
            .unwrap()
            .clone();
        assert_eq!(after.charges, 2, "one use spends one native charge");

        // three more uses exhaust the pool; the fourth cancels (no spend)
        for _ in 0..2 {
            let _ = ctx.dispatch(UseAbility {
                user,
                ability: "Ability_ShieldBash".into(),
                target,
            });
        }
        let out = ctx
            .dispatch(UseAbility {
                user,
                ability: "Ability_ShieldBash".into(),
                target,
            })
            .unwrap();
        assert!(matches!(out, EventOutcome::Cancelled { .. }));
        let last = ctx
            .broker
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .typed_part::<Charges>(user, &PartTypeId::new(PART_CHARGES))
            .unwrap()
            .clone();
        assert_eq!(last.charges, 0);
    }

    #[test]
    fn use_ability_without_pool_cancels() {
        let mut ctx = effects_context();
        let user = spawn(&mut ctx);
        let target = spawn_at(&mut ctx, 6, 5);
        let out = ctx
            .dispatch(UseAbility {
                user,
                ability: "Ability_ShieldBash".into(),
                target,
            })
            .unwrap();
        assert!(matches!(out, EventOutcome::Cancelled { .. }));
    }

    #[test]
    fn use_ability_unknown_ability_cancels() {
        let mut ctx = effects_context();
        let user = spawn(&mut ctx);
        let target = spawn_at(&mut ctx, 6, 5);
        let out = ctx
            .dispatch(UseAbility {
                user,
                ability: "Ability_Nonexistent".into(),
                target,
            })
            .unwrap();
        assert!(matches!(out, EventOutcome::Cancelled { .. }));
    }

    // ------------------------------------------------------------------
    // declarative resolution + event ordering (the Shield Bash path seam)
    // ------------------------------------------------------------------

    #[test]
    fn use_ability_resolves_effects_in_declared_order() {
        let mut ctx = effects_context();
        let user = spawn_at(&mut ctx, 5, 5);
        grant(&mut ctx, user, "Ability_ShieldBash");
        let target = spawn_at(&mut ctx, 6, 5);

        let log = Rc::new(RefCell::new(Vec::new()));
        let amounts = Rc::new(RefCell::new(Vec::new()));
        let dtypes = Rc::new(RefCell::new(Vec::new()));

        // Entry logging uses EARLY *mutators* (they run before the NORMAL
        // resolver mutators, so the log records the causal dispatch order:
        // UseAbility → TakeDamage → ApplyStatus → StatusApplied → Move).
        let l = log.clone();
        ctx.register::<crate::events::UseAbility>(EARLY, move |_, ev| {
            l.borrow_mut().push(ev.ability.clone());
            HandlerAction::Continue
        });
        let l = log.clone();
        ctx.register::<ApplyStatus>(EARLY, move |_, ev| {
            l.borrow_mut().push(ev.status.clone());
            HandlerAction::Continue
        });
        // Payload capture + effect-arrival logging via LATE observers.
        let a = amounts.clone();
        let d = dtypes.clone();
        let log2 = log.clone();
        ctx.register_observer::<TakeDamage>(LATE, move |_, ev| {
            log2.borrow_mut().push("TakeDamage".into());
            a.borrow_mut().push(ev.amount);
            d.borrow_mut().push(ev.dtype.clone());
        });
        let l = log.clone();
        ctx.register_observer::<StatusApplied>(LATE, move |_, ev| {
            l.borrow_mut().push(ev.status.clone());
        });
        let l = log.clone();
        ctx.register_observer::<Move>(LATE, move |_, ev| {
            l.borrow_mut()
                .push(format!("Move:{}->{}", ev.from.x, ev.to.x));
        });

        ctx.dispatch(UseAbility {
            user,
            ability: "Ability_ShieldBash".into(),
            target,
        })
        .unwrap();

        // The declared resolution order: damage first, then the status
        // application (which fires StatusApplied), then the push move.
        let got = log.borrow().clone();
        assert_eq!(
            got,
            vec![
                "Ability_ShieldBash".to_string(),
                "TakeDamage".to_string(),
                "Status_Staggered".to_string(),
                "Status_Staggered".to_string(),
                "Move:6->7".to_string(),
            ]
        );
        // Damage part: rolled in lo..=hi, dtype carried through.
        assert_eq!(amounts.borrow().len(), 1);
        assert!((3..=6).contains(&amounts.borrow()[0]));
        assert_eq!(dtypes.borrow().clone(), vec!["blunt".to_string()]);

        // the pushed target actually moved one tile away from the user
        let pos = ctx.position(target).unwrap();
        assert_eq!((pos.x, pos.y), (7, 5));
        // the status is applied on the target with its declared duration
        let store = ctx
            .broker
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .typed_part::<StatusEffects>(target, &PartTypeId::new(PART_STATUS_EFFECTS))
            .unwrap()
            .clone();
        assert!(store.has("Status_Staggered"));
        assert_eq!(store.skip_turns("Status_Staggered"), 1);
    }

    #[test]
    fn damage_roll_is_deterministic_per_seed_in_range() {
        // Two contexts seeded identically must roll identical damage and
        // resolve to identical event order (the forked-stream determinism
        // rule the later headless replay depends on).
        let run = |seed: u64| {
            let mut ctx = SimContext::new(
                EntityBroker::new(),
                crate::zone::tests::demo_zone(20, 10),
                SimRng::from_seed(seed),
            );
            install_native_effects(&mut ctx, fixture_library());
            let user = spawn_at(&mut ctx, 5, 5);
            grant(&mut ctx, user, "Ability_ShieldBash");
            let target = spawn_at(&mut ctx, 6, 5);
            let amounts = Rc::new(RefCell::new(Vec::new()));
            let a = amounts.clone();
            ctx.register_observer::<TakeDamage>(LATE, move |_, ev| {
                a.borrow_mut().push(ev.amount);
            });
            ctx.dispatch(UseAbility {
                user,
                ability: "Ability_ShieldBash".into(),
                target,
            })
            .unwrap();
            amounts.borrow().clone()
        };

        let a = run(42);
        let b = run(42);
        assert_eq!(a, b, "same seed must reproduce the identical roll");
        assert_eq!(a.len(), 1, "one Damage part → exactly one TakeDamage");
        assert!((3..=6).contains(&a[0]), "roll {} outside lo..=hi", a[0]);

        // a different seed may differ (sanity check the stream is live)
        let c = run(43);
        assert_eq!(c.len(), a.len());
    }

    #[test]
    fn push_stops_at_wall_or_occupant() {
        // user at (5,5), target at (6,5); the demo pillar at (8%4==… ) does
        // not collide here — instead place an occupant at the push cell.
        let mut ctx = effects_context();
        let user = spawn_at(&mut ctx, 5, 5);
        grant(&mut ctx, user, "Ability_ShieldBash");
        let target = spawn_at(&mut ctx, 6, 5);
        let _blocker = spawn_at(&mut ctx, 7, 5);

        let moved = Rc::new(RefCell::new(0));
        let m = moved.clone();
        ctx.register_observer::<Move>(LATE, move |_, _| {
            *m.borrow_mut() += 1;
        });

        ctx.dispatch(UseAbility {
            user,
            ability: "Ability_ShieldBash".into(),
            target,
        })
        .unwrap();

        // the pushed cell is occupied → no Move dispatch, target stays put
        assert_eq!(*moved.borrow(), 0);
        let pos = ctx.position(target).unwrap();
        assert_eq!((pos.x, pos.y), (6, 5));
    }

    // ------------------------------------------------------------------
    // Derived cache invalidation hook
    // ------------------------------------------------------------------

    #[test]
    fn derived_cache_invalidated_on_status_lifecycle_and_recomputes() {
        let mut ctx = effects_context();
        let e = spawn(&mut ctx);
        let id = PartTypeId::new(PART_DERIVED);

        // a rules script wrote values through the whitelisted set_part path
        ctx.broker
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .insert_part(
                e,
                PART_DERIVED,
                Box::new(Derived {
                    values: BTreeMap::from([("hp".into(), 100)]),
                }),
            )
            .unwrap();
        assert!(
            ctx.broker
                .read()
                .unwrap_or_else(|p| p.into_inner())
                .typed_part::<Derived>(e, &id)
                .is_some()
        );

        // status application is the declared dirtying event: cache cleared
        ctx.dispatch(ApplyStatus {
            target: e,
            status: "Status_Staggered".into(),
        })
        .unwrap();
        assert!(
            ctx.broker
                .read()
                .unwrap_or_else(|p| p.into_inner())
                .typed_part::<Derived>(e, &id)
                .is_none(),
            "Derived cache must be invalidated on StatusApplied"
        );

        // recompute seam: a rules script rewrites the cache → reads see it
        ctx.broker
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .insert_part(
                e,
                PART_DERIVED,
                Box::new(Derived {
                    values: BTreeMap::from([("hp".into(), 90)]),
                }),
            )
            .unwrap();
        let cached = ctx
            .broker
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .typed_part::<Derived>(e, &id)
            .unwrap()
            .clone();
        assert_eq!(cached.values["hp"], 90);

        // expiry is the other declared dirtying event: cleared again
        elapse_statuses(&mut ctx, e, 100);
        assert!(
            ctx.broker
                .read()
                .unwrap_or_else(|p| p.into_inner())
                .typed_part::<Derived>(e, &id)
                .is_none(),
            "Derived cache must be invalidated on StatusExpired"
        );
    }

    // ------------------------------------------------------------------
    // registration
    // ------------------------------------------------------------------

    #[test]
    fn effect_parts_register_sorted_dynamic() {
        let mut r = PartRegistry::new();
        register_all(&mut r).unwrap();
        let names: Vec<&str> = r.iter_storage().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "Abilities",
                "Charges",
                "Damage",
                "Derived",
                "Effect",
                "Modifiers",
                "StatusEffects"
            ]
        );
        for name in names {
            assert_eq!(
                r.storage(&PartTypeId::new(name)),
                Some(StorageClass::Dynamic)
            );
        }
        // duplicate registration rejected (single source of truth)
        assert!(matches!(
            register_all(&mut r),
            Err(PartError::DuplicatePartType(id)) if id.as_str() == "Abilities"
        ));
    }

    #[test]
    fn grant_stamps_full_pool_from_blueprint() {
        let mut ctx = effects_context();
        let user = spawn(&mut ctx);
        grant(&mut ctx, user, "Ability_ShieldBash");
        let pool = ctx
            .broker
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .typed_part::<Charges>(user, &PartTypeId::new(PART_CHARGES))
            .unwrap()
            .clone();
        assert_eq!((pool.max, pool.recharge, pool.charges), (3, 1200, 3));
    }
}
