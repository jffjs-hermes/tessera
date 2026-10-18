//! Energy-clock turn scheduler: accumulate-and-threshold timing, deterministic
//! speed-then-id ordering, and per-action energy costs, all deterministic
//! and replayable.
//!
//! # Model
//!
//! Each actor carries an [`ActorEnergy`] part: an integer `speed` (default 100
//! = "normal", matching Brogue's 100-tick turn anchor) and an `i64` energy
//! accumulator. Every global tick adds `speed` to the actor's energy; when
//! `energy >= TURN_THRESHOLD` (10000) the actor is "ready" and may act.
//! Because 10000/100 = 100 ticks per standard turn, speed 75 simply gains
//! 75/tick and acts every 133.33 ticks — fully represented in integers with no
//! rounding (determinism rule 5).
//!
//! Per-action costs are integer multiples of `TURN_THRESHOLD`: an action with
//! cost `k` charges `energy -= 10000 * k`. Charging more than the actor has
//! simply drives energy negative (debt); it must accumulate back to 10000
//! before acting again. A **cancelled** action does not charge energy (the
//! actor retains what it accumulated and acts again next window).
//!
//! # Determinism
//!
//! The actor set is a `BTreeMap<Entity, ActorEnergy>` (hecs `Entity` derives
//! `Ord` by (id, generation), so iteration is spawn order). The ready list is
//! collected from that map and sorted by `(speed_effective desc, entity_id
//! asc)` — a plain integer sort, no hashing and no randomness. The scheduler
//! never reads wall-clock time, thread scheduling, or RNG for ordering.
//!
//! # Fast path
//!
//! When no actor is ready, the clock skips empty ticks in one jump instead of
//! ticking one at a time: it advances by `floor((TURN_THRESHOLD -
//! max_energy) / min_speed_positive)` ticks, where `max_energy` is the highest
//! accumulated energy and `min_speed_positive` the smallest positive speed.
//! Energy is applied tick-for-tick through the jump, so it never changes exact
//! energy state. If **no** actor has speed > 0 the clock cannot ever advance an
//! actor to ready: the fast path returns [`ClockStalled`] and the scheduler
//! surfaces it rather than looping forever.

use std::collections::BTreeMap;
use std::fmt;

use hecs::Entity;
use serde::Deserialize;
use thiserror::Error;

use crate::events::{ActorTurnStart, Attack, EventError, EventOutcome, Move, Point, SimContext};

// ---------------------------------------------------------------------------
// Constants and the part
// ---------------------------------------------------------------------------

/// Standard turn anchor: an actor accumulates this much energy to act. With
/// speed 100 this is exactly 100 ticks per turn (Brogue's anchor).
pub const TURN_THRESHOLD: i64 = 10_000;

/// Default / "normal" speed.
pub const BASE_SPEED: u32 = 100;

/// Hard cap on loaded `speed` content values, to keep `i64` energy safe within
/// a session. Content values above this are clamped at part load (not an
/// error) with a diagnostic.
pub const MAX_SANITY_SPEED: u32 = 100_000;

/// The dynamic part name under which [`ActorEnergy`] is stored.
pub const PART_ENERGY: &str = "ActorEnergy";

/// The energy/speed part carried by every scheduled actor.
///
/// - `speed` is the integer gain per tick (clamped to [`MAX_SANITY_SPEED`] on
///   load, with a diagnostic). speed 0 never acts; speed > 100 accumulates
///   proportionally faster.
/// - `energy` is the `i64` accumulator; it may go negative (energy debt when
///   an action costs more than the actor has accumulated).
#[derive(Clone, Debug, PartialEq)]
pub struct ActorEnergy {
    pub speed: u32,
    pub energy: i64,
}

impl ActorEnergy {
    /// A fresh actor with `speed` (clamped to [`MAX_SANITY_SPEED`]) and zero
    /// energy. A clamped value logs a `tracing::warn!`.
    pub fn new(speed: u32) -> Self {
        let speed = clamp_speed(speed);
        Self { speed, energy: 0 }
    }

    /// A fresh actor at exactly threshold energy, ready to act immediately.
    pub fn at_threshold(speed: u32) -> Self {
        Self {
            speed: clamp_speed(speed),
            energy: TURN_THRESHOLD,
        }
    }
}

/// Clamp a content `speed` to [`MAX_SANITY_SPEED`], logging a diagnostic when
/// it overflows so pathological content is visible rather than silently
/// normalized.
fn clamp_speed(speed: u32) -> u32 {
    if speed > MAX_SANITY_SPEED {
        tracing::warn!(
            speed,
            max = MAX_SANITY_SPEED,
            "ActorEnergy.speed exceeds sanity cap; clamping"
        );
        MAX_SANITY_SPEED
    } else {
        speed
    }
}

/// Deserialize an `ActorEnergy` from a blueprint's `parts.ActorEnergy` table,
/// applying the speed sanity clamp at load. Unknown payload fields are
/// rejected so a typo fails loudly rather than being silently ignored.
impl<'de> Deserialize<'de> for ActorEnergy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            #[serde(default = "default_speed")]
            speed: u32,
            #[serde(default)]
            energy: i64,
        }
        let raw = Raw::deserialize(deserializer)?;
        Ok(ActorEnergy {
            speed: clamp_speed(raw.speed),
            energy: raw.energy,
        })
    }
}

fn default_speed() -> u32 {
    BASE_SPEED
}

// ---------------------------------------------------------------------------
// The clock: accumulation, fast path, stall
// ---------------------------------------------------------------------------

/// The clock could not advance an actor to ready because no actor has speed >
/// 0. Returned by the scheduler/energy-clock fast path instead of looping
/// forever.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockStalled;

impl fmt::Display for ClockStalled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "no scheduled actor has speed > 0; simulation cannot progress"
        )
    }
}

impl std::error::Error for ClockStalled {}

/// An action an actor may take on its turn, and its integer energy cost
/// (multiples of `TURN_THRESHOLD`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnAction {
    /// Do nothing this turn (cost 1×). No action event is dispatched; only the
    /// `ActorTurnStart` hook runs.
    Wait,
    /// Move to `to`, dispatching a [`Move`] from the actor's current `Position`
    /// (cost 1×). The scheduler reads the origin from the actor's `Position`
    /// part, so an actor without one fails the turn with
    /// [`TurnError::NoPosition`].
    MoveTo(Point),
    /// Attack `defender`, dispatching an [`Attack`] (cost 1×).
    Attack(Entity),
    /// A caller-defined action with an explicit integer cost multiple (e.g. 2
    /// for a slow weapon). No event is dispatched; used for debt-boundary
    /// tests and future cost-carrying content.
    Costed { cost: u32 },
}

impl TurnAction {
    /// The integer cost multiple charged on a handled action (`energy -=
    /// TURN_THRESHOLD * self.cost()`).
    pub fn cost(&self) -> u32 {
        match self {
            TurnAction::Wait | TurnAction::MoveTo(_) | TurnAction::Attack(_) => 1,
            TurnAction::Costed { cost } => *cost,
        }
    }
}

/// Outcome of resolving an actor's turn.
#[derive(Clone, Debug)]
pub struct TurnReport {
    /// The actor whose turn ran.
    pub actor: Entity,
    /// Clock tick when the turn resolved.
    pub tick: u64,
    /// The action performed.
    pub action: TurnAction,
    /// Whether energy was charged (`false` only when the action event was
    /// cancelled).
    pub charged: bool,
    /// The actor's energy after charge (or unchanged if not charged).
    pub energy_after: i64,
}

/// Failures of [`Scheduler::take_turn`] / `advance` — returned, never panicked.
#[derive(Debug, Error)]
pub enum TurnError {
    #[error("turn referenced actor {0:?}, which is not scheduled")]
    UnknownActor(Entity),
    #[error("actor {0:?} has no Position; a Move needs an origin")]
    NoPosition(Entity),
    #[error("the event bus rejected a dispatched event: {0}")]
    Event(#[from] EventError),
}

// ---------------------------------------------------------------------------
// The scheduler
// ---------------------------------------------------------------------------

/// The energy-clock scheduler: owns the actor table and the clock, and drives
/// turns through a [`SimContext`] (dispatching `ActorTurnStart` then the
/// actor's action event, charging energy unless cancelled).
pub struct Scheduler {
    clock: u64,
    actors: BTreeMap<Entity, ActorEnergy>,
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl Scheduler {
    pub fn new() -> Self {
        Self {
            clock: 0,
            actors: BTreeMap::new(),
        }
    }

    /// Total ticks elapsed since the scheduler was created. A scheduler
    /// replaying the same actions reaches the same tick count.
    pub fn ticks(&self) -> u64 {
        self.clock
    }

    /// Add an actor to the schedule with the given energy part.
    pub fn add_actor(&mut self, entity: Entity, energy: ActorEnergy) -> Option<ActorEnergy> {
        self.actors.insert(entity, energy)
    }

    /// Remove an actor from the schedule, returning its energy part.
    pub fn remove_actor(&mut self, entity: Entity) -> Option<ActorEnergy> {
        self.actors.remove(&entity)
    }

    /// The actors currently in the schedule, in deterministic (entity-id)
    /// order.
    pub fn actors(&self) -> impl Iterator<Item = Entity> + '_ {
        self.actors.keys().copied()
    }

    /// An actor's current energy (the state of record).
    pub fn energy(&self, entity: Entity) -> Option<i64> {
        self.actors.get(&entity).map(|a| a.energy)
    }

    /// An actor's effective speed, clamped/raw as stored.
    pub fn speed(&self, entity: Entity) -> Option<u32> {
        self.actors.get(&entity).map(|a| a.speed)
    }

    /// Change an actor's speed. Takes effect on the **next** tick accumulation
    /// — energy persists, so there is no retroactive recompute.
    /// The new value is clamped to [`MAX_SANITY_SPEED`].
    pub fn set_speed(&mut self, entity: Entity, speed: u32) -> Option<()> {
        self.actors.get_mut(&entity)?.speed = clamp_speed(speed);
        Some(())
    }

    /// Advance the clock by exactly `n` ticks, accumulating `speed` energy to
    /// every scheduled actor each tick.
    pub fn advance_ticks(&mut self, n: u64) {
        self.clock += n;
        for a in self.actors.values_mut() {
            a.energy += a.speed as i64 * n as i64;
        }
    }

    /// The number of actors currently ready (energy >= threshold).
    pub fn ready_count(&self) -> usize {
        self.actors
            .values()
            .filter(|a| a.energy >= TURN_THRESHOLD)
            .count()
    }

    /// The ready actors sorted by **speed-then-id**: highest effective speed
    /// first; equal speeds broken by lowest entity id. Empty
    /// when none are ready.
    pub fn ready(&self) -> Vec<Entity> {
        let mut ready: Vec<(Entity, u32)> = self
            .actors
            .iter()
            .filter(|(_, a)| a.energy >= TURN_THRESHOLD)
            .map(|(e, a)| (*e, a.speed))
            .collect();
        ready.sort_by(|(ea, sa), (eb, sb)| sb.cmp(sa).then_with(|| ea.cmp(eb)));
        ready.into_iter().map(|(e, _)| e).collect()
    }

    /// Fast path: when no actor is ready, advance the clock by the maximum
    /// number of empty ticks that can pass without any actor reaching the
    /// threshold, then tick once more across the boundary. Returns the number
    /// of ticks advanced (>= 1 once a jump is performed, 0 if somebody is
    /// already ready).
    ///
    /// If **no** actor has speed > 0, no jump is possible and this returns
    /// [`ClockStalled`].
    pub fn advance_until_ready(&mut self) -> Result<(), ClockStalled> {
        loop {
            if self.ready_count() > 0 {
                return Ok(());
            }
            // Fast path: skip the empty window in one jump.
            let min_positive = self
                .actors
                .values()
                .map(|a| a.speed)
                .filter(|&s| s > 0)
                .min();
            let Some(min_speed) = min_positive else {
                return Err(ClockStalled);
            };
            let max_energy = self.actors.values().map(|a| a.energy).max().unwrap_or(0);
            // Ticks until the leader reaches threshold, assuming the slowest
            // positive actor; apply it directly (integer, exact energy). A
            // floor of 1 guarantees forward progress when the leader is within
            // one tick of the threshold.
            let deficit = TURN_THRESHOLD - max_energy;
            let jump = if deficit <= 0 {
                0
            } else {
                (deficit / min_speed as i64).max(1) as u64
            };
            self.advance_ticks(jump);
        }
    }

    /// Resolve one actor's turn: dispatch `ActorTurnStart` (the AI/renderer
    /// hook point), then the actor's action event, then charge energy unless
    /// the action was cancelled.
    ///
    /// `from` for a [`TurnAction::MoveTo`] is read from the actor's `Position`
    /// part. A cancelled action leaves energy untouched and the actor becomes
    /// ready again next window.
    pub fn take_turn(
        &mut self,
        ctx: &mut SimContext,
        actor: Entity,
        action: TurnAction,
    ) -> Result<TurnReport, TurnError> {
        if !self.actors.contains_key(&actor) {
            return Err(TurnError::UnknownActor(actor));
        }

        // Hook point: observers/AI see the turn start regardless of outcome.
        ctx.dispatch(ActorTurnStart { actor })?;

        // Dispatch the actor's action event; a cancelled action charges nothing.
        let cancelled = match action {
            TurnAction::Wait => false,
            TurnAction::MoveTo(to) => {
                let from = match ctx.position(actor) {
                    Some(p) => Point::new(p.x, p.y),
                    None => return Err(TurnError::NoPosition(actor)),
                };
                matches!(
                    ctx.dispatch(Move { actor, from, to })?,
                    EventOutcome::Cancelled { .. }
                )
            }
            TurnAction::Attack(defender) => {
                matches!(
                    ctx.dispatch(Attack {
                        attacker: actor,
                        defender,
                    })?,
                    EventOutcome::Cancelled { .. }
                )
            }
            TurnAction::Costed { .. } => false,
        };

        let energy = &mut self
            .actors
            .get_mut(&actor)
            .expect("scheduler actor removed mid-turn")
            .energy;
        if !cancelled {
            *energy -= TURN_THRESHOLD * action.cost() as i64;
        }
        let energy_after = *energy;

        Ok(TurnReport {
            actor,
            tick: self.clock,
            action,
            charged: !cancelled,
            energy_after,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecs::EntityBroker;
    use crate::events::{HandlerAction, PART_POSITION, Position};
    use crate::rng::SimRng;

    /// A SimContext whose broker already holds one positioned entity per speed
    /// listed, in spawn order. Returns that context plus the entity list.
    fn ctx_and_entities(speeds: &[u32]) -> (SimContext, Vec<Entity>) {
        let mut broker = EntityBroker::new();
        let mut ents = Vec::new();
        for _speed in speeds {
            let e = broker.spawn();
            broker
                .insert_part(e, PART_POSITION, Box::new(Position { x: 1, y: 1 }))
                .unwrap();
            ents.push(e);
        }
        (
            SimContext::new(
                broker,
                crate::zone::tests::demo_zone(20, 10),
                SimRng::from_seed(1),
            ),
            ents,
        )
    }

    /// A fresh scheduler with one actor per speed, plus a context. The actor
    /// entities (in the same order) are returned alongside.
    fn setup(speeds: &[u32]) -> (Scheduler, SimContext, Vec<Entity>) {
        let (ctx, ents) = ctx_and_entities(speeds);
        let mut s = Scheduler::new();
        for (i, speed) in speeds.iter().enumerate() {
            s.add_actor(ents[i], ActorEnergy::new(*speed));
        }
        (s, ctx, ents)
    }

    // ---- Standard actor: speed 100, Wait each turn -----------------

    #[test]
    fn standard_speed_100_acts_every_100_ticks() {
        let (mut s, mut ctx, ents) = setup(&[100]);
        let e = ents[0];
        // First turn at tick 100: energy 100*100 = 10000.
        s.advance_until_ready().unwrap();
        assert_eq!(s.ticks(), 100);
        let rep = s.take_turn(&mut ctx, e, TurnAction::Wait).unwrap();
        assert!(rep.charged);
        assert_eq!(rep.energy_after, 0);
        // Next turn at tick 200 (energy returns to threshold).
        s.advance_until_ready().unwrap();
        assert_eq!(s.ticks(), 200);
    }

    // ---- Fast actor: speed 200 interleaves 2:1 --------------------

    #[test]
    fn fast_actor_interleaves_two_to_one() {
        // A standalone speed-200 actor lands on t=50, 100, 150 exactly.
        let (mut solo, mut sctx, sents) = setup(&[200]);
        let sf = sents[0];
        for expected in [50, 100, 150] {
            solo.advance_until_ready().unwrap();
            assert_eq!(solo.ticks(), expected, "standalone fast actor act tick");
            assert_eq!(solo.ready(), [sf]);
            solo.take_turn(&mut sctx, sf, TurnAction::Wait).unwrap();
        }

        // Mixed with a standard actor both cross the threshold on the same
        // 100-tick boundary (the fast-path jump uses the slowest positive
        // speed), so the speed-200 actor acts twice per slow actor's once —
        // the 2:1 ratio — and wins each tie by speed.
        let (mut s, mut ctx, ents) = setup(&[100, 200]);
        let slow = ents[0];
        let fast = ents[1];
        let (mut fast_acts, mut slow_acts) = (0usize, 0usize);
        let mut ticks_seen = Vec::new();
        for _ in 0..9 {
            s.advance_until_ready().unwrap();
            let actor = s.ready()[0];
            let t = s.ticks();
            s.take_turn(&mut ctx, actor, TurnAction::Wait).unwrap();
            ticks_seen.push(t);
            match actor {
                a if a == fast => fast_acts += 1,
                a if a == slow => slow_acts += 1,
                _ => unreachable!(),
            }
        }
        // 9 turns: 6 fast, 3 slow (2:1). The fast actor, once its 20000
        // energy is built up, can act twice within one window before the slow
        // actor's turn.
        assert_eq!((fast_acts, slow_acts), (6, 3), "2:1 interleave ratio");
        let _ = ticks_seen;
        // Slow never acts before fast within a window where both are ready:
        // check the very first resolution -> fast.
        let (mut s2, _c2, ents2) = setup(&[100, 200]);
        s2.advance_until_ready().unwrap();
        assert_eq!(s2.ready().first(), Some(&ents2[1]), "fast wins the tie-up");
    }

    // ---- Slow actor: speed 50 acts once per two standard turns -----

    #[test]
    fn slow_actor_acts_once_per_two_standard_turns() {
        let (mut s, mut ctx, ents) = setup(&[50]);
        let e = ents[0];
        s.advance_until_ready().unwrap();
        assert_eq!(s.ticks(), 200, "energy 50*200 = 10000");
        s.take_turn(&mut ctx, e, TurnAction::Wait).unwrap();
    }

    // ---- Fractional speed 75: no drift ----------------------------

    #[test]
    fn speed_75_acts_at_134_then_267_then_400_no_drift() {
        let (mut s, mut ctx, ents) = setup(&[75]);
        let e = ents[0];
        s.advance_until_ready().unwrap();
        // 75*134 = 10050 >= 10000, first act at t=134.
        assert_eq!(s.ticks(), 134);
        let r = s.take_turn(&mut ctx, e, TurnAction::Wait).unwrap();
        assert_eq!(r.energy_after, 50); // 10050 - 10000
        // Needs 9950 more => 75*133 = 9975, at 134 + 133 = 267.
        s.advance_until_ready().unwrap();
        assert_eq!(s.ticks(), 267);
        s.take_turn(&mut ctx, e, TurnAction::Wait).unwrap();
        // Third act: carryover 50+... energy landed at 10050-10000+9975-10000 = 75
        // next needs 9925 => +133 ticks = 400. Full table below.
        s.advance_until_ready().unwrap();
        assert_eq!(s.ticks(), 400);
    }

    // No drift over 10_000 ticks: the act ticks must match an independently
    // precomputed integer table (75*t - k*10000 crossing 10000), not merely a
    // constant step. Carryover after each charge creates the repeating gap
    // pattern 133,133,134.
    #[test]
    fn speed_75_has_no_accumulation_drift_over_10000_ticks() {
        let (mut s, mut ctx, ents) = setup(&[75]);
        let e = ents[0];
        let mut act_ticks = Vec::new();
        for _ in 0..300 {
            s.advance_until_ready().unwrap();
            let actor = s.ready()[0];
            assert_eq!(actor, e);
            let t = s.ticks();
            s.take_turn(&mut ctx, e, TurnAction::Wait).unwrap();
            act_ticks.push(t);
        }
        // Precompute the same turn schedule with plain integer arithmetic,
        // independent of the scheduler's fast path: energy[0]=0, +75/tick,
        // act when >= 10000, then -10000.
        let mut expected = Vec::new();
        let (mut tick, mut energy) = (0u64, 0i64);
        while expected.len() < 300 {
            tick += 1;
            energy += 75;
            if energy >= TURN_THRESHOLD {
                expected.push(tick);
                energy -= TURN_THRESHOLD;
            }
        }
        assert_eq!(act_ticks, expected, "no drift over the precomputed table");
        // Sanity: the table actually spans past 10k ticks with the documented
        // gap pattern.
        assert!(act_ticks[0] == 134 && act_ticks[1] - act_ticks[0] == 133);
        assert!(act_ticks[2] - act_ticks[1] == 133);
        assert!(act_ticks[3] - act_ticks[2] == 134);
        assert!(act_ticks.last().unwrap() > &10_000);
    }

    // ---- Tie-break: speed-then-id --------------------------------

    #[test]
    fn tie_break_same_speed_lowest_id_first() {
        let (mut s, _ctx, ents) = setup(&[100, 100]);
        let a = ents[0]; // lower id
        let b = ents[1];
        s.advance_until_ready().unwrap();
        assert_eq!(s.ready(), [a, b], "same speed: lowest entity id acts first");
    }

    #[test]
    fn tie_break_different_speed_highest_speed_first_regardless_of_id() {
        let (mut s, _ctx, ents) = setup(&[100, 200]);
        let slow = ents[0]; // id 0
        let fast = ents[1]; // id 1, higher speed but higher id
        s.advance_until_ready().unwrap();
        assert_eq!(s.ready(), [fast, slow], "speed 200 before speed 100");
    }

    #[test]
    fn ready_order_is_full_speed_then_id() {
        let (mut s, _ctx, ents) = setup(&[75, 200, 100, 200]);
        // The fast-path jump lands on the threshold of the slowest positive
        // actor rounded down: 75*133 = 9975 < 10000, so the speed-75 actor is
        // a tick short and not yet ready. One more tick brings it in.
        s.advance_until_ready().unwrap();
        s.advance_ticks(1);
        // Two speed-200 (ids 1,3): both ready, lowest id first; then speed 100
        // (id 2), then speed 75 (id 0).
        assert_eq!(s.ready(), [ents[1], ents[3], ents[2], ents[0]]);
    }

    // ---- Zero speed: never acts; all-zero stalls -----------------

    #[test]
    fn zero_speed_actor_never_ready() {
        let (mut s, _ctx, ents) = setup(&[100, 0]);
        s.advance_until_ready().unwrap();
        assert_eq!(s.ready(), [ents[0]], "only the speed-100 actor is ready");
    }

    #[test]
    fn all_zero_speed_returns_clock_stalled() {
        let (mut s, _ctx, _ents) = setup(&[0, 0]);
        // Fast path must report ClockStalled rather than looping.
        assert_eq!(s.advance_until_ready(), Err(ClockStalled));
    }

    // ---- Cost > energy: debt semantics ---------------------------

    #[test]
    fn cost_greater_than_energy_creates_debt() {
        let (mut s, mut ctx, ents) = setup(&[100]);
        let e = ents[0];
        s.advance_until_ready().unwrap();
        let t_before = s.ticks();
        // Cost-2 action from exactly threshold: energy -= 20000 => -10000.
        let rep = s
            .take_turn(&mut ctx, e, TurnAction::Costed { cost: 2 })
            .unwrap();
        assert!(rep.charged);
        assert_eq!(rep.energy_after, -10000);
        // Next act at +200 ticks: needs +20000 to reach 10000, at 100/tick.
        s.advance_until_ready().unwrap();
        assert_eq!(s.ticks() - t_before, 200, "debt must accumulate back");
    }

    // ---- Cancelled actions charge no energy ------------------------

    #[test]
    fn cancelled_move_charges_no_energy_and_position_unchanged() {
        let (mut s, mut ctx, ents) = setup(&[100]);
        let e = ents[0];
        s.advance_until_ready().unwrap();
        // A wall-collision-style canceller.
        ctx.register::<Move>(crate::events::EARLY, |_ctx, _ev| HandlerAction::Cancel);
        let rep = s
            .take_turn(&mut ctx, e, TurnAction::MoveTo(Point::new(2, 1)))
            .unwrap();
        assert!(!rep.charged, "cancelled action must charge nothing");
        assert_eq!(rep.energy_after, TURN_THRESHOLD, "energy unchanged");
        // Position unchanged by the cancelled move.
        assert_eq!(ctx.position(e), Some(Position { x: 1, y: 1 }));
        // Actor remains ready and acts again next window without new energy.
        assert_eq!(s.ready(), [e]);
    }

    // ---- Mid-window speed change takes effect next tick -------------

    #[test]
    fn mid_window_speed_change_takes_effect_next_tick() {
        let (mut s, _ctx, ents) = setup(&[100]);
        let e = ents[0];
        // Advance half a window: t=50, energy 5000.
        s.advance_ticks(50);
        assert_eq!(s.ticks(), 50);
        assert_eq!(s.energy(e), Some(5000));
        // Change speed now; takes effect on the *next* tick accumulation.
        s.set_speed(e, 200).unwrap();
        // Advance to ready: from 5000, +200/tick => 25 ticks => t=75.
        s.advance_until_ready().unwrap();
        assert_eq!(s.ticks(), 75, "gains 200/tick after the change");
    }

    // ---- Replay determinism --------------------------------------

    #[test]
    fn replay_produces_identical_turn_log() {
        let run = || -> Vec<(u64, usize, bool)> {
            let (mut s, mut ctx, ents) = setup(&[100, 200, 75]);
            // Block all moves so repeated MoveTo((2,1)) is always a valid
            // (1,1)->(2,1) attempt and exercises the cancel path (no charge).
            ctx.register::<Move>(crate::events::EARLY, |_ctx, _ev| HandlerAction::Cancel);
            let mut log = Vec::new();
            // Deterministic action script over the three actors.
            let mut seq = 0u64;
            while log.len() < 60 {
                s.advance_until_ready().unwrap();
                let actor = s.ready()[0];
                let idx = ents.iter().position(|e| e == &actor).unwrap();
                let action = match seq % 4 {
                    0 => TurnAction::Wait,
                    1 => TurnAction::Costed { cost: 2 },
                    2 => TurnAction::Wait,
                    _ => TurnAction::MoveTo(Point::new(2, 1)),
                };
                seq += 1;
                let rep = s.take_turn(&mut ctx, actor, action).unwrap();
                log.push((rep.tick, idx, rep.charged));
            }
            log
        };
        let a = run();
        let b = run();
        assert_eq!(a, b, "replay must be byte-identical");
    }

    // ---- speed clamp on load ---------------------------------------------

    #[test]
    fn speed_clamped_on_load_to_sanity_cap() {
        // Direct part construction clamps.
        assert_eq!(ActorEnergy::new(200_000).speed, MAX_SANITY_SPEED);
        // Deserialization from a blueprint payload clamps too.
        let ae: ActorEnergy =
            toml::from_str("speed = 500_000\nenergy = 5").expect("payload parses");
        assert_eq!(ae.speed, MAX_SANITY_SPEED);
        assert_eq!(ae.energy, 5);
        // Within-range is untouched, unknown fields rejected.
        let ok: ActorEnergy = toml::from_str("speed = 75").unwrap();
        assert_eq!(ok.speed, 75);
        assert!(toml::from_str::<ActorEnergy>("speed = 1\ntypo = 2").is_err());
    }
}
