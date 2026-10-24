//! Typed event bus with priorities, cancel/modify semantics, and strict
//! re-entrancy control over a deterministic, replayable event stream.
//!
//! # Model
//!
//! Parts and scripts communicate by dispatching a typed event (a plain struct
//! implementing [`Event`]) through a [`SimContext`]. Handlers are registered
//! per event type with an `i8` priority and run in deterministic
//! `(priority, seq)` order — a monotonic `seq` counter breaks priority ties by
//! registration order, stored in a `BTreeMap`/`BTreeSet` so no `HashMap` is
//! involved (determinism rule 3). Mutator handlers run first (they may modify
//! the payload in place and may cancel); observer handlers run after the
//! mutator phase in the same deterministic order and may not cancel.
//!
//! # Determinism
//!
//! Every collection here is a `BTreeMap`/`BTreeSet`/`Vec` — no `HashMap` or
//! `HashSet` (enforced by the crate's `clippy.toml`). Handler ordering,
//! re-entrancy rules, and depth checks are fully deterministic and
//! reproducible from a seed (the bus consumes no randomness by itself).
//!
//! # Re-entrancy and depth
//!
//! A handler may dispatch follow-up events (nested dispatch) so long as the
//! `(event_name, entity)` pair being dispatched is not already in flight;
//! otherwise the nested dispatch returns [`EventError::ReentrantDispatch`],
//! never panics. A runaway cascade is stopped by a depth cap (16); exceeding
//! it returns [`EventError::DepthExceeded`]. Handler lists are snapshotted
//! before iteration, so a handler that registers/unregisters handlers
//! mid-dispatch does not corrupt the current pass (new registrations take
//! effect on the next dispatch).

use std::any::Any;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};

use hecs::Entity;
use thiserror::Error;

use crate::ecs::{EntityBroker, PartTypeId};
use crate::rng::SimRng;
use crate::zone::Zone;

/// Priority band constants (i8 range, headroom for mods to slot before/after
/// base handlers without renumbering). Mid-values (e.g. -32, 20) are allowed.
pub const EARLY: i8 = -64;
pub const NORMAL: i8 = 0;
pub const LATE: i8 = 64;

/// Hard cap on nested dispatches before [`EventError::DepthExceeded`].
pub const MAX_DEPTH: usize = 16;

/// Name of the dynamic position part attached to an entity by [`SimContext`].
pub const PART_POSITION: &str = "Position";
/// Name of the dynamic health part used by minimal combat ([`Attack`]).
pub const PART_HEALTH: &str = "Health";

/// A whole-cell coordinate on the map. Integer-only (determinism rule 5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

/// The event-facing `Position` part attached to an entity (written by
/// [`Move`]'s post-dispatch commit step). Integer-only cell coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct Position {
    pub x: i32,
    pub y: i32,
}

impl Position {
    pub fn from_point(p: Point) -> Self {
        Self { x: p.x, y: p.y }
    }
}

/// Base-data `Health` part. Integer-only; used by minimal combat, which
/// requires a defender to carry this part before [`Attack`] resolves.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct Health {
    pub hp: i32,
}

/// The outcome of one actor attempting to enter a tile, from its perspective.
/// Caves-of-Qud-style occupancy: at most one actor per tile, and moving into a
/// live enemy becomes a bump attack rather than an overlap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TileEntry {
    /// The tile holds no live actor the mover would collide with; a `Move`
    /// onto it may proceed.
    Clear,
    /// The tile holds a live, attackable actor (a `Health` part with `hp >
    /// 0`). Moving into it is a bump: dispatch an `Attack` against the carried
    /// entity instead of a `Move`.
    Bump(Entity),
    /// The tile holds a live blocking actor that cannot be attacked (no
    /// `Health` part, or a `Health` with `hp == 0`). A `Move` onto it is
    /// cancelled, the same way a wall-blocked move is cancelled.
    Blocked,
}

/// What damaged a [`TakeDamage`] target. Integer-only enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DamageSource {
    /// A direct physical attack ([`Attack`]).
    Attack,
    /// A declarative ability effect ([`UseAbility`] resolution): the ability
    /// blueprint's `parts.Damage`. Hit resolution itself is later work; the
    /// source is named now so the pipeline can tell the two apart.
    Ability,
}

/// A typed gameplay event. Events are plain structs; handlers mutate the
/// payload **in place** through the `&mut` argument and may cancel by
/// returning [`HandlerAction::Cancel`] (observers may not). Events reference
/// entities by id only — never a `&mut World` — which is what lets the bus
/// enforce re-entrancy control.
pub trait Event: 'static {
    /// Stable name used by diagnostics and tooling scripts, e.g. `"Move"`.
    const NAME: &'static str;

    /// The primary entity this event is about. Used for the re-entrancy key
    /// `(event_name, entity)` and for the unknown-entity check.
    fn entity(&self) -> Entity;

    /// Whether this event is still considered handled after a mutator runs.
    /// Defaults to `true`; a mutator may set this to `false` to veto/cancel
    /// (the same signal as returning [`HandlerAction::Cancel`]).
    fn handled(&mut self) -> bool {
        true
    }

    /// Validate the payload *before* dispatch. Return an error to reject the
    /// event at the bus boundary (fail loudly, not deep in handlers). Default:
    /// always valid.
    fn validate(&self) -> Result<(), EventError> {
        Ok(())
    }

    /// Called after the mutator phase completes without cancellation, before
    /// observers, so query-style events can commit their effect to world state
    /// (e.g. [`Move`] writes the actor's `Position`). Default: no effect.
    fn post_handled(&mut self, _ctx: &mut SimContext) {}
}

/// What a mutator handler returns after running.
#[derive(Clone, Debug, PartialEq)]
pub enum HandlerAction {
    /// Continue dispatching to the remaining handlers.
    Continue,
    /// Veto the event now: stop the mutator phase, skip observers.
    Cancel,
    /// Abort the entire dispatch and surface `err` to the original caller. A
    /// handler that dispatched a nested event whose return was an
    /// [`EventError`] (e.g. [`EventError::DepthExceeded`]) returns this to
    /// propagate it up to the original caller.
    Fail(EventError),
}

/// A stable per-event-type handler identifier (the `seq` registration slot).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct HandlerId(pub u64);

/// Unique ordered key: `(priority, seq, id)`. Sorted by `BTreeMap` iteration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct HandlerKey {
    priority: i8,
    seq: u64,
    id: HandlerId,
}

/// Result of a completed dispatch.
#[derive(Clone, Debug)]
pub enum EventOutcome<E> {
    /// The event ran to completion and its payload was not changed by any
    /// mutator (or the event type has no query-style result).
    Handled,
    /// A mutator vetoed the event; observers were skipped.
    Cancelled { by: HandlerId },
    /// The event ran to completion with a modified payload.
    Modified(E),
}

/// Errors the bus returns (never panics) for invalid, unknown, re-entrant, or
/// runaway-dispatch inputs.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum EventError {
    #[error("event {name:?} references unknown entity {entity:?}")]
    UnknownEntity { name: &'static str, entity: Entity },
    #[error("re-entrant dispatch of {name:?} on entity {entity:?} while it is in flight")]
    ReentrantDispatch { name: &'static str, entity: Entity },
    #[error("dispatch depth exceeded (max {max})")]
    DepthExceeded { max: usize },
    #[error("invalid payload for {name:?}: {msg}")]
    InvalidPayload { name: &'static str, msg: String },
}

/// The mutable context handed to every handler: the entity broker (pairing
/// hecs components with dynamic parts), the map, forked RNG streams, and the
/// dispatch bookkeeping (depth, in-flight pairs). Handlers call
/// [`SimContext::dispatch`] on it to fire follow-up events.
pub struct SimContext {
    /// The entity store. Handlers read/write world state through this. Shared
    /// with the script host's [`WorldAccess`] (see `tessera-script`) so native
    /// typed handlers and scripted handlers operate on the *same* live broker;
    /// a single `Arc<RwLock<EntityBroker>>` is seen by both dispatch paths.
    pub broker: Arc<RwLock<EntityBroker>>,
    /// The current zone (provided for handler-side spatial checks).
    pub map: Zone,
    /// Forked, deterministic RNG available to handlers.
    pub rng: SimRng,
    /// Named per-system streams, created lazily from [`Self::rng`] and kept for
    /// the context's lifetime (and across save/load). This is the "one forked
    /// stream per system" rule: the first draw of a label forks once from the
    /// parent, and every later draw of that label touches only the cached
    /// entry — so adding a roll in one system cannot shift another.
    streams: BTreeMap<String, SimRng>,
    /// The typed handler store and dispatch bookkeeping.
    bus: EventBus,
    /// Deferred `RecomputeDerived` targets: dirtying observers queue here
    /// instead of dispatching inline, and [`SimContext::dispatch`] flushes
    /// them once the dispatch stack unwinds to depth 0 — so the recompute
    /// (and its headless log line) lands *after* the dirtying dispatch and
    /// its own observer lines, matching the declared ordering.
    /// Entities are flushed in queue order (deterministic).
    pending_recompute: Vec<Entity>,
}

impl SimContext {
    /// Build a context owning an exclusive broker (tests / callers that don't
    /// share with a script host).
    pub fn new(broker: EntityBroker, map: Zone, rng: SimRng) -> Self {
        Self::shared(Arc::new(RwLock::new(broker)), map, rng)
    }

    /// Build a context over a *shared* broker handle. Native typed handlers
    /// and the script host's world access lock the same handle, so the script
    /// `WorldAccess` is implemented over this context.
    pub fn shared(broker: Arc<RwLock<EntityBroker>>, map: Zone, rng: SimRng) -> Self {
        Self {
            broker,
            map,
            rng,
            streams: BTreeMap::new(),
            bus: EventBus::default(),
            pending_recompute: Vec::new(),
        }
    }

    /// The persistent named stream `label`, forked once from [`Self::rng`] on
    /// first use and returned mutable thereafter. The parent is drawn exactly
    /// once per label for the context's whole life, so a system's draws never
    /// perturb another system's sequence.
    pub fn stream(&mut self, label: &str) -> &mut SimRng {
        if !self.streams.contains_key(label) {
            let forked = self.rng.fork(stream_label_hash(label));
            self.streams.insert(label.to_string(), forked);
        }
        self.streams
            .get_mut(label)
            .expect("stream inserted immediately above")
    }

    /// The named-stream map, for the save snapshot. Each entry carries its full
    /// ChaCha8 state, so a load resumes every stream exactly where it stood.
    pub fn streams(&self) -> &BTreeMap<String, SimRng> {
        &self.streams
    }

    /// Replace the named-stream map from a loaded snapshot. Called by the
    /// load path after the parent stream is restored.
    pub fn restore_streams(&mut self, streams: BTreeMap<String, SimRng>) {
        self.streams = streams;
    }

    /// Queue a derivable entity for recompute once the current dispatch
    /// stack fully unwinds (the dirtying-event deferral instead of a nested
    /// dispatch; see [`Self::pending_recompute`]).
    pub fn defer_recompute(&mut self, entity: Entity) {
        self.pending_recompute.push(entity);
    }

    /// Write (upsert) an entity's `Position` part.
    pub fn set_position(&mut self, entity: Entity, pos: Position) -> Option<()> {
        self.broker
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .part_map_mut(entity)
            .map(|m| m.insert(PartTypeId::new(PART_POSITION), Box::new(pos)))
            .map(|_| ())
    }

    /// Read an entity's `Position` part, if present.
    pub fn position(&self, entity: Entity) -> Option<Position> {
        self.broker
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .typed_part::<Position>(entity, &PartTypeId::new(PART_POSITION))
            .copied()
    }

    /// Classify the tile `tile` from the perspective of `mover` entering it:
    /// whether it is free to move into, holds a live enemy to bump-attack, or
    /// is occupied by a blocking non-combat actor. Scans every live entity
    /// carrying a [`Position`] in deterministic (entity-id) order; the mover
    /// itself is excluded. This is the single occupancy query behind the
    /// Caves-of-Qud bump rule (moving into a live `Health` actor becomes an
    /// `Attack`, never an overlap).
    // all Health actors are attackable for now
    pub fn tile_entry(&self, tile: Point, mover: Entity) -> TileEntry {
        let b = self.broker.read().unwrap_or_else(|p| p.into_inner());
        let mut bump: Option<Entity> = None;
        let mut blocked = false;
        for e in b.entities_with_part(PART_POSITION) {
            if e == mover {
                continue;
            }
            let in_tile = b
                .typed_part::<Position>(e, &PartTypeId::new(PART_POSITION))
                .map(|p| p.x == tile.x && p.y == tile.y)
                .unwrap_or(false);
            if !in_tile {
                continue;
            }
            let live = b
                .typed_part::<Health>(e, &PartTypeId::new(PART_HEALTH))
                .map(|h| h.hp > 0)
                .unwrap_or(false);
            if live {
                bump = Some(e);
                break;
            }
            blocked = true;
        }
        match bump {
            Some(e) => TileEntry::Bump(e),
            None if blocked => TileEntry::Blocked,
            None => TileEntry::Clear,
        }
    }

    /// Register a **mutator** handler for event type `E`. Mutators run by
    /// priority and may modify `&mut E` and may cancel (return
    /// [`HandlerAction::Cancel`]); cancellation stops the mutator phase and
    /// skips observers.
    ///
    /// Returns the handler id assigned by deterministic registration order.
    pub fn register<E: Event>(
        &mut self,
        priority: i8,
        handler: impl Fn(&mut SimContext, &mut E) -> HandlerAction + 'static,
    ) -> HandlerId {
        self.bus.register::<E>(priority, handler)
    }

    /// Register an **observer** handler for event type `E`. Observers run
    /// after the mutator phase in the same deterministic order and may not
    /// cancel (they receive the read-only payload). If any mutator cancelled,
    /// observers are skipped entirely.
    pub fn register_observer<E: Event>(
        &mut self,
        priority: i8,
        handler: impl Fn(&mut SimContext, &E) + 'static,
    ) -> HandlerId {
        self.bus.register_observer::<E>(priority, handler)
    }

    /// Remove a handler registered for event type `E` by its id. A no-op if
    /// the id is unknown. Mid-dispatch removals take effect on the next
    /// dispatch (the current pass already snapshotted its handler list).
    pub fn unregister<E: Event>(&mut self, id: HandlerId) {
        self.bus.unregister::<E>(id);
    }

    /// Dispatch an event through the bus.
    ///
    /// Order of checks: payload validation, entity existence, re-entrancy,
    /// depth. On success, mutators run in `(priority, seq)` order; if none
    /// cancel, the event's [`Event::post_handled`] commit step runs (e.g.
    /// [`Move`] writing `Position`) followed by observers. Returns the
    /// outcome, or an [`EventError`] — never panics for rule violations.
    pub fn dispatch<E: Event + Clone + PartialEq + std::fmt::Debug>(
        &mut self,
        ev: E,
    ) -> Result<EventOutcome<E>, EventError> {
        let name = E::NAME;
        let entity = ev.entity();

        ev.validate()?;
        if !self
            .broker
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .is_alive(entity)
        {
            return Err(EventError::UnknownEntity { name, entity });
        }

        // Snapshot the handler list (clones the Arc handles, not the closure
        // state) before touching dispatch bookkeeping or running anything, so
        // a handler that registers/unregisters mid-dispatch cannot corrupt
        // the current pass.
        let (mutators, observers) = self.bus.snapshot::<E>(name);

        if self.bus.in_flight.contains(&(name, entity)) {
            return Err(EventError::ReentrantDispatch { name, entity });
        }
        if self.bus.depth >= MAX_DEPTH {
            return Err(EventError::DepthExceeded { max: MAX_DEPTH });
        }
        self.bus.depth += 1;
        self.bus.in_flight.insert((name, entity));

        let original = ev.clone();
        let mut ev = ev;
        let mut cancelled: Option<HandlerId> = None;
        let mut failed: Option<EventError> = None;

        for (_key, handler) in &mutators {
            match handler(&mut *self, &mut ev) {
                HandlerAction::Continue => continue,
                HandlerAction::Cancel => {
                    cancelled = Some(_key.id);
                    break;
                }
                HandlerAction::Fail(err) => {
                    failed = Some(err);
                    break;
                }
            }
        }

        if cancelled.is_none() && failed.is_none() {
            ev.post_handled(&mut *self);
            for (_key, handler) in &observers {
                handler(&mut *self, &ev);
            }
        }

        self.bus.depth -= 1;
        self.bus.in_flight.remove(&(name, entity));

        // Flush deferred recomputes once the dispatch stack fully unwinds:
        // status dirtying queues them (instead of a nested dispatch) so the
        // recompute — and its headless log line — lands after the dirtying
        // event's own observers.
        if self.bus.depth == 0 {
            let deferred = std::mem::take(&mut self.pending_recompute);
            for entity in deferred {
                let _ = self.dispatch(RecomputeDerived { entity });
            }
        }

        if let Some(err) = failed {
            return Err(err);
        }
        match cancelled {
            Some(by) => Ok(EventOutcome::Cancelled { by }),
            None if ev == original => Ok(EventOutcome::Handled),
            None => Ok(EventOutcome::Modified(ev)),
        }
    }
}

// ---------------------------------------------------------------------------
// Event definitions
// ---------------------------------------------------------------------------

/// A request to move `actor` from `from` to `to`. On success (not cancelled),
/// [`Move::post_handled`] writes the actor's `Position` part to `to`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Move {
    pub actor: Entity,
    pub from: Point,
    pub to: Point,
}

impl Event for Move {
    const NAME: &'static str = "Move";

    fn entity(&self) -> Entity {
        self.actor
    }

    fn validate(&self) -> Result<(), EventError> {
        if self.from == self.to {
            return Err(EventError::InvalidPayload {
                name: Self::NAME,
                msg: "from == to".into(),
            });
        }
        Ok(())
    }

    fn post_handled(&mut self, ctx: &mut SimContext) {
        ctx.set_position(self.actor, Position::from_point(self.to));
    }
}

/// A request for `attacker` to hit `defender`. Minimal combat ([`Attack`]):
/// resolves to `TakeDamage { amount: 1, source: Attack }` when the
/// defender carries a [`Health`] part; otherwise the attack cancels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attack {
    pub attacker: Entity,
    pub defender: Entity,
}

impl Event for Attack {
    const NAME: &'static str = "Attack";

    fn entity(&self) -> Entity {
        // The subject of the attack is the defender (the entity being acted
        // upon) — used for the re-entrancy key.
        self.defender
    }
}

/// Fired inside an [`Attack`] resolution, after the cached chance to hit is
/// computed and before any roll is taken. Mutators may cancel it to veto the
/// attack (a dodge/sidestep hook) or adjust `hit_pct`; observers see the final
/// chance. The native pipeline itself registers no handler here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BeforeHit {
    pub attacker: Entity,
    pub defender: Entity,
    /// The cached chance to hit the attack would use.
    pub hit_pct: i64,
}

impl Event for BeforeHit {
    const NAME: &'static str = "BeforeHit";

    fn entity(&self) -> Entity {
        self.defender
    }
}

/// Fired when an [`Attack`]'s hit roll lands. Exactly one of `Hit`, `Missed`
/// and `Blocked` fires per attack. `crit` is `roll <= min(crit_threshold,
/// hit_pct)`; the damage itself travels in the follow-up `TakeDamage`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hit {
    pub attacker: Entity,
    pub defender: Entity,
    /// The hit d100 that landed.
    pub roll: i64,
    /// The chance it was resolved against.
    pub hit_pct: i64,
    pub crit: bool,
}

impl Event for Hit {
    const NAME: &'static str = "Hit";

    fn entity(&self) -> Entity {
        self.defender
    }
}

/// Fired when the hit roll fails: no damage, no further draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Missed {
    pub attacker: Entity,
    pub defender: Entity,
    pub roll: i64,
    pub hit_pct: i64,
}

impl Event for Missed {
    const NAME: &'static str = "Missed";

    fn entity(&self) -> Entity {
        self.defender
    }
}

/// Fired when a landed physical hit is negated by the defender's shield. No
/// damage, no damage draw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Blocked {
    pub attacker: Entity,
    pub defender: Entity,
    pub roll: i64,
    pub hit_pct: i64,
    pub block_pct: i64,
}

impl Event for Blocked {
    const NAME: &'static str = "Blocked";

    fn entity(&self) -> Entity {
        self.defender
    }
}

/// Damage applied to `target`. Mutators may modify `amount` (armor, statuses)
/// before the final mutation phase reads it. Negative `amount` is rejected at
/// the boundary, as is a `dtype` outside the closed damage-type vocabulary.
/// `dtype` is the damage-type tag: `"generic"` (the compatibility tag, zero
/// resistance and threshold) or one of [`crate::combat::DamageType`]'s names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TakeDamage {
    pub target: Entity,
    pub amount: i32,
    pub source: DamageSource,
    pub dtype: String,
}

impl TakeDamage {
    /// A direct attack with the generic damage type (`Attack`-pipeline hits
    /// that carry no declarative dtype).
    pub fn generic(target: Entity, amount: i32, source: DamageSource) -> Self {
        Self {
            target,
            amount,
            source,
            dtype: "generic".to_string(),
        }
    }
}

impl Event for TakeDamage {
    const NAME: &'static str = "TakeDamage";

    fn entity(&self) -> Entity {
        self.target
    }

    fn validate(&self) -> Result<(), EventError> {
        if self.amount < 0 {
            return Err(EventError::InvalidPayload {
                name: Self::NAME,
                msg: format!("negative amount {}", self.amount),
            });
        }
        if crate::combat::DamageType::from_str(&self.dtype).is_none() {
            return Err(EventError::InvalidPayload {
                name: Self::NAME,
                msg: format!("unknown damage type '{}'", self.dtype),
            });
        }
        Ok(())
    }
}

/// Internal event: the start of an actor's turn. **Reserved** for the turn
/// scheduler; dispatched by it, observed for AI/renderer hooks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActorTurnStart {
    pub actor: Entity,
}

impl Event for ActorTurnStart {
    const NAME: &'static str = "ActorTurnStart";

    fn entity(&self) -> Entity {
        self.actor
    }
}

/// Internal event: `target` has died. **Reserved** for future use;
/// dispatched by death/corpse logic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Death {
    pub target: Entity,
}

impl Event for Death {
    const NAME: &'static str = "Death";

    fn entity(&self) -> Entity {
        self.target
    }
}

/// The minimal ability-use event: `user` spends the resource for the named
/// ability (a blueprint id) aimed at `target`. The native resource path
/// (declarative effect resolution) decrements the user's charge pool; the
/// targeting pipeline that *selects* ability/target is later work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UseAbility {
    pub user: Entity,
    pub ability: String,
    pub target: Entity,
}

impl Event for UseAbility {
    const NAME: &'static str = "UseAbility";

    fn entity(&self) -> Entity {
        self.user
    }
}

/// A request to apply the named status (a blueprint id) to `target`. Native
/// resolution snapshots the status definition into the target's status store
/// and fires [`StatusApplied`]; unknown status ids cancel the event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyStatus {
    pub target: Entity,
    pub status: String,
}

impl Event for ApplyStatus {
    const NAME: &'static str = "ApplyStatus";

    fn entity(&self) -> Entity {
        self.target
    }
}

/// Fired when `status` is successfully applied to `target` (the named
/// application seam; payload enrichment is later work).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusApplied {
    pub target: Entity,
    pub status: String,
}

impl Event for StatusApplied {
    const NAME: &'static str = "StatusApplied";

    fn entity(&self) -> Entity {
        self.target
    }
}

/// Fired when an applied `status` on `target` expires (duration elapses past
/// zero). The scheduler consumes the seam later; it currently fires once per
/// expiration in deterministic order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusExpired {
    pub target: Entity,
    pub status: String,
}

impl Event for StatusExpired {
    const NAME: &'static str = "StatusExpired";

    fn entity(&self) -> Entity {
        self.target
    }
}

/// Internal event: recompute the cached `Derived` part of `entity` (the
/// declared stat-pipeline invalidation point). Dispatched after every
/// dirtying event in the declared set — spawn completion,
/// `StatusApplied`/`StatusExpired`, writes to `Attributes`/`Level`, and a
/// `Tuning_*` content reload — so the recomputation always sees settled
/// inputs.
///
/// The native handler clears the cache first (the `invalidate_derived` hook,
/// absent is detectable), the engine forwards the event to the registered
/// `rules/stats.luau` handler (which recomputes the five formula bases and
/// writes them via the whitelisted `set_part` path), and a native finalize
/// seam applies live status modifiers, the invariant clamps and the
/// `Health.hp <= max_hp` clamp before control returns to the scheduler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecomputeDerived {
    pub entity: Entity,
}

impl Event for RecomputeDerived {
    const NAME: &'static str = "RecomputeDerived";

    fn entity(&self) -> Entity {
        self.entity
    }
}

// ---------------------------------------------------------------------------
// Handler storage
// ---------------------------------------------------------------------------

type Mutator<E> = Arc<dyn Fn(&mut SimContext, &mut E) -> HandlerAction>;
type Observer<E> = Arc<dyn Fn(&mut SimContext, &E)>;

/// A snapshot of the handler list for one event type, in dispatch order:
/// `(mutators, observers)`.
type Snapshot<E> = (
    Vec<(HandlerKey, Mutator<E>)>,
    Vec<(HandlerKey, Observer<E>)>,
);

/// The handler store for one event type `E`. Both mutators and observers are
/// kept in `BTreeMap<HandlerKey, _>` so iteration is in deterministic
/// `(priority, seq)` order. `next_seq` is the monotonic registration counter
/// that breaks priority ties by registration order.
struct HandlerStore<E> {
    mutators: BTreeMap<HandlerKey, Mutator<E>>,
    observers: BTreeMap<HandlerKey, Observer<E>>,
    next_seq: u64,
}

impl<E: Event> HandlerStore<E> {
    fn new() -> Self {
        HandlerStore {
            mutators: BTreeMap::new(),
            observers: BTreeMap::new(),
            next_seq: 0,
        }
    }
}

/// The typed event stores together with cross-event dispatch bookkeeping.
/// Keyed by `Event::NAME`.
#[derive(Default)]
struct EventBus {
    stores: BTreeMap<&'static str, Box<dyn Any>>,
    /// Pairs `(event_name, entity)` currently mid-dispatch (re-entrancy set).
    in_flight: BTreeSet<(&'static str, Entity)>,
    /// Current nested-dispatch depth.
    depth: usize,
}

impl EventBus {
    fn store_mut<E: Event>(&mut self) -> &mut HandlerStore<E> {
        self.stores
            .entry(E::NAME)
            .or_insert_with(|| Box::new(HandlerStore::<E>::new()))
            .downcast_mut::<HandlerStore<E>>()
            .expect("store keyed by E::NAME must be a HandlerStore<E>")
    }

    fn register<E: Event>(
        &mut self,
        priority: i8,
        handler: impl Fn(&mut SimContext, &mut E) -> HandlerAction + 'static,
    ) -> HandlerId {
        let store = self.store_mut::<E>();
        let seq = store.next_seq;
        store.next_seq += 1;
        let id = HandlerId(seq);
        store
            .mutators
            .insert(HandlerKey { priority, seq, id }, Arc::new(handler));
        id
    }

    fn register_observer<E: Event>(
        &mut self,
        priority: i8,
        handler: impl Fn(&mut SimContext, &E) + 'static,
    ) -> HandlerId {
        let store = self.store_mut::<E>();
        let seq = store.next_seq;
        store.next_seq += 1;
        let id = HandlerId(seq);
        store
            .observers
            .insert(HandlerKey { priority, seq, id }, Arc::new(handler));
        id
    }

    fn unregister<E: Event>(&mut self, id: HandlerId) {
        let store = self.store_mut::<E>();
        store.mutators.retain(|key, _| key.id != id);
        store.observers.retain(|key, _| key.id != id);
    }

    /// Take the snapshot, cloning the Arc handles (not the closed-over state).
    fn snapshot<E: Event>(&self, name: &'static str) -> Snapshot<E> {
        let Some(store) = self
            .stores
            .get(&name)
            .and_then(|b| b.downcast_ref::<HandlerStore<E>>())
        else {
            return (Vec::new(), Vec::new());
        };
        let mutators = store
            .mutators
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect();
        let observers = store
            .observers
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect();
        (mutators, observers)
    }
}

/// A deterministic 64-bit hash of a stream label, used as the fork label so
/// distinct systems derive distinct streams from the parent. FNV-1a: fixed
/// arithmetic, identical on every platform.
pub fn stream_label_hash(label: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in label.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecs::{EntityBroker, PartTypeId};
    use std::cell::RefCell;
    use std::rc::Rc;

    fn context() -> SimContext {
        SimContext::new(
            EntityBroker::new(),
            crate::zone::tests::demo_zone(20, 10),
            SimRng::from_seed(1),
        )
    }

    fn log() -> Rc<RefCell<Vec<String>>> {
        Rc::new(RefCell::new(Vec::new()))
    }

    fn spawn_pos(ctx: &mut SimContext, x: i32, y: i32) -> Entity {
        let e = ctx
            .broker
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .spawn();
        ctx.set_position(e, Position { x, y });
        e
    }

    // ---- 1. deterministic ordering and registration ties -----------------

    #[test]
    fn handlers_run_in_priority_then_registration_order() {
        let mut ctx = context();
        let log = log();
        let push = |log: Rc<RefCell<Vec<String>>>, s: &'static str| {
            let log = log.clone();
            move |_ctx: &mut SimContext, _ev: &mut Move| {
                log.borrow_mut().push(s.to_string());
                HandlerAction::Continue
            }
        };
        ctx.register::<Move>(LATE, push(log.clone(), "late"));
        ctx.register::<Move>(NORMAL, push(log.clone(), "normal"));
        ctx.register::<Move>(EARLY, push(log.clone(), "early"));

        // Two same-priority handlers: registration order breaks the tie.
        ctx.register::<Move>(NORMAL, push(log.clone(), "normal2"));

        let e = spawn_pos(&mut ctx, 5, 5);
        ctx.dispatch(Move {
            actor: e,
            from: Point::new(5, 5),
            to: Point::new(6, 5),
        })
        .unwrap();

        let got: Vec<String> = log.borrow().clone();
        // -64 early, then seq order at NORMAL: normal (registered before normal2),
        // then LATE.
        assert_eq!(got, ["early", "normal", "normal2", "late"]);
    }

    // ---- 2. cancellation skips observers and leaves state unchanged --------

    #[test]
    fn cancel_skips_observers_and_mutates_nothing() {
        let mut ctx = context();
        let e = spawn_pos(&mut ctx, 5, 5);

        // A NORMAL mutator cancels the move.
        ctx.register::<Move>(NORMAL, |_ctx, _ev| HandlerAction::Cancel);
        let observer_hit = Rc::new(RefCell::new(0));
        {
            let o = observer_hit.clone();
            ctx.register_observer::<Move>(LATE, move |_ctx, _ev| {
                *o.borrow_mut() += 1;
            });
        }

        let outcome = ctx
            .dispatch(Move {
                actor: e,
                from: Point::new(5, 5),
                to: Point::new(6, 5),
            })
            .unwrap();

        assert!(matches!(outcome, EventOutcome::Cancelled { by: _ }));
        // No observer ran.
        assert_eq!(*observer_hit.borrow(), 0);
        // Position was not committed (Move::post_handled skipped on cancel).
        assert_eq!(
            ctx.position(e),
            Some(Position { x: 5, y: 5 }),
            "cancelled move must not change position"
        );
    }

    // ---- 3. modify semantics: ordering is load-bearing --------------------

    #[test]
    fn armor_chain_modifies_payload_and_order_flips_the_result() {
        // Damage cap of 5 makes ordering load-bearing: EARLY armor (subtract
        // 2) before the LATE cap yields 5; cap first then armor yields 3.
        let modify = |mut ctx: SimContext| {
            ctx.register::<TakeDamage>(EARLY, |_ctx, ev| {
                ev.amount -= 2;
                HandlerAction::Continue
            });
            ctx.register::<TakeDamage>(LATE, |_ctx, ev| {
                ev.amount = ev.amount.min(5);
                HandlerAction::Continue
            });
            ctx
        };
        let mut ctx = modify(context());
        let e = ctx
            .broker
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .spawn();
        let out = ctx
            .dispatch(TakeDamage::generic(e, 10, DamageSource::Attack))
            .unwrap();
        match out {
            EventOutcome::Modified(ev) => assert_eq!(ev.amount, 5, "10 - 2 = 8, capped at 5"),
            other => panic!("expected Modified, got {other:?}"),
        }

        // Same two effects, priorities swapped: cap first (EARLY), then armor
        // subtract (LATE) -> 10 capped to 5, then -2 = 3.
        let swapped = |mut ctx: SimContext| {
            ctx.register::<TakeDamage>(EARLY, |_ctx, ev| {
                ev.amount = ev.amount.min(5);
                HandlerAction::Continue
            });
            ctx.register::<TakeDamage>(LATE, |_ctx, ev| {
                ev.amount -= 2;
                HandlerAction::Continue
            });
            ctx
        };
        let mut ctx = swapped(context());
        let e = ctx
            .broker
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .spawn();
        let out = ctx
            .dispatch(TakeDamage::generic(e, 10, DamageSource::Attack))
            .unwrap();
        match out {
            EventOutcome::Modified(ev) => assert_eq!(ev.amount, 3, "cap first then -2"),
            other => panic!("expected Modified, got {other:?}"),
        }
    }

    // ---- 4. re-entrancy ---------------------------------------------------

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct EvA {
        who: Entity,
        go: bool,
    }
    impl Event for EvA {
        const NAME: &'static str = "EvA";
        fn entity(&self) -> Entity {
            self.who
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct EvB {
        who: Entity,
    }
    impl Event for EvB {
        const NAME: &'static str = "EvB";
        fn entity(&self) -> Entity {
            self.who
        }
    }

    #[test]
    fn direct_reentrancy_is_rejected() {
        let mut ctx = context();
        let e = ctx
            .broker
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .spawn();
        ctx.register::<EvA>(NORMAL, move |ctx, _ev| {
            // Same event type + same entity -> rejected at the nested call.
            let err = ctx.dispatch(EvA { who: e, go: false }).unwrap_err();
            assert!(matches!(err, EventError::ReentrantDispatch { .. }));
            HandlerAction::Continue
        });
        ctx.dispatch(EvA { who: e, go: true }).unwrap();
    }

    #[test]
    fn indirect_reentrancy_is_rejected() {
        let mut ctx = context();
        let e = ctx
            .broker
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .spawn();
        // A dispatches B (same entity); B dispatches A (same entity) -> A is
        // in flight, rejected.
        ctx.register::<EvA>(NORMAL, |ctx, ev| {
            let _ = ctx.dispatch(EvB { who: ev.who });
            HandlerAction::Continue
        });
        ctx.register::<EvB>(NORMAL, move |ctx, _ev| {
            let err = ctx.dispatch(EvA { who: e, go: false }).unwrap_err();
            assert!(matches!(err, EventError::ReentrantDispatch { .. }));
            HandlerAction::Continue
        });
        ctx.dispatch(EvA { who: e, go: true }).unwrap();
    }

    #[test]
    fn cross_entity_same_event_is_allowed() {
        let mut ctx = context();
        let x = ctx
            .broker
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .spawn();
        let y = ctx
            .broker
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .spawn();
        let hits = Rc::new(RefCell::new(0));
        {
            let h = hits.clone();
            ctx.register::<EvA>(NORMAL, move |ctx, ev| {
                *h.borrow_mut() += 1;
                if ev.who == x && ev.go {
                    // Dispatch A on a *different* entity: allowed.
                    ctx.dispatch(EvA { who: y, go: false }).unwrap();
                }
                HandlerAction::Continue
            });
        }
        ctx.dispatch(EvA { who: x, go: true }).unwrap();
        // Handler ran for x (go=true) and y (go=false) = 2 calls.
        assert_eq!(*hits.borrow(), 2);
    }

    // ---- 5. depth cap -----------------------------------------------------

    #[test]
    fn runaway_cross_event_cycle_stops_at_depth_cap_without_panicking() {
        let mut ctx = context();
        // A -> B (fresh entity), B -> A (fresh entity). Distinct pairs each
        // step so re-entrancy never fires; depth grows until the cap. Each
        // handler propagates a nested error up (Fail) so the *root* dispatch
        // surfaces DepthExceeded rather than swallowing it.
        let propagate_a = |ctx: &mut SimContext, _: &mut EvA| -> HandlerAction {
            let next = ctx
                .broker
                .write()
                .unwrap_or_else(|p| p.into_inner())
                .spawn();
            match ctx.dispatch(EvB { who: next }) {
                Ok(_) => HandlerAction::Continue,
                Err(e) => HandlerAction::Fail(e),
            }
        };
        let propagate_b = |ctx: &mut SimContext, _: &mut EvB| -> HandlerAction {
            let next = ctx
                .broker
                .write()
                .unwrap_or_else(|p| p.into_inner())
                .spawn();
            match ctx.dispatch(EvA {
                who: next,
                go: true,
            }) {
                Ok(_) => HandlerAction::Continue,
                Err(e) => HandlerAction::Fail(e),
            }
        };
        ctx.register::<EvA>(NORMAL, move |ctx, ev| propagate_a(ctx, ev));
        ctx.register::<EvB>(NORMAL, move |ctx, ev| propagate_b(ctx, ev));

        let root = ctx
            .broker
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .spawn();
        let err = ctx
            .dispatch(EvA {
                who: root,
                go: true,
            })
            .unwrap_err();
        assert!(
            matches!(err, EventError::DepthExceeded { max: 16 }),
            "got {err:?}"
        );
    }

    // ---- 6. snapshot isolation --------------------------------------------

    #[test]
    fn handler_registered_mid_dispatch_takes_effect_next_time() {
        let mut ctx = context();
        let e = spawn_pos(&mut ctx, 5, 5);

        let observed = Rc::new(RefCell::new(Vec::new()));
        // First handler: records whether the "second" handler ran, and if not,
        // registers it.
        let second = Rc::new(RefCell::new(false));
        {
            let observed = observed.clone();
            let second = second.clone();
            ctx.register::<Move>(EARLY, move |ctx, _ev| {
                observed.borrow_mut().push("early".to_string());
                if !*second.borrow() {
                    *second.borrow_mut() = true;
                    let observed = observed.clone();
                    ctx.register::<Move>(NORMAL, move |_ctx, _ev| {
                        observed.borrow_mut().push("late_reg".to_string());
                        HandlerAction::Continue
                    });
                }
                HandlerAction::Continue
            });
        }

        // First dispatch: only EARLY is registered; the NORMAL handler is
        // registered *during* it, so it must NOT run this pass.
        ctx.dispatch(Move {
            actor: e,
            from: Point::new(5, 5),
            to: Point::new(6, 5),
        })
        .unwrap();
        assert_eq!(*observed.borrow(), ["early"]);

        // Second dispatch: the mid-dispatch registration is now live.
        ctx.dispatch(Move {
            actor: e,
            from: Point::new(6, 5),
            to: Point::new(7, 5),
        })
        .unwrap();
        assert_eq!(*observed.borrow(), ["early", "early", "late_reg"]);
    }

    // ---- 7. replay determinism ----------------------------------------------

    #[test]
    fn same_scripted_sequence_replays_byte_identically() {
        let run = |seed: u64| -> Vec<String> {
            let mut ctx = SimContext::new(
                EntityBroker::new(),
                crate::zone::tests::demo_zone(20, 10),
                SimRng::from_seed(seed),
            );
            let e = spawn_pos(&mut ctx, 5, 5);
            let f = ctx
                .broker
                .write()
                .unwrap_or_else(|p| p.into_inner())
                .spawn();
            ctx.broker
                .write()
                .unwrap_or_else(|p| p.into_inner())
                .insert_part(f, PartTypeId::new(PART_HEALTH), Box::new(Health { hp: 10 }))
                .unwrap();

            // A deterministic observer logs each dispatched event name + amount.
            let log = log();
            let l = log.clone();
            ctx.register_observer::<Move>(LATE, move |_ctx, ev| {
                l.borrow_mut()
                    .push(format!("Move:{}->{}", ev.from.x, ev.to.x));
            });
            let l = log.clone();
            ctx.register_observer::<TakeDamage>(LATE, move |_ctx, ev| {
                l.borrow_mut().push(format!("TakeDamage:{}", ev.amount));
            });
            let l = log.clone();
            ctx.register_observer::<Attack>(LATE, move |_ctx, ev| {
                l.borrow_mut()
                    .push(format!("Attack:{:?}->{:?}", ev.attacker, ev.defender));
            });

            // Scripted sequence.
            let mut m = |actor: Entity, from: Point, to: Point| {
                let _ = ctx.dispatch(Move { actor, from, to });
            };
            m(e, Point::new(5, 5), Point::new(6, 5));
            m(e, Point::new(6, 5), Point::new(7, 5));
            ctx.dispatch(Attack {
                attacker: f,
                defender: e,
            })
            .unwrap();
            ctx.dispatch(TakeDamage::generic(e, 3, DamageSource::Attack))
                .unwrap();
            log.borrow().clone()
        };

        let a = run(42);
        let b = run(42);
        assert_eq!(a, b, "replay must be byte-identical for the same seed");
    }

    // ---- boundary errors ---------------------------------------------------

    #[test]
    fn unknown_entity_is_an_error() {
        let mut ctx = context();
        let err = ctx
            .dispatch(Move {
                actor: Entity::DANGLING,
                from: Point::new(0, 0),
                to: Point::new(1, 0),
            })
            .unwrap_err();
        assert!(matches!(err, EventError::UnknownEntity { .. }));
    }

    #[test]
    fn move_from_equal_to_is_invalid() {
        let mut ctx = context();
        let e = spawn_pos(&mut ctx, 5, 5);
        let err = ctx
            .dispatch(Move {
                actor: e,
                from: Point::new(5, 5),
                to: Point::new(5, 5),
            })
            .unwrap_err();
        assert!(
            matches!(err, EventError::InvalidPayload { ref msg, .. } if msg == "from == to"),
            "got {err:?}"
        );
    }

    #[test]
    fn negative_damage_is_invalid() {
        let mut ctx = context();
        let e = ctx
            .broker
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .spawn();
        let err = ctx
            .dispatch(TakeDamage::generic(e, -1, DamageSource::Attack))
            .unwrap_err();
        assert!(matches!(err, EventError::InvalidPayload { .. }));
    }

    // ---- occupancy classification (Caves-of-Qud bump) ---------------------

    /// Spawn a positioned entity optionally carrying a Health part (hp 0 means
    /// "dead/non-combat": present but not live).
    fn spawn_actor_at(ctx: &mut SimContext, x: i32, y: i32, hp: Option<i32>) -> Entity {
        let e = spawn_pos(ctx, x, y);
        if let Some(hp) = hp {
            ctx.broker
                .write()
                .unwrap_or_else(|p| p.into_inner())
                .insert_part(e, PartTypeId::new(PART_HEALTH), Box::new(Health { hp }))
                .unwrap();
        }
        e
    }

    #[test]
    fn empty_tile_is_clear() {
        let mut ctx = context();
        let mover = spawn_pos(&mut ctx, 5, 5);
        assert_eq!(ctx.tile_entry(Point::new(6, 5), mover), TileEntry::Clear);
    }

    #[test]
    fn live_health_actor_is_a_bump_target() {
        let mut ctx = context();
        let mover = spawn_pos(&mut ctx, 5, 5);
        let foe = spawn_actor_at(&mut ctx, 6, 5, Some(10));
        assert_eq!(
            ctx.tile_entry(Point::new(6, 5), mover),
            TileEntry::Bump(foe)
        );
        // The mover itself occupies its own tile: excluded, so not flagged.
        assert_eq!(ctx.tile_entry(Point::new(5, 5), mover), TileEntry::Clear);
    }

    #[test]
    fn actor_without_health_blocks() {
        let mut ctx = context();
        let mover = spawn_pos(&mut ctx, 5, 5);
        spawn_actor_at(&mut ctx, 6, 5, None); // Position only, no Health
        assert_eq!(ctx.tile_entry(Point::new(6, 5), mover), TileEntry::Blocked);
    }

    #[test]
    fn dead_actor_blocks_rather_than_bumps() {
        let mut ctx = context();
        let mover = spawn_pos(&mut ctx, 5, 5);
        spawn_actor_at(&mut ctx, 6, 5, Some(0)); // Health present but hp 0
        assert_eq!(
            ctx.tile_entry(Point::new(6, 5), mover),
            TileEntry::Blocked,
            "a non-live Health actor is a blocking non-combatant, not a bump target"
        );
    }

    #[test]
    fn successful_move_commits_position() {
        let mut ctx = context();
        let e = spawn_pos(&mut ctx, 5, 5);
        ctx.dispatch(Move {
            actor: e,
            from: Point::new(5, 5),
            to: Point::new(9, 3),
        })
        .unwrap();
        assert_eq!(ctx.position(e), Some(Position { x: 9, y: 3 }));
    }

    // ---- the attack pipeline ----------------------------------------------

    #[test]
    fn attack_cancels_without_health_part() {
        let mut ctx = context();
        crate::combat::install_combat(&mut ctx, crate::combat::CombatTuning::default());
        let a = ctx
            .broker
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .spawn();
        let d = spawn_pos(&mut ctx, 5, 5); // defender has Position, no Health

        let outcome = ctx
            .dispatch(Attack {
                attacker: a,
                defender: d,
            })
            .unwrap();
        assert!(matches!(outcome, EventOutcome::Cancelled { .. }));
    }

    #[test]
    fn attack_applies_takedamage_when_defender_has_health() {
        let mut ctx = context();
        crate::combat::install_combat(&mut ctx, crate::combat::CombatTuning::default());
        let a = ctx
            .broker
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .spawn();
        let d = ctx
            .broker
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .spawn();
        ctx.broker
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .insert_part(d, PartTypeId::new(PART_HEALTH), Box::new(Health { hp: 10 }))
            .unwrap();

        // The `BeforeHit` mutator hook settles the chance to hit, so the test
        // does not depend on a draw; an unarmed attacker deals its 1 point.
        ctx.register::<BeforeHit>(NORMAL, |_ctx, ev| {
            ev.hit_pct = 100;
            HandlerAction::Continue
        });

        // Observe the nested TakeDamage.
        let dmg = Rc::new(RefCell::new(None));
        {
            let dmg = dmg.clone();
            ctx.register_observer::<TakeDamage>(LATE, move |_ctx, ev| {
                *dmg.borrow_mut() = Some(ev.amount);
            });
        }
        let seen_hit = Rc::new(RefCell::new(false));
        {
            let seen_hit = seen_hit.clone();
            ctx.register_observer::<Hit>(LATE, move |_ctx, _ev| {
                *seen_hit.borrow_mut() = true;
            });
        }

        ctx.dispatch(Attack {
            attacker: a,
            defender: d,
        })
        .unwrap();
        assert_eq!(
            *dmg.borrow(),
            Some(1),
            "an unarmed landed hit deals its 1 generic point"
        );
        assert!(*seen_hit.borrow(), "a landed hit fires Hit before TakeDamage");
    }

    #[test]
    fn before_hit_cancel_vetoes_the_attack() {
        let mut ctx = context();
        crate::combat::install_combat(&mut ctx, crate::combat::CombatTuning::default());
        let a = ctx
            .broker
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .spawn();
        let d = spawn_actor_at(&mut ctx, 6, 5, Some(10));
        ctx.set_position(a, Position { x: 5, y: 5 });
        ctx.register::<BeforeHit>(NORMAL, |_ctx, _ev| HandlerAction::Cancel);
        let dmg = Rc::new(RefCell::new(0));
        {
            let dmg = dmg.clone();
            ctx.register_observer::<TakeDamage>(LATE, move |_ctx, _ev| {
                *dmg.borrow_mut() += 1;
            });
        }
        let outcome = ctx
            .dispatch(Attack {
                attacker: a,
                defender: d,
            })
            .unwrap();
        assert!(matches!(outcome, EventOutcome::Cancelled { .. }));
        assert_eq!(*dmg.borrow(), 0, "a vetoed attack deals no damage");
    }

    #[test]
    fn out_of_reach_attack_cancels_before_any_event() {
        let mut ctx = context();
        crate::combat::install_combat(&mut ctx, crate::combat::CombatTuning::default());
        let a = spawn_pos(&mut ctx, 1, 1);
        let d = spawn_actor_at(&mut ctx, 4, 1, Some(10)); // three cells away
        let fired = Rc::new(RefCell::new(0));
        {
            let fired = fired.clone();
            ctx.register_observer::<BeforeHit>(LATE, move |_ctx, _ev| {
                *fired.borrow_mut() += 1;
            });
        }
        let outcome = ctx
            .dispatch(Attack {
                attacker: a,
                defender: d,
            })
            .unwrap();
        assert!(matches!(outcome, EventOutcome::Cancelled { .. }));
        assert_eq!(*fired.borrow(), 0, "legality runs before BeforeHit");
    }

    // ---- named streams -----------------------------------------------------

    #[test]
    fn named_stream_forks_once_and_persists() {
        let mut ctx = context();
        let parent_before = ctx.rng.clone();
        // First use forks once from the parent...
        let _ = ctx.stream("combat").next_u32();
        let parent_after = ctx.rng.clone();
        assert_ne!(
            parent_before, parent_after,
            "the first use of a label draws once from the parent"
        );
        // ...and every later use of the same label draws no more parent state.
        let _ = ctx.stream("combat").next_u32();
        let _ = ctx.stream("combat").next_u32();
        assert_eq!(ctx.rng, parent_after, "later draws never touch the parent");
        // A different label is a different stream.
        let combat_next = ctx.stream("combat").next_u32();
        let other_next = ctx.stream("other").next_u32();
        assert_ne!(combat_next, other_next);
        assert_eq!(ctx.streams().len(), 2);
    }

    #[test]
    fn named_stream_round_trips_through_the_snapshot() {
        let mut ctx = context();
        let _ = ctx.stream("combat").below(100);
        let saved = ctx.streams().clone();
        let expected: Vec<u32> = (0..4).map(|_| ctx.stream("combat").next_u32()).collect();

        let mut restored = context();
        restored.restore_streams(saved);
        let got: Vec<u32> = (0..4)
            .map(|_| restored.stream("combat").next_u32())
            .collect();
        assert_eq!(got, expected, "a restored stream resumes exactly");
    }

    #[test]
    fn observer_cannot_see_cancelled_event() {
        let mut ctx = context();
        let e = spawn_pos(&mut ctx, 5, 5);
        ctx.register::<Move>(NORMAL, |_ctx, _ev| HandlerAction::Cancel);
        let hit = Rc::new(RefCell::new(0));
        {
            let h = hit.clone();
            // Register an observer BEFORE the cancelling mutator.
            ctx.register_observer::<Move>(EARLY, move |_ctx, _ev| {
                *h.borrow_mut() += 1;
            });
        }
        let _ = ctx.dispatch(Move {
            actor: e,
            from: Point::new(5, 5),
            to: Point::new(6, 5),
        });
        assert_eq!(*hit.borrow(), 0);
    }
}
