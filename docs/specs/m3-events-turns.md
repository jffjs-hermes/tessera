# M3 spec: typed events and the energy/turn scheduler

Status: research proposal for M3 (author: researcher bot, awaiting human approval).
Grounded in [architecture.md](../architecture.md) §2 (design principle 3), §5 ("A typed
bus with priorities and cancel/modify semantics", "Turn scheduler. Energy/speed-based
(as in Qud and Brogue)") and §8, and [determinism.md](../determinism.md). M3 scope per
architecture §8: *`Move`, `Attack`, `TakeDamage` events, speed-based scheduler,
headless simulation tests*.

External references used (verify against source at implementation time):

- Caves of Qud event model: official wiki, [Modding:Events](https://wiki.cavesofqud.com/wiki/Modding:Events)
  (FireEvent, Parts register listeners, events carry parameters, cancellation;
  "minimal"/pooled events and cascade levels for the optimization path).
- Brogue CE timing: [Brogue wiki "Timing"](https://brogue.fandom.com/wiki/Timing)
  (standard turn = 100 ticks; haste/slow halve/double; per-action delays; separate
  movement/attack delays per monster) and the DeepWiki mechanical summary of
  [tmewett/BrogueCE](https://deepwiki.com/tmewett/BrogueCE/3-game-mechanics)
  (`movementSpeed`/`attackSpeed` per creature, `ticksUntilTurn` set by action type).

---

## 1. Event bus design

### 1.1 Event representation: typed structs, not stringly-keyed bags

Qud's runtime event objects (`Event.New("TakeDamage", ...)` with string parameters)
exist because its parts are C# and events cross a serialization boundary. We have
static typing on the native side and Luau at M4, so:

```rust
// tessera-core/src/events.rs
pub trait Event: 'static {
    /// Stable name used by scripts (M4) and diagnostics, e.g. "TakeDamage".
    const NAME: &'static str;
    /// Handlers may mutate the event payload before it reaches the next handler.
    /// Returning `false` vetoes/cancels the event.
    fn handled(&mut self) -> bool { true }
}
```

Each event is a plain struct: `Move { actor: Entity, from: Point, to: Point }`,
`Attack { attacker: Entity, defender: Entity }`,
`TakeDamage { target: Entity, amount: i32, source: DamageSource }`.

Payload rules:

- Events reference entities by id, never by `&mut World` — handlers get their state
  access through a dispatch context (below). This is what allows re-entrancy control
  (§1.5) and keeps the bus free of borrows it cannot hold while iterating handlers.
- Handlers mutate the event *payload in place* (modify semantics) — `TakeDamage`
  handlers subtract armor, apply resistances, add `amount` — and can cancel.
- Cancellation is by `Result`: dispatch returns
  `Result<EventOutcome<T>, EventError>` where `EventOutcome` is
  `Handled` (ran to completion), `Cancelled { by: HandlerId }` or (for query-style
  events like `Move` that need a "modified" result) `Modified(T)`. A cancelled `Move`
  means "did not move"; the caller does not spend energy for a vetoed action (§2.6).
- Integer-only payloads (i32/u32) for gameplay-affecting values — no `f32` in event
  state, per determinism rule 5.

Extensibility: new event types are new structs implementing `Event`. There is no
global event enum to extend — mods/scripts (M4/M5) will register handlers for any
`NAME` they know; the typed core keeps native code safe, and the M4 scripting bridge
will adapt `Event::NAME` + a per-event `to_script_params`/`from_script_params` pair.
Specifying that adaptation seam now (each event type ships both) is what keeps M3
from being rewritten at M4.

### 1.2 Handler registration and ordering

Handlers live per-event-type in `EventBus`:

```rust
struct Handlers<E: Event> {
    // BTreeSet<HandlerKey>; HandlerKey = (priority: i8, seq: u64, handler: HandlerId)
}
```

- **Registration order is deterministic:** a monotonic `seq` counter breaks
  priority ties (registration order, not registration *content*), stored in a
  `BTreeSet` so no `HashMap` is involved (determinism rule 3). Handlers are the
  parts themselves (Qud-style: a part registers for events in its `register`
  step, which runs at spawn; unregistering happens at part removal/despawn).
- **Priorities are a small i8 range** with named constants:
  `EARLY = -64`, `NORMAL = 0`, `LATE = 64` (mid-values allowed for fine-tuning).
  This reserves headroom for mods to insert *before* or *after* base handlers
  without renumbering, the same reason Qud exposes listener priority.
- **Two handler shapes** (decided): *mutator* handlers (run by priority, may modify
  or cancel) and *observer* handlers (run after the mutator phase, in the same
  deterministic order, may not cancel). Most gameplay effects are mutators;
  logging, stats, and renderer hooks are observers. This mirrors the "before/after"
  split every evented roguelike ends up needing, without inventing a three-phase
  pipeline on day one.

### 1.3 Dispatch semantics

```rust
impl EventBus {
    /// Returns the post-dispatch event (possibly modified) or a cancel reason.
    pub fn dispatch<E: Event>(&mut self, ctx: &mut SimContext, ev: E) -> EventOutcome<E>;
}
```

- Dispatch order: handlers for `E` in `HandlerKey` order (priority, then `seq`).
  Fully deterministic, fully testable.
- A mutator cancelling (`Handled = false`) stops the mutator phase immediately;
  observers are skipped for cancelled events too (an observer on a vetoed `Move`
  should not record movement that did not happen). The `Cancelled` outcome records
  which handler cancelled for diagnostics.
- Handlers receive `&mut SimContext` (the broker + world + bus + forked rngs), not
  the event bus alone, so a handler can dispatch follow-up events.

### 1.4 Modify semantics concretely

`TakeDamage { target, amount, source }` walkthrough — this is the reference example
every other event copies:

1. `EARLY` handler on an armor part: `ev.amount -= 2`.
2. `NORMAL` handler on a poison effect: *cancels* if target is poison-immune, else
   adds a status (dispatching a nested `ApplyStatus` event — see re-entrancy).
3. `LATE` handler on `Health`: clamps `ev.amount` to `[0, health]` and applies it.
4. Observer on a stats part: logs the final number.

The applied damage is whatever the final payload says when the mutator phase
completes without cancellation. Amount is clamped at the *last* mutator, not
dispatch time, so ordering choices are meaningful and testable.

### 1.5 Nested / re-entrant dispatch

Problem: a handler of event `A` dispatches event `B` (e.g. `TakeDamage` handler
starts a `Death` event). Two failure modes exist: mutable aliasing (can't hold the
handler list across a nested dispatch) and infinite recursion (A's handler
dispatches A).

Policy (decided):

- **Nesting is allowed; a nested dispatch never runs handlers of an event type
  currently mid-dispatch on the same entity.** Implementation: the dispatch context
  carries a depth counter and an in-flight set of `(event_name, entity)` pairs;
  a re-entrant dispatch of an in-flight pair is *rejected* (see error behavior).
  This kills infinite loops cheaply and predictably; genuine chains (damage →
  death → corpse spawn) all use *different* event types, which is the normal shape
  of roguelike cascades and exactly how Qud's cascade levels behave.
- **Handler lists are snapshotted before iteration** (clone of the `Vec` of
  handler ids for that dispatch — cheap: ids, not closures), so a handler that
  registers/unregisters handlers mid-dispatch does not corrupt the in-flight
  iteration. Newly registered handlers take effect on the *next* dispatch.
- **Depth cap:** 16 nested dispatches. Exceeding it is an error (below), not a
  panic — a runaway cascade is a content bug and must be reported, not crash the
  sim.

### 1.6 Error / invalid-event behavior

- Dispatching an event for an entity that no longer exists: error
  (`EventError::UnknownEntity`), returned, never panicked.
- Re-entrancy rejection (§1.5): `EventError::ReentrantDispatch(name, entity)` —
  returned to the *caller of the nested dispatch*, which decides (handler code
  returns the error up; the outer dispatch surfaces it as `Err`). The outer event
  is *not* auto-cancelled; the error propagates.
- Depth cap exceeded: `EventError::DepthExceeded`.
- An event whose payload is internally invalid (negative `TakeDamage.amount`,
  `Move` from == to) is validated *before* dispatch by the bus; invalid →
  `EventError::InvalidPayload`. Fail loudly at the boundary, not deep in handlers.
- Unknown entity id inside a handler is the handler's bug: handlers get
  `ctx.broker.try_entity(id)` and must handle `None` (skip-and-log). The bus does
  not police handler internals.

### 1.7 Test strategy (events)

All headless, in `tessera-core` (no GPU deps), `SimRng` forked per test as needed:

1. **Ordering:** three handlers at priorities `LATE/NORMAL/EARLY` record their call
   order; assert ascending `(priority, seq)`. Two same-priority handlers assert
   registration order.
2. **Cancel:** handler at `NORMAL` cancels `Move`; assert outcome `Cancelled`, no
   `Position` change, no observer calls, caller pays no energy (§2.6).
3. **Modify:** armor chain from §1.4; assert final `amount` for a fixed armor
   ordering, then assert the *order flips the result* when priorities are swapped
   (documents that priority is load-bearing, not decorative).
4. **Re-entrancy:** handler of `A` dispatches `A` on the same entity →
   `ReentrantDispatch`; handler of `A` dispatches `B` which dispatches `A` on the
   same entity → also rejected (in-flight pair, not just direct recursion);
   handler of `A` on entity X dispatches `A` on entity Y → allowed.
5. **Depth cap:** a self-feeding cross-event cycle A→B→A on two entities hits
   `DepthExceeded` at 16, does not hang or panic.
6. **Snapshot isolation:** handler registers a new handler mid-dispatch; assert the
   current dispatch finishes with the old set and the next dispatch includes it.
7. **Determinism:** run the same scripted sequence of moves/attacks twice with the
   same seed; assert byte-identical event logs (see §3 demo output).

## 2. Energy/speed scheduler

### 2.1 Model choice: accumulate-and-threshold (Qud-style), not fixed delays (Brogue-style)

Two established shapes:

| | Brogue-style delays | Energy accumulation |
|---|---|---|
| Mechanism | after acting, `ticksUntilTurn = movementSpeed` (e.g. 100 ticks); global clock ticks down all actors ([Timing wiki](https://brogue.fandom.com/wiki/Timing)) | each actor gains `energy += speed * cost_multiplier` per tick; acts at `energy >= threshold` (Qud `Energy` part) |
| Pros | simple mental model; "how many ticks did this cost" is literal | per-action costs compose naturally with speed; no global tick-scan needed; partial energy persists across actions |
| Cons | mixing costs and speeds gets fiddly; needs the global countdown pass | must choose integer scaling carefully |

Recommendation: **energy accumulation with a global-tick drain**, integer-only:

- Every actor part has `speed: u32` (default 100 = "normal"). Qud/Brogue both
  anchor on 100 for a standard turn ([Timing wiki](https://brogue.fandom.com/wiki/Timing):
  "A standard player turn lasts 100 ticks"), so we keep that anchor.
- Global loop: `EnergyClock::tick()` adds `ACTOR_BASE_GAIN` (constant, chosen
  below) × `speed / 100` to every actor... — no; division loses determinism-safe
  granularity for speeds like 75. Instead: **`energy += speed` each tick; act when
  `energy >= 10000`**. 10000/100 = 100 ticks per standard turn, matching Brogue's
  100-tick anchor, and every speed is exactly represented with no rounding: speed
  75 gains 75/tick and acts every 133.33 ticks — same fractional behavior Brogue
  and Qud produce, computed with integers only (determinism rule 5).
- Action costs: each action declares a cost multiplier — move = 1×, attack = 1×
  (further weapons can be slower later, as Brogue's 200-tick mace swing is exactly
  cost 2×). Cost is charged as `energy -= 10000 * cost`. Haste/slow effects are
  simply speed modifiers on the part (`speed_effective = speed * 2 / 1` computed
  with integer `mul/div` helpers and documented rounding *down* — a slow creature
  slightly loses rather than slightly gains).

Algorithm (one full pass):

```
while sim has a scheduled actor:
    clock.tick():  for actor in actors_sorted_by_id: energy[i] += speed_effective(i)
    ready = actors with energy >= 10000
    if none: repeat tick (fast path: jump clock by floor((10000 - max_energy)/min_speed) to skip empty ticks)
    actor = ready.first()  # see tie-break §2.3
    dispatch ActorTurnStart(actor)            # observers, AI hook point (M6)
    outcome = dispatch the actor's action event (Move / Attack / Wait)
    if outcome was Cancelled: no energy charge (§2.6)
    else: energy[actor] -= 10000 * action.cost()
```

### 2.2 Speed edge cases (decided behavior)

- **speed = 0:** the actor gains 0 energy per tick and never acts. It is not an
  error (frozen statues, sleeping NPCs are legitimate). It must not stall the
  clock: the fast-path jump (§2.1) only uses `min_speed > 0` actors; if *no* actor
  has speed > 0, `tick()` returns `ClockStalled` and the scheduler returns an
  explicit "no actor can ever act" result rather than looping forever. Tests
  cover both.
- **speed > 100 (fast actors):** accumulate more per tick; an actor with speed 200
  acts twice per 100-tick window. With the fast-path jump this stays exact — energy
  is the state of record, never "skipped ticks".
- **speed changes mid-window** (buff applied during someone else's turn): takes
  effect on the *next* tick accumulation. No retroactive recompute. This is the
  natural consequence of "energy is the state of record" and needs no special rule.
- **negative energy** (charged more than it had — cost > accumulated): allowed;
  actor simply goes into energy debt and must accumulate back to 10000. This is
  how "slow but powerful action" emerges naturally.
- **speed overflow:** `energy` is `i64` (u32 speed × ticks fits easily); gains are
  capped per tick to prevent pathological content from overflowing i64 within a
  session — clamp `speed` content values to `<= 100_000` at part-load with a
  diagnostic.

### 2.3 Deterministic tie-breaking (no HashMap iteration anywhere)

- Actor set is a `BTreeMap<EntityId, ActorEnergy>`. Iteration order = entity id
  order — deterministic by construction (determinism rule 3).
- When ≥2 actors are ready in the same tick window, the one with the **lowest
  entity id** acts first. Rationale: entity ids are assigned in deterministic
  spawn order, so "oldest actor first" is stable, cheap, and never consults
  randomness. (A speed-then-id ordering would equally work; speed-first would make
  the fast actor act first in a burst, which is a *design* choice, not a
  correctness one — see open questions §4.)
- The scheduler never reads wall-clock time, thread scheduling, or `SimRng` for
  ordering. Randomness enters only via game logic during handlers (AI at M6),
  always through forked `SimRng` streams.

### 2.4 Test vectors (scheduler)

Headless integer tests, no rendering:

1. **Standard actor:** speed 100, one `Wait` per turn: acts exactly every 100
   ticks (energy 100×t reaches 10000 at t=100).
2. **Fast actor:** speed 200 acts at t=50, 100, 150 (interleaves 2:1 with the
   standard actor).
3. **Slow actor:** speed 50 acts at t=200 (once per two standard turns).
4. **Fractional composition:** speed 75 acts at t≈134 and never drifts: energy
   after t ticks is 75t; first act t=134 (75×134=10050 ≥ 10000); second at t=268
   (exact doubling). Assert no accumulation drift over 10_000 ticks vs. a
   precomputed table.
5. **Tie-break:** two actors (ids 1 and 2) both at threshold same tick: id 1 acts
   first; assert via event log.
6. **Zero speed:** actor with speed 0 never appears in ready set; sim with only
   speed-0 actors returns `ClockStalled`, terminates.
7. **Cost > energy:** speed 100 actor performs cost-2 action from exactly
   threshold energy → energy −10000, next act at t+200 (debt semantics).
8. **Replay determinism:** scripted 1000-tick run twice; identical event logs and
   final positions.

## 3. Headless crate boundary and the demo migration

### 3.1 Boundary

- All of §1 and §2 lives in `tessera-core` (already GPU/window-free; its
  `Cargo.toml` gains no new dependencies for M3). `tessera-render` and the
  `tessera` binary are the only windowing/GPU dependents; they consume the sim
  read-only, per determinism rule 6 and architecture §5.
- The scheduler and event bus are exercised by `cargo test` in `tessera-core`
  directly — no app run needed, no `winit`/`wgpu` in the test graph. CI's existing
  clippy `disallowed-types` guard applies to the new modules automatically.
- New public API surface is confined to `tessera-core::{events, turns}` (module
  names: `events.rs` for §1, `turns.rs` for §2).

### 3.2 Demo migration: `@` moves through the `Move` event

Today `main.rs` mutates `Position` directly on keypress (M1 shape). Migration:

- Input handling maps arrow keys → a `PlayerAction` (Move(dir) | Wait) and submits
  it to the simulation (`Sim::submit_action(player, action)`).
- The sim runs the scheduler until the player's turn comes up, then dispatches
  `Move { actor: player, from, to }`. A wall-blocked move is produced by an `EARLY`
  map-collision handler cancelling the event (this is the first content-level
  handler and proves the cancellation path in the shipped demo, not just tests).
- On `Handled`, the sim applies the position change in a `Move`'s
  post-handler commit step (the *bus* mutates `Position` after the mutator phase,
  so no handler can desync position from event outcome).
- **Demo controls (obligation):** arrow keys / WASD move the `@` (WASD added
  alongside the existing arrows), `.` or `Space` = Wait. Turn flow is
  player-input-driven: the sim advances only when the player acts (no autonomous
  monsters at M3 — AI is M6), so no new keys are needed for the scheduler itself.
- **Demo output (obligation):** the existing glyph grid shows the `@` moving;
  additionally the binary gains a `--headless <n>` flag: run `n` ticks/actions
  without a window, print the final map + event log to stdout, exit 0. This is the
  deterministic smoke check CI can run without a display and is the seed of the
  future replay format.
- Rendering stays a pure reader: it never dispatches events, never mutates sim
  state; the redraw on hot reload is unchanged.

## 4. Open decisions for Jeff (explicit, not guessed)

1. **Tie-break rule:** lowest-entity-id-first (proposed) vs. speed-then-id
   (fast actors win ties). Both are deterministic; it affects feel only when two
   actors cross the threshold in the same tick window, which is rare but visible
   in combat. **Needs Jeff's call.**
2. **Event-priority range:** proposed `i8` with named constants at ±64. Alternative:
   full `u32` priorities (Qud-style flexibility, uglier API). **Needs Jeff's call**;
   default is `i8`.
3. **Nested-dispatch rejection vs. queueing:** proposed *reject* re-entrant
   same-(event,entity) dispatches with a returned error. Alternative: queue the
   nested event to run after the outer dispatch completes (more forgiving, hides
   content bugs longer, more state to test). **Needs Jeff's call**; default is
   reject, because M4 scripts will make runaway cascades common and fail-loud is
   the cheaper habit to build.
4. **`Attack` at M3 scope:** the event exists and dispatches, but there is no
   damage/health content yet (stats parts are M2 data, not M3 behavior). Proposed:
   `Attack` cancels unless the defender has a `Health` part (added in M3 base data
   as `Health { hp: i32 }`); on hit, dispatches `TakeDamage { amount: 1 }` with no
   modifiers. Rich combat is explicitly later. **Needs Jeff's confirmation** that
   a minimal `Health` part + fixed 1 damage is acceptable M3 scope.

## 5. Implementation boundaries (for builder cards)

**In scope (M3):**
- `tessera-core::events`: `Event` trait, `EventBus`, handler registration
  (`BTreeSet` keys), priority constants, dispatch with snapshot isolation,
  re-entrancy/depth rules, `EventOutcome`/`EventError`.
- `tessera-core::turns`: `EnergyClock`, `Scheduler`, `ActorEnergy` part,
  speed clamp on load, tie-break, fast-path tick jump, `ClockStalled`.
- Events: `Move`, `Attack`, `TakeDamage` (+ internal `ActorTurnStart`, `Death`
  reserved), position commit after `Move` handled.
- Base data: `parts.Health = { hp = N }` on `Actor` blueprint; a simple wall map
  already exists from M1 (collision handler uses it).
- Demo: input → `Sim::submit_action`, wall-collision cancellation handler,
  WASD + Wait keys, `--headless <n>` mode with event-log output.
- Tests: §1.7 and §2.4 lists, plus the `--headless` CI smoke path.

**Out of scope (explicitly):**
- Luau/script handlers (M4); AI-driven actors and autonomous monster turns (M6);
  statuses/effects framework (later); mod registration of events (M5); inventory,
  armor content, real damage formulas (post-M7 roadmap).
- Multiple simultaneous player characters; real-time or interleaved modes.
- Saves/replays (M7) — but `--headless` event-log format should be chosen with
  replay reuse in mind (stable `Event::NAME` ordering, one event per line).

**Do not:** introduce `HashMap`/`HashSet` in sim code (clippy-enforced); use
floating point for energy/speed; let the renderer dispatch or mutate sim state;
make `tessera-core` depend on anything windowing/GPU; silently ignore cancelled
moves as if they cost energy.