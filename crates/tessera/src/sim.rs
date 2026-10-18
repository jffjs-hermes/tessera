//! Demo simulation harness (energy-clock, deterministic event bus).
//!
//! Wires the core event bus and energy scheduler into the demo: the player is
//! the single scheduled actor, keyboard input is converted to a
//! [`PlayerAction`] and submitted to [`Sim::submit_action`], which runs the
//! scheduler until the player's turn then dispatches the action event through
//! the bus. A wall-collision `EARLY` handler on [`Move`] cancels any move
//! whose target is not walkable — the first content-level handler in the
//! shipped demo, and the path that proves event cancellation outside tests.
//!
//! The renderer is a strict reader: [`Sim::text_map`] / `build_cells` only
//! *read* sim state (map + player position); they never dispatch events or
//! mutate state. Redraw on hot reload is unchanged.
//!
//! # Headless
//!
//! [`Sim::run_headless`] advances the sim through `n` scripted actions with no
//! window and returns the final map + a stable, one-event-per-line event log
//! (KEY <NAME> tick=.. [fields]) suitable as the seed of the future replay
//! format. Deterministic for a fixed script and map (the sim consumes no
//! randomness — no AI, no f32, integer-only energy).

use std::sync::{Arc, Mutex, RwLock};

use hecs::Entity;
use tessera_core::ecs::EntityBroker;
use tessera_core::events::{
    EARLY, HandlerAction, LATE, Move, NORMAL, Point, SimContext, TakeDamage,
};
use tessera_core::rng::SimRng;
use tessera_core::tiles::TileRegistry;
use tessera_core::turns::{
    ActorEnergy, BASE_SPEED, PART_ENERGY, Scheduler, TurnAction, TurnReport,
};
use tessera_core::world::RenderTile;
use tessera_core::zone::Zone;

use crate::scripting::{ScriptBridge, entity_to_id, int_params};

/// Deterministic seed for the demo's forked RNG streams. The sim never reads
/// randomness (no AI), so this keeps headless output identical run to
/// run for a fixed script.
const SIM_SEED: u64 = 20_260_317;

/// An action the player can submit to the simulation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayerAction {
    /// Try to move one cell in a direction (dx, dy ∈ {-1, 0, 1}).
    Move { dx: i32, dy: i32 },
    /// The scripted `Zap` (E): dispatch `Attack` toward the currently faced
    /// direction. A wall/no-target cell is a no-op (the attack cancels or the
    /// scripted handler vetoes when out of charges).
    Zap,
    /// Do nothing this turn.
    Wait,
}

/// The demo simulation harness: entity broker + map + rng + typed event bus +
/// energy scheduler, plus the deterministic event log.
pub struct Sim {
    ctx: SimContext,
    /// The energy-clock scheduler, wrapped in the shared handle so scripted
    /// `spawn_at` / `despawn` register/remove actors on the *same* clock the
    /// sim drives.
    scheduler: Arc<Mutex<Scheduler>>,
    player: Entity,
    /// Script host + world-access bridge sharing `ctx`'s broker
    /// (one-dispatch-path). When present, `take_turn` forwards the actor's
    /// scripted events to the host so `engine.on` handlers share the dispatch.
    bridge: Option<ScriptBridge>,
    /// Optional AI-controlled enemy (the demo Zapper) scheduled alongside the
    /// player, attacking the player on its turns.
    enemy: Option<Entity>,
    /// Last facing direction (dx, dy), for the E=Zap scripted action.
    facing: (i32, i32),
    log: Vec<String>,
    /// Player-turn clock: increments once per submitted player action
    /// (cancelled or not). Exposed to scripts via `engine.player_turns()`.
    player_turns: u64,
}

impl Sim {
    /// Build the harness around an already-spawned `player` whose broker
    /// carries a `Position` part (no script host / no enemy).
    #[cfg_attr(not(test), allow(dead_code))] // used by the sim test fixture
    pub fn new(broker: EntityBroker, zone: Zone, player: Entity) -> Self {
        let broker = Arc::new(RwLock::new(broker));
        Self::from_shared(broker, None, zone, player, None)
    }

    /// Build the harness around a *shared* broker handle (the same one a
    /// [`ScriptBridge`] was created over, so native handlers and script
    /// handlers observe one live broker). Optionally installs the reference
    /// demo combat (base `TakeDamage` → `Health.hp` reduction), the script
    /// bridge forwarding, and an enemy actor that attacks the player.
    pub fn from_shared(
        broker: Arc<RwLock<EntityBroker>>,
        bridge: Option<ScriptBridge>,
        zone: Zone,
        player: Entity,
        enemy: Option<Entity>,
    ) -> Self {
        let mut ctx = SimContext::shared(broker.clone(), zone, SimRng::from_seed(SIM_SEED));
        // Content-level wall collision: cancel any Move whose target is not
        // walkable. This is the shipped cancellation handler.
        ctx.register::<Move>(EARLY, |ctx, ev| {
            if ctx.map.is_walkable(ev.to.x, ev.to.y) {
                HandlerAction::Continue
            } else {
                HandlerAction::Cancel
            }
        });
        // Reference demo combat: an Attack on a Health-carrying defender
        // resolves to a base `TakeDamage`, and a TakeDamage always reduces
        // `Health.hp`. This is the native "base hit" the Zapper's scripted zap
        // stacks on top of.
        install_base_combat(&mut ctx);
        // Forward the reserved `Death` event (dispatched by the native
        // TakeDamage applicator when hp clamps to zero) to the script host, so
        // scripts can react with respawn/policy (the turn hook contract).
        if let Some(b) = &bridge {
            let fwd = b.clone();
            ctx.register_observer::<tessera_core::events::Death>(LATE, move |_ctx, ev| {
                fwd.forward_death(entity_to_id(ev.target));
            });
        }

        let speed = ctx
            .broker
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .typed_part::<ActorEnergy>(player, &PART_ENERGY.into())
            .map(|a| a.speed)
            .unwrap_or(BASE_SPEED);
        let mut scheduler = Scheduler::new();
        scheduler.add_actor(player, ActorEnergy::at_threshold(speed));
        if let Some(e) = enemy {
            let espeed = ctx
                .broker
                .read()
                .unwrap_or_else(|p| p.into_inner())
                .typed_part::<ActorEnergy>(e, &PART_ENERGY.into())
                .map(|a| a.speed)
                .unwrap_or(BASE_SPEED);
            scheduler.add_actor(e, ActorEnergy::at_threshold(espeed));
        }
        let scheduler = Arc::new(Mutex::new(scheduler));
        // Install the live world state into the shared bridge snapshot so the
        // script host's map/nav reads and `spawn_at` resolve against the same
        // zone, scheduler and player this sim drives.
        if let Some(b) = &bridge {
            b.set_sim_world_state(ctx.map.clone(), scheduler.clone(), player);
        }
        Sim {
            ctx,
            scheduler,
            player,
            bridge,
            enemy,
            facing: (1, 0),
            log: Vec::new(),
            player_turns: 0,
        }
    }

    /// The player's current grid position (from the `Position` part).
    pub fn position(&self) -> (i32, i32) {
        match self.ctx.position(self.player) {
            Some(p) => (p.x, p.y),
            None => (0, 0),
        }
    }

    /// Read-only zone access (renderer / headless output).
    pub fn map(&self) -> &Zone {
        &self.ctx.map
    }

    /// The player's rendered tile id (from its `RenderTile` component).
    pub fn player_tile(&self) -> String {
        self.ctx
            .broker
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .world()
            .get::<&RenderTile>(self.player)
            .ok()
            .map_or_else(|| "creature.player.missing".to_string(), |r| r.tile.clone())
    }

    /// The accumulated event log, one entry per player action. Deterministic
    /// for a fixed script and map. This is the seed of the replay format.
    pub fn log(&self) -> &[String] {
        &self.log
    }

    /// The player's current `Health.hp` (0 if absent), for the smoke test.
    #[allow(dead_code)] // used by the integration smoke test (crates/tessera/tests)
    pub fn player_health(&self) -> i32 {
        use tessera_core::events::PART_HEALTH;
        self.ctx
            .broker
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .typed_part::<tessera_core::events::Health>(self.player, &PART_HEALTH.into())
            .map(|h| h.hp)
            .unwrap_or(0)
    }

    /// Whether `entity` is currently registered with the scheduler (i.e.
    /// scheduled). Exposed for integration tests of scripted `spawn_at`.
    #[allow(dead_code)] // used by the integration smoke test (crates/tessera/tests)
    pub fn has_scheduled(&self, entity: Entity) -> bool {
        self.scheduler
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .actors()
            .any(|e| e == entity)
    }

    /// Translate the script-facing id returned by `engine.spawn_at` to the
    /// live broker entity (via the shared bridge snapshot). `None` when the id
    /// does not correspond to a scripted spawn.
    #[allow(dead_code)] // used by the integration smoke test (crates/tessera/tests)
    pub fn resolve_script_entity(&self, script_id: u64) -> Option<Entity> {
        self.bridge.as_ref()?.resolve_script_entity(script_id)
    }

    /// The enemy (Zapper)'s current `Health.hp` (0 if absent), for the
    /// smoke test of the player's `Zap` action.
    #[allow(dead_code)] // used by the integration smoke test (crates/tessera/tests)
    pub fn enemy_health(&self) -> i32 {
        let Some(enemy) = self.enemy else {
            return 0;
        };
        use tessera_core::events::PART_HEALTH;
        self.ctx
            .broker
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .typed_part::<tessera_core::events::Health>(enemy, &PART_HEALTH.into())
            .map(|h| h.hp)
            .unwrap_or(0)
    }

    /// The enemy (Zapper)'s scripted `Charge` count, if it carries one.
    #[allow(dead_code)] // used by the integration smoke test (crates/tessera/tests)
    pub fn enemy_charge(&self) -> Option<i64> {
        let enemy = self.enemy?;
        use tessera_core::ecs::PartTypeId;
        use tessera_core::script_part::PartValue;
        let b = self.ctx.broker.read().unwrap_or_else(|p| p.into_inner());
        let part = b.get_part(enemy, &PartTypeId::new("Charge"))?;
        let sp = part
            .as_any()
            .downcast_ref::<tessera_core::script_part::ScriptPart>()?;
        match sp.0.get("charges") {
            Some(PartValue::Int(n)) => Some(*n),
            _ => None,
        }
    }

    /// Submit one player action: run the scheduler until the player's turn
    /// comes up, then dispatch the corresponding event. A wall-blocked move is
    /// cancelled by the EARLY handler (no energy charged, position unchanged).
    ///
    /// `Zap` operators the player's scripted strike: `Attack` toward the last
    /// faced direction (matching the reference demo mapping). When an
    /// enemy is scheduled, any enemy turns that become ready ahead of the
    /// player's are resolved first (the enemy attacks the player), so the
    /// shared dispatch path is exercised for both sides.
    pub fn submit_action(&mut self, action: PlayerAction) -> anyhow::Result<TurnReport> {
        // Advance until the player is ready. If an enemy becomes ready first,
        // resolve its turn (it attacks the player) and keep going. This lets
        // the demo Zapper act on the scheduler's clock while preserving the
        // exactly-once player-action contract of the harness.
        loop {
            let mut sched = self
                .scheduler
                .lock()
                .map_err(|_| anyhow::anyhow!("scheduler lock poisoned"))?;
            if let Err(stalled) = sched.advance_until_ready() {
                anyhow::bail!("{stalled}");
            }
            let player_ready = sched.ready().contains(&self.player);
            if player_ready {
                break;
            }
            if let Some(enemy) = self.enemy {
                if sched.ready().contains(&enemy) {
                    drop(sched);
                    self.take_turn(enemy, TurnAction::Attack(self.player))?;
                    continue;
                }
            }
            // No enemy and player not ready: can't happen (advance guarantees
            // somebody is ready), but bail rather than spin.
            anyhow::bail!("no actor became ready");
        }
        let from = self
            .ctx
            .position(self.player)
            .map(|p| (p.x, p.y))
            .unwrap_or((0, 0));
        let turn = match action {
            PlayerAction::Move { dx, dy } => {
                self.facing = (dx, dy);
                TurnAction::MoveTo(Point::new(from.0 + dx, from.1 + dy))
            }
            PlayerAction::Wait => TurnAction::Wait,
            PlayerAction::Zap => {
                // The player's scripted strike: Attack a target in the last
                // faced cell. With no live target there the Attack cancels (or
                // the scripted handler vetoes when out of charges).
                let (dx, dy) = self.facing;
                let target = Point::new(from.0 + dx, from.1 + dy);
                TurnAction::Attack(self.target_at(target))
            }
        };
        let report = {
            // Player-turn clock: increment *before* the player's turn resolves
            // so the turn's ActorTurnStart handler observes the incremented
            // count; "after N player turns" is defined on submitted actions,
            // cancelled or not. Mirrored into the shared bridge snapshot so
            // scripts read the same clock via `engine.player_turns()`.
            self.player_turns += 1;
            if let Some(b) = &self.bridge {
                b.set_player_turns(self.player_turns);
            }
            self.take_turn(self.player, turn)?
        };
        Ok(report)
    }

    /// The live entity (enemy, then player) standing at a grid cell, if any.
    /// Deterministic prioritization is not content-dependent: the demo enemy
    /// is the only other scheduled actor, so this resolves the Zapper.
    fn target_at(&self, cell: Point) -> hecs::Entity {
        let mut at = Vec::new();
        for (e, _p) in self.entities_at(cell) {
            if self.enemy == Some(e) {
                return e;
            }
            if e == self.player {
                return e;
            }
            at.push(e);
        }
        at.first().copied().unwrap_or(self.player)
    }

    /// All live entities carrying a `Position` at the given cell.
    fn entities_at(&self, cell: Point) -> Vec<(Entity, Point)> {
        use tessera_core::events::PART_POSITION;
        let b = self.ctx.broker.read().unwrap_or_else(|p| p.into_inner());
        let mut out = Vec::new();
        for entity in {
            let sched = self.scheduler.lock().unwrap_or_else(|p| p.into_inner());
            sched.actors().collect::<Vec<_>>()
        } {
            if let Some(pos) =
                b.typed_part::<tessera_core::events::Position>(entity, &PART_POSITION.into())
            {
                if pos.x == cell.x && pos.y == cell.y {
                    out.push((entity, Point::new(pos.x, pos.y)));
                }
            }
        }
        out
    }

    /// Resolve one actor's scheduled turn, logging the event line, then forward
    /// the scripted event to the host (if a bridge is attached) so `engine.on`
    /// handlers share this dispatch. Used for both the player (in
    /// `submit_action`) and the enemy (Zapper).
    fn take_turn(&mut self, actor: Entity, action: TurnAction) -> anyhow::Result<TurnReport> {
        let from = self
            .ctx
            .position(actor)
            .map(|p| (p.x, p.y))
            .unwrap_or((0, 0));
        let report = {
            let mut sched = self.scheduler.lock().unwrap_or_else(|p| p.into_inner());
            sched.take_turn(&mut self.ctx, actor, action)?
        };
        self.log.push(self.format_log_line(&report, from));
        // Scripted dispatch: the host's `engine.on` handlers share the same
        // broker, so e.g. a Zapper's scripted LATE Attack handler stacks its
        // zap on the native base hit, and a goblin AI acting on the exposed
        // `ActorTurnStart` hook can read the world. We forward the hook for
        // every actor turn, then the actor's own action event.
        if self.bridge.is_some() {
            self.forward_actor_turn_start(actor);
            self.forward_scripted_action(actor, action);
        }
        Ok(report)
    }

    /// Forward the actor's `ActorTurnStart` to the script host: the exposed
    /// turn hook (`engine.on("ActorTurnStart", ...)`) that lets a script
    /// policy react to any actor's turn with the read/nav API.
    fn forward_actor_turn_start(&mut self, actor: Entity) {
        let Some(bridge) = &self.bridge else {
            return;
        };
        let host = bridge.host().clone();
        let actor_id = entity_to_id(actor);
        let params = int_params(&[("actor", actor_id as i64)]);
        let mut h = match host.lock() {
            Ok(h) => h,
            Err(_) => return,
        };
        let _res = h.dispatch("ActorTurnStart", actor_id, &params);
        for line in h.drain_log() {
            self.log.push(line);
        }
    }

    /// Forward an actor's action event to the script host and append
    /// deterministic evidence of each script-handler outcome to the event log.
    fn forward_scripted_action(&mut self, actor: Entity, action: TurnAction) {
        let TurnAction::Attack(defender) = action else {
            return;
        };
        let Some(bridge) = &self.bridge else {
            return;
        };
        let host = bridge.host().clone();
        let attacker_id = entity_to_id(actor);
        let defender_id = entity_to_id(defender);
        let params = int_params(&[
            ("attacker", attacker_id as i64),
            ("defender", defender_id as i64),
        ]);
        let mut h = match host.lock() {
            Ok(h) => h,
            Err(_) => return,
        };
        let res = h.dispatch("Attack", defender_id, &params);
        for ev in &res.outcomes {
            let line = match ev {
                tessera_script::HandlerOutcome::Passed => {
                    format!("SCRIPT Attack attacker={attacker_id} defender={defender_id} passed")
                }
                tessera_script::HandlerOutcome::Vetoed => {
                    format!("SCRIPT Attack defender={defender_id} veto")
                }
                tessera_script::HandlerOutcome::Errored { error } => {
                    format!("SCRIPT Attack ERROR {error}")
                }
                tessera_script::HandlerOutcome::Quarantined { script, event } => {
                    format!("SCRIPT Attack QUARANTINE {script}/{event}")
                }
            };
            self.log.push(line);
        }
        for err in &res.apply_errors {
            self.log.push(format!("SCRIPT Attack APPLY_ERROR {err}"));
        }
        // Capture this dispatch's `engine.log` messages (e.g. the Zapper's
        // "zapper zaps <id>") into the same event log, so `--headless` prints
        // observable script evidence alongside the native event lines.
        for line in h.drain_log() {
            self.log.push(line);
        }
    }

    /// Pretty-print one event-log line: stable one-event-per-line format
    /// `KEY <Event::NAME> tick=..` with the outcome for moves.
    fn format_log_line(&self, report: &TurnReport, from: (i32, i32)) -> String {
        match report.action {
            TurnAction::MoveTo(to) => {
                let outcome = if report.charged {
                    "handled"
                } else {
                    "cancelled"
                };
                format!(
                    "EVENT Move tick={} from=({},{}) to=({},{}) {}",
                    report.tick, from.0, from.1, to.x, to.y, outcome
                )
            }
            TurnAction::Wait => format!("EVENT Wait tick={}", report.tick),
            _ => format!("EVENT {:?} tick={}", report.action, report.tick),
        }
    }

    /// Render the final map as ASCII text (uses the tile registry glyphs; the
    /// player overlays `@`). A pure reader — no dispatch, no mutation.
    pub fn text_map(&self, tiles: &TileRegistry) -> String {
        let (px, py) = self.position();
        let floor = tile_glyph(tiles, "terrain.floor");
        let wall = tile_glyph(tiles, "terrain.wall");
        let mut s = String::with_capacity(self.ctx.map.width * self.ctx.map.height);
        for y in 0..self.ctx.map.height {
            for x in 0..self.ctx.map.width {
                if (x as i32, y as i32) == (px, py) {
                    s.push('@');
                } else {
                    s.push(if self.ctx.map.is_walkable(x as i32, y as i32) {
                        floor
                    } else {
                        wall
                    });
                }
            }
            s.push('\n');
        }
        s
    }

    /// Run `n` scripted actions headlessly (no window), returning the final
    /// map text. The event log is available via [`Self::log`]. The script
    /// deterministically exercises ordinary handled movement, a wall-collision
    /// cancellation, and Wait — the full demo obligation — so CI can assert on
    /// it without a display.
    pub fn run_headless(&mut self, tiles: &TileRegistry, n: u32) -> String {
        for i in 0..n {
            let action = headless_script(i as usize);
            // A wall block or other bus error is a content bug: surface it as
            // part of the run rather than silently swallowing (a cancelled move
            // must never look like it cost energy).
            if let Err(e) = self.submit_action(action) {
                let tick = self
                    .scheduler
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .ticks();
                self.log.push(format!("ERROR tick={tick} {e}"));
                break;
            }
        }
        self.text_map(tiles)
    }
}

/// Deterministic headless action script. Cycle of `[right, right, down, down,
/// wait]`, so within a few steps the player walks into a map collision (the
/// pillar at x%8==4 && y%6==3) and the cancellation path is exercised.
fn headless_script(i: usize) -> PlayerAction {
    match i % 5 {
        0 => PlayerAction::Move { dx: 1, dy: 0 },
        1 => PlayerAction::Move { dx: 1, dy: 0 },
        2 => PlayerAction::Move { dx: 0, dy: 1 },
        3 => PlayerAction::Move { dx: 0, dy: 1 },
        _ => PlayerAction::Wait,
    }
}

/// The glyph for a tile id, falling back to `'?'`.
fn tile_glyph(tiles: &TileRegistry, id: &str) -> char {
    match tiles.get(id).visual {
        tessera_core::tiles::TileVisual::Glyph { ch } => ch,
    }
}

/// Register the reference demo combat: an `Attack` on a Health-carrying
/// defender resolves to `TakeDamage` (base hit, `NORMAL`), and a `TakeDamage`
/// always reduces `Health.hp` by its amount. The base hit is the native half
/// of the Zapper demo's damage; the scripted zap (LATE) stacks on top.
///
/// When a `TakeDamage` clamps `hp` to zero, the native applicator dispatches
/// the reserved [`Death`] event exactly once: the "was_alive" guard means a
/// second hit on an already-dead entity (`hp == 0` at entry) does not re-emit.
fn install_base_combat(ctx: &mut SimContext) {
    use tessera_core::events::{Death, Health, PART_HEALTH, install_minimal_combat};
    // Attack -> TakeDamage (base 1), cancels when the defender has no Health.
    install_minimal_combat(ctx);
    // TakeDamage applicator: the actual native damage the base hit inflicts.
    ctx.register::<TakeDamage>(NORMAL, |ctx, ev| {
        let mut reduce = false;
        {
            let b = ctx.broker.read().unwrap_or_else(|p| p.into_inner());
            if let Some(h) = b.typed_part::<Health>(ev.target, &PART_HEALTH.into()) {
                if h.hp > 0 {
                    reduce = true;
                }
            }
        }
        if reduce {
            if let Some(h) = ctx
                .broker
                .write()
                .unwrap_or_else(|p| p.into_inner())
                .typed_part_mut::<Health>(ev.target, &PART_HEALTH.into())
            {
                h.hp = (h.hp - ev.amount).max(0);
            }
            // Emit Death exactly once at hp zero. This is a nested dispatch of
            // a *different* event type, so the bus's re-entrancy control does
            // not reject it; a second reducing hit on an already-dead entity
            // never reaches here (reduce requires hp > 0 at entry).
            let dead = ctx
                .broker
                .read()
                .unwrap_or_else(|p| p.into_inner())
                .typed_part::<Health>(ev.target, &PART_HEALTH.into())
                .map(|h| h.hp == 0)
                .unwrap_or(false);
            if dead {
                let _ = ctx.dispatch(Death { target: ev.target });
            }
        }
        HandlerAction::Continue
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_core::events::Position;
    use tessera_core::zone::Zone;

    /// The demo room the fixture runs in: bordered all-floor grid with a pillar
    /// at `x % 8 == 4 && y % 6 == 3`, matching the legacy hardcoded map.
    fn demo_zone(width: usize, height: usize) -> Zone {
        let mut cells = vec!["terrain.floor".to_string(); width * height];
        for y in 0..height {
            for x in 0..width {
                let edge = x == 0 || y == 0 || x == width - 1 || y == height - 1;
                let pillar = x % 8 == 4 && y % 6 == 3;
                if edge || pillar {
                    cells[y * width + x] = "terrain.wall".to_string();
                }
            }
        }
        Zone::from_cells("test", width, height, Point::new(1, 1), cells)
    }

    /// A demo fixture: one positioned, scheduled player in a demo room, with a
    /// spawn-order entity id and a `Position` part.
    fn fixture() -> Sim {
        let mut broker = EntityBroker::new();
        let player = broker.spawn();
        broker
            .insert_part(player, PART_ENERGY, Box::new(ActorEnergy::new(100)))
            .unwrap();
        broker
            .insert_part(player, tessera_core::events::PART_POSITION, {
                use tessera_core::ecs::Part;
                Box::new(Position { x: 1, y: 1 }) as Box<dyn Part>
            })
            .unwrap();
        let mut sim = Sim::new(broker, demo_zone(20, 10), player);
        sim.log.clear(); // the harness doesn't log setup
        sim
    }

    #[test]
    fn ordinary_move_is_handled_and_commits_position() {
        let mut sim = fixture();
        sim.submit_action(PlayerAction::Move { dx: 1, dy: 0 })
            .unwrap();
        assert_eq!(sim.position(), (2, 1), "player moved one cell right");
        let line = &sim.log()[0];
        assert!(line.contains("Move") && line.contains("handled"), "{line}");
    }

    #[test]
    fn wall_blocked_move_is_cancelled_no_position_change() {
        let mut sim = fixture();
        // (0,?) is out of bounds to the left -> wall block at EARLY.
        sim.submit_action(PlayerAction::Move { dx: -1, dy: 0 })
            .unwrap();
        assert_eq!(sim.position(), (1, 1), "cancelled move leaves position");
        let line = &sim.log()[0];
        assert!(
            line.contains("Move") && line.contains("cancelled"),
            "{line}"
        );
    }

    #[test]
    fn wait_records_a_turn() {
        let mut sim = fixture();
        sim.submit_action(PlayerAction::Wait).unwrap();
        assert_eq!(sim.position(), (1, 1));
        assert!(sim.log()[0].contains("Wait"), "{}", sim.log()[0]);
    }

    #[test]
    fn headless_run_is_deterministic_and_covers_all_paths() {
        let tiles = TileRegistry::new();
        let run = || {
            let mut sim = fixture();
            let map = sim.run_headless(&tiles, 12);
            (map, sim.log().to_vec())
        };
        let (map_a, log_a) = run();
        let (map_b, log_b) = run();
        assert_eq!(map_a, map_b, "final map identical across runs");
        assert_eq!(log_a, log_b, "event log identical across runs");

        // Full coverage of the demo obligation in one headless run:
        // ordinary handled move, wall cancellation, and Wait.
        assert!(
            log_a
                .iter()
                .any(|l| l.contains("Move") && l.contains("handled")),
            "expected a handled Move, got {log_a:?}"
        );
        assert!(
            log_a
                .iter()
                .any(|l| l.contains("Move") && l.contains("cancelled")),
            "expected a cancelled wall Move, got {log_a:?}"
        );
        assert!(
            log_a.iter().any(|l| l.contains("Wait")),
            "expected a Wait, got {log_a:?}"
        );
    }

    #[test]
    fn log_lines_are_stable_one_event_per_line() {
        let mut sim = fixture();
        for _ in 0..5 {
            sim.submit_action(PlayerAction::Move { dx: 1, dy: 0 })
                .unwrap();
        }
        for line in sim.log() {
            assert!(line.starts_with("EVENT "), "unexpected log line: {line}");
        }
    }
}
