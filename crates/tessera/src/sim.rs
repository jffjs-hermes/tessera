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
    EARLY, HandlerAction, HandlerId, Health, LATE, Move, NORMAL, PART_HEALTH, PART_POSITION, Point,
    Position, RecomputeDerived, SimContext, TakeDamage, TileEntry,
};
use tessera_core::rng::SimRng;
use tessera_core::save::SAVE_FORMAT_VERSION;
use tessera_core::save::{SaveData, SaveError, SaveHooks, restore_broker, snapshot_entities};
use tessera_core::stats::PART_ATTRIBUTES;
use tessera_core::tiles::{Rgb, Tile, TileRegistry, TileVisual};
use tessera_core::turns::{
    ActorEnergy, BASE_SPEED, PART_ENERGY, Scheduler, TurnAction, TurnReport,
};
use tessera_core::world::{RenderTile, World};
use tessera_core::zone::Zone;
use tessera_render::Cell;
use tessera_render::atlas::Atlas;

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
    /// The scripted `Bash` (X): use the player's first granted ability (the
    /// campaign's `Ability_ShieldBash`) on the reference enemy — the
    /// declarative ability path (charge spend -> damage/status/push -> Luau
    /// assertion), with no ability script involved.
    Bash,
    /// Do nothing this turn.
    Wait,
}

/// A positioned entity the renderer draws: a data-driven tile id at a grid
/// cell, resolved against the tile registry at draw time (never a hardcoded
/// glyph). Drawn in the deterministic (entity-id) order the caller supplies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderTarget {
    pub x: i32,
    pub y: i32,
    pub tile: String,
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
    pub log: Vec<String>,
    /// Shared sink for the deterministic native event lines dispatched during
    /// a turn (`UseAbility`, `TakeDamage`, `ApplyStatus` — the headless
    /// event-log mapping). The LATE observers push here; the turn loop drains
    /// the lines into `log` right after each turn so nested events appear in
    /// causal order.
    event_lines: Arc<Mutex<Vec<String>>>,
    /// Player-turn clock: increments once per submitted player action
    /// (cancelled or not). Exposed to scripts via `engine.player_turns()`.
    player_turns: u64,
    /// The registered `RecomputeDerived` handler id (unregistered/replaced on
    /// a rules reload so the new bridge's handler takes over without stacking).
    #[allow(dead_code)] // registered handler id, kept for reload wiring
    recompute_handler: HandlerId,
    /// Whether a `rules/stats.luau` handler was registered at install time —
    /// the fail-closed spawn gate (a stat-ful entity without a formula owner
    /// is a spawn error, ).
    stats_armed: bool,
}

#[allow(dead_code)] // stats-slice contract seams: actor_speed/reload_rules/log
impl Sim {
    // ---- console/devtools support -------------------------------------
    //
    // Small public surfaces the headless console (`devtools/console.rs`)
    // executes against, added to satisfy the debug console contract without
    // leaking the whole broker/scheduler. Each returns a deterministic,
    // human-readable line the executor turns into a `CMD ...` event-log line.

    /// Read-only shared handle to the live broker (console reads: `print`,
    /// `entities`). The executor holds the read lock only for the duration of
    /// one command — it never spans turns or event dispatch.
    pub fn broker(&self) -> &Arc<RwLock<EntityBroker>> {
        &self.ctx.broker
    }

    /// The scheduler's current tick count (for `CMD ... tick=..` lines).
    pub fn ticks(&self) -> u64 {
        self.scheduler
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .ticks()
    }

    /// Append one deterministic event-log line (a `CMD ...` console line).
    pub fn log_push(&mut self, line: String) {
        self.log.push(line);
    }

    /// Spawn a blueprint into the live world at `at` (default: the zone start
    /// tile), stamp a `Position`, and schedule it as an actor if it carries an
    /// `ActorEnergy` part (console `spawn`). Fail-closed: unknown blueprint or
    /// a build error despawns the husk and returns the diagnostic, never a
    /// partial entity.
    pub fn spawn_blueprint_console(
        &mut self,
        world: &World,
        bp: &str,
        at: Option<(i32, i32)>,
    ) -> Result<hecs::Entity, String> {
        let (x, y) = at.unwrap_or((self.ctx.map.start.x, self.ctx.map.start.y));
        let entity = {
            let mut broker = self
                .ctx
                .broker
                .write()
                .map_err(|_| "broker poisoned".to_string())?;
            let e = world
                .spawn_blueprint(bp, &mut broker)
                .map_err(|e| e.to_string())?;
            broker
                .insert_part(
                    e,
                    PART_POSITION,
                    Box::new(Position::from_point(Point::new(x, y)))
                        as Box<dyn tessera_core::ecs::Part>,
                )
                .map_err(|e| e.to_string())?;
            e
        };
        // Fail-closed spawn : a stat-ful entity requires a
        // registered rules script — a formula-less stat is an
        // unrepresentable state, not a silently slow goblin.
        let statful = {
            let broker = self.ctx.broker.read().unwrap_or_else(|p| p.into_inner());
            tessera_core::recompute::carries_attributes(&broker, entity)
        };
        if statful && !self.stats_armed {
            let _ = self.despawn_entity(entity);
            return Err(format!(
                "spawn {bp}: entity carries parts.Attributes but no rules/stats.luau \
                 handler is registered (fail-closed)"
            ));
        }
        let speed = self
            .ctx
            .broker
            .read()
            .map_err(|_| "broker poisoned".to_string())?
            .typed_part::<ActorEnergy>(entity, &PART_ENERGY.into())
            .map(|a| a.speed)
            .unwrap_or(BASE_SPEED);
        self.scheduler
            .lock()
            .map_err(|_| "scheduler poisoned".to_string())?
            .add_actor(entity, ActorEnergy::at_threshold(speed));
        // Spawn-after-stamp dirtying : the recompute lands the
        // cache (and the scheduler speed sync) before the next scheduled
        // action.
        let _ = self.ctx.dispatch(RecomputeDerived { entity });
        self.log.extend(drain_event_lines(&self.event_lines));
        Ok(entity)
    }

    /// Write a stat part directly (console `setpart`; debug mutation between
    /// turns). Only the persistent stat parts (`Attributes`/`Level`) are
    /// writable — this is the declared console dirtying path: a write to
    /// either dispatches `RecomputeDerived` so the cache reflects the new
    /// values before the next action . Bounds are the native
    /// load invariants; an out-of-range write fails closed.
    pub fn set_part_console(
        &mut self,
        entity: hecs::Entity,
        name: &str,
        fields: &std::collections::BTreeMap<String, i64>,
    ) -> Result<(), String> {
        use tessera_core::stats::{ATTRIBUTE_MAX, ATTRIBUTE_MIN, LEVEL_MAX, LEVEL_MIN};
        {
            let mut broker = self
                .ctx
                .broker
                .write()
                .map_err(|_| "broker poisoned".to_string())?;
            match name {
                tessera_core::stats::PART_ATTRIBUTES => {
                    let get = |key: &str| fields.get(key).copied().unwrap_or(10);
                    for (key, value) in [
                        ("might", get("might")),
                        ("agility", get("agility")),
                        ("toughness", get("toughness")),
                        ("will", get("will")),
                        ("intellect", get("intellect")),
                    ] {
                        if !(ATTRIBUTE_MIN..=ATTRIBUTE_MAX).contains(&value) {
                            return Err(format!(
                                "attribute '{key}' must be in [{ATTRIBUTE_MIN}, {ATTRIBUTE_MAX}], wrote {value}"
                            ));
                        }
                    }
                    broker
                        .insert_part(
                            entity,
                            tessera_core::stats::PART_ATTRIBUTES.to_string(),
                            Box::new(tessera_core::stats::Attributes {
                                might: get("might"),
                                agility: get("agility"),
                                toughness: get("toughness"),
                                will: get("will"),
                                intellect: get("intellect"),
                            }),
                        )
                        .map_err(|e| format!("setpart Attributes: {e}"))?;
                }
                tessera_core::stats::PART_LEVEL => {
                    let level = fields.get("level").copied().unwrap_or(1);
                    if !(LEVEL_MIN..=LEVEL_MAX).contains(&level) {
                        return Err(format!(
                            "level must be in [{LEVEL_MIN}, {LEVEL_MAX}], wrote {level}"
                        ));
                    }
                    broker
                        .insert_part(
                            entity,
                            tessera_core::stats::PART_LEVEL.to_string(),
                            Box::new(tessera_core::stats::Level { level }),
                        )
                        .map_err(|e| format!("setpart Level: {e}"))?;
                }
                other => {
                    return Err(format!(
                        "setpart: '{other}' is not writable; only Attributes/Level are"
                    ));
                }
            }
        }
        // The console dirtying path: recompute the cache immediately (the
        // values settle before the next scheduled action).
        let _ = self.ctx.dispatch(RecomputeDerived { entity });
        self.log.extend(drain_event_lines(&self.event_lines));
        Ok(())
    }

    /// The scheduler speed of an actor (the row the recompute sync keeps in
    /// step with the derived cache). Mainly for tests and read-only seams.
    pub fn actor_speed(&self, entity: Entity) -> Option<u32> {
        self.scheduler
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .speed(entity)
    }

    /// Resolve a console entity id (the `CMD ...`/`entities` decimal `u64`
    /// bits) to a live broker entity, if any.
    pub fn entity_from_id(&self, id: u64) -> Option<hecs::Entity> {
        let e = hecs::Entity::from_bits(id)?;
        self.ctx
            .broker
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .is_alive(e)
            .then_some(e)
    }

    /// The sim's player entity (the `stats player` console alias and the
    /// demo readout's anchor).
    pub fn player_entity(&self) -> hecs::Entity {
        self.player
    }

    /// Despawn a live entity and unregister it from the scheduler (console
    /// `kill`). A gone id is an error.
    pub fn despawn_entity(&mut self, entity: hecs::Entity) -> Result<(), String> {
        {
            let mut broker = self
                .ctx
                .broker
                .write()
                .map_err(|_| "broker poisoned".to_string())?;
            broker.despawn(entity).map_err(|e| e.to_string())?;
        }
        self.scheduler
            .lock()
            .map_err(|_| "scheduler poisoned".to_string())?
            .remove_actor(entity);
        Ok(())
    }

    /// Write the `Health.hp` part directly (console `sethp`; a debug mutation,
    /// executed between turns, never during dispatch). No event is emitted —
    /// the `CMD` log line is the record.
    pub fn set_entity_hp(&mut self, entity: hecs::Entity, hp: i32) -> Result<(), String> {
        let mut broker = self
            .ctx
            .broker
            .write()
            .map_err(|_| "broker poisoned".to_string())?;
        let h = broker
            .typed_part_mut::<Health>(entity, &PART_HEALTH.into())
            .ok_or_else(|| format!("no Health part on {}", entity_to_id(entity)))?;
        h.hp = hp;
        Ok(())
    }

    /// Write the `Position` part directly (console `tp`; debug mutation,
    /// between turns). No event is emitted — same rule as `sethp`.
    pub fn set_entity_position(
        &mut self,
        entity: hecs::Entity,
        x: i32,
        y: i32,
    ) -> Result<(), String> {
        let mut broker = self
            .ctx
            .broker
            .write()
            .map_err(|_| "broker poisoned".to_string())?;
        let pos = broker
            .typed_part_mut::<Position>(entity, &PART_POSITION.into())
            .ok_or_else(|| format!("no Position part on {}", entity_to_id(entity)))?;
        pos.x = x;
        pos.y = y;
        Ok(())
    }

    /// Replace this sim's live state from a saved snapshot (console `load` and
    /// the `--load` startup flag). Preserves the current zone (which must match
    /// the save's, else [`SaveError::ZoneMismatch`]) and the script bridge.
    pub fn restore_from_save(
        &mut self,
        data: SaveData,
        hooks: &SaveHooks,
    ) -> Result<(), SaveError> {
        let zone = self.ctx.map.clone();
        let bridge = self.bridge.clone();
        *self = Self::load(data, zone, hooks, bridge)?;
        Ok(())
    }

    /// Build the harness around an already-spawned `player` whose broker
    /// carries a `Position` part (no script host / no enemy).
    #[cfg_attr(not(test), allow(dead_code))] // used by the sim test fixture()
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
        // Content-level handlers (collision, base combat, script death forward)
        // are re-registered identically on every construction — a save captures
        // *state*, not wiring, and load re-runs them exactly as zone start.
        let event_lines = Arc::new(Mutex::new(Vec::new()));
        install_default_handlers(&mut ctx, bridge.as_ref(), event_lines.clone());

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
        // The stats recompute pipeline: register the `RecomputeDerived` handler
        // (native invalidate -> rules script forward -> native finalize ->
        // scheduler speed sync), then recompute every live stat-ful entity in
        // deterministic entity-id order so the cache exists before the first
        // scheduled action. Entities already carrying a `Derived` cache are
        // recomputed too (spawn-completion dirtying, ).
        let recompute_handler =
            install_recompute_derived(&mut ctx, bridge.as_ref(), scheduler.clone());
        let stats_armed = bridge
            .as_ref()
            .is_some_and(|b| b.has_event_handler("RecomputeDerived"));
        // Install the live world state into the shared bridge snapshot so the
        // script host's map/nav reads and `spawn_at` resolve against the same
        // zone, scheduler and player this sim drives.
        if let Some(b) = &bridge {
            b.set_sim_world_state(ctx.map.clone(), scheduler.clone(), player);
            b.set_stats_armed(stats_armed);
        }
        let mut sim = Sim {
            ctx,
            scheduler,
            player,
            bridge,
            enemy,
            facing: (1, 0),
            log: Vec::new(),
            event_lines,
            player_turns: 0,
            recompute_handler,
            stats_armed,
        };
        sim.recompute_live();
        sim
    }

    /// The stats headless demo mapping for the recompute pipeline: dispatch
    /// `RecomputeDerived` for every live stat-ful entity in deterministic
    /// (entity-id) order. Called at sim construction (spawn completion for
    /// entities stamped before the sim existed) and after a rules/tuning
    /// reload; the dirtying events dispatched inside a turn (statuses,
    /// part writes) are wired separately.
    pub fn recompute_live(&mut self) {
        let entities = {
            let broker = self.ctx.broker.read().unwrap_or_else(|p| p.into_inner());
            broker.entities_with_part(PART_ATTRIBUTES)
        };
        for entity in entities {
            let _ = self.ctx.dispatch(RecomputeDerived { entity });
        }
        self.log.extend(drain_event_lines(&self.event_lines));
    }

    /// Bring every scheduled stat-ful actor's scheduler row in step with its
    /// `Derived` cache (the post-turn safety net: status dirtying recomputes
    /// run *under* `take_turn`'s scheduler lock, where the handler's inline
    /// sync skips via `try_lock`). Called right after the lock is released,
    /// before the next scheduled action; entity-id order, deterministic.
    fn sync_derived_speeds(&mut self) {
        let entities = {
            let broker = self.ctx.broker.read().unwrap_or_else(|p| p.into_inner());
            broker.entities_with_part(PART_ATTRIBUTES)
        };
        for entity in entities {
            let speed = tessera_core::recompute::derived_or_energy_speed(&self.ctx, entity);
            if let Ok(mut s) = self.scheduler.try_lock() {
                s.set_speed(entity, speed);
            }
        }
    }

    /// Drain the scripted-write recompute queue (an `Attributes`/`Level`
    /// write applied by a pending `set_part` during the previous dispatch) and
    /// dispatch each queued recompute before the next scheduled action.
    fn drain_recompute_queue(&mut self) {
        let Some(b) = &self.bridge else {
            return;
        };
        let ids = b.drain_recompute();
        for id in ids {
            // The id may be a script-facing (host-assigned) id from a
            // scripted spawn; resolve through the mapping first, then the
            // plain broker bits.
            let entity = b
                .resolve_script_entity(id)
                .or_else(|| Entity::from_bits(id));
            let Some(entity) = entity else {
                continue;
            };
            let _ = self.ctx.dispatch(RecomputeDerived { entity });
        }
        self.log.extend(drain_event_lines(&self.event_lines));
    }

    /// Replace the rules handler with a freshly built script bridge's (the
    /// deterministic tuning/script reload seam: a content reload re-registers
    /// `rules/stats.luau` from the new library and recomputes every live
    /// stat-ful entity in entity-id order, so the cache reflects the new
    /// tuning before the next scheduled action — ).
    pub fn reload_rules(&mut self, bridge: Option<ScriptBridge>) {
        self.ctx
            .unregister::<RecomputeDerived>(self.recompute_handler);
        self.recompute_handler =
            install_recompute_derived(&mut self.ctx, bridge.as_ref(), self.scheduler.clone());
        self.stats_armed = bridge
            .as_ref()
            .is_some_and(|b| b.has_event_handler("RecomputeDerived"));
        self.bridge = bridge;
        if let Some(b) = &self.bridge {
            b.set_stats_armed(self.stats_armed);
        }
        self.recompute_live();
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

    /// Every live entity carrying both a `RenderTile` (the `Render` part) and
    /// a `Position`, as a positioned tile id to draw on top of the terrain.
    /// Deterministic draw order: sorted by entity id (never hash order), so the
    /// resulting cells are stable frame to frame. Meant for the renderer, which
    /// stays a pure reader — this never dispatches events or mutates state.
    pub fn renderables(&self) -> Vec<RenderTarget> {
        use tessera_core::events::{PART_POSITION, Position};
        let b = self.ctx.broker.read().unwrap_or_else(|p| p.into_inner());
        let mut targets: Vec<(Entity, RenderTarget)> = Vec::new();
        for e in b.entities_with_part(PART_POSITION) {
            let Some(pos) = b.typed_part::<Position>(e, &PART_POSITION.into()) else {
                continue;
            };
            let Ok(rt) = b.world().get::<&RenderTile>(e) else {
                continue;
            };
            targets.push((
                e,
                RenderTarget {
                    x: pos.x,
                    y: pos.y,
                    tile: rt.tile.clone(),
                },
            ));
        }
        targets.sort_by_key(|(e, _)| *e);
        targets.into_iter().map(|(_, t)| t).collect()
    }

    /// The player's turn clock (increments once per submitted player action).
    pub fn player_turns_public(&self) -> u64 {
        self.player_turns
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

    /// Snapshot the sim's full live state into a [`SaveData`] via the hook
    /// registry. Fail-closed: any entity part not in the whitelist returns
    /// [`SaveError::UnserializablePart`]. A save captures *state*, not event
    /// handlers or blueprint wiring — those are re-registered on load exactly
    /// as at zone start ([`Sim::load`]).
    // NB: exercised by the unit/integration tests and the snapshot_smoke
    // example; the CLI --save binding lands with the headless console work.
    #[allow(dead_code)]
    pub fn save(&self, hooks: &SaveHooks) -> Result<SaveData, SaveError> {
        let broker = self.ctx.broker.read().unwrap_or_else(|p| p.into_inner());
        let snapshot = snapshot_entities(&broker, hooks)?;
        let scheduler = self.scheduler.lock().unwrap_or_else(|p| p.into_inner());
        let actors = scheduler
            .actors()
            .map(|e| {
                (
                    e.to_bits().get(),
                    scheduler.energy(e).unwrap_or(0),
                    scheduler.speed(e).unwrap_or(BASE_SPEED),
                )
            })
            .collect();
        Ok(SaveData {
            version: SAVE_FORMAT_VERSION,
            zone_id: self.ctx.map.id.clone(),
            next_entity_bits: snapshot.next_entity_bits,
            entities: snapshot.entities,
            player: Some(self.player.to_bits().get()),
            clock: scheduler.ticks(),
            actors,
            rng: self.ctx.rng.clone(),
            streams: self.ctx.streams().clone(),
            player_turns: self.player_turns,
            facing: self.facing,
        })
    }

    /// Rebuild a sim from a saved snapshot. The zone is *not* serialized (it is
    /// a pure function of the pack), so the caller provides it — and the saved
    /// `zone_id` must match it or this returns
    /// [`SaveError::ZoneMismatch`] (saves are pack-specific; fail closed
    /// rather than teleport entities into a different map).
    ///
    /// Re-registers the demo world's default handlers exactly as [`Sim::new()`] /
    /// [`Sim::from_shared`] do at zone start (event handlers are wiring, not
    /// state, and are never saved), restores the exact entity ids so
    /// deterministic id-ordering is preserved, and restores the scheduler
    /// clock/actor energies, the ChaCha8 RNG stream, the player, facing and
    /// player-turn clock. On load the event log starts empty.
    #[allow(dead_code)] // exercised by tests + the snapshot_smoke example
    pub fn load(
        data: SaveData,
        zone: Zone,
        hooks: &SaveHooks,
        bridge: Option<ScriptBridge>,
    ) -> Result<Self, SaveError> {
        if data.zone_id != zone.id {
            return Err(SaveError::ZoneMismatch {
                saved: data.zone_id.clone(),
                current: zone.id,
            });
        }
        // Restore the broker at exact saved entity ids with all parts. Keep a
        // bridge's existing Arc and replace its contents in place: all of the
        // host's WorldAccess clones then observe the same broker as the sim.
        let restored_broker = restore_broker(&data, hooks)?;
        let broker = bridge
            .as_ref()
            .map(ScriptBridge::broker)
            .unwrap_or_else(|| Arc::new(RwLock::new(EntityBroker::new())));

        // Restore the scheduler state of record: clock + (entity, energy,
        // speed). Fail closed on an entity id that no longer resolves.
        let mut sched_actors = Vec::new();
        for (bits, energy, speed) in &data.actors {
            let e = Entity::from_bits(*bits).ok_or_else(|| SaveError::PartRestore {
                name: "ActorEnergy".into(),
                msg: format!("saved actor entity id {bits:#x} does not resolve"),
            })?;
            sched_actors.push((
                e,
                ActorEnergy {
                    speed: *speed,
                    energy: *energy,
                },
            ));
        }
        let scheduler = Arc::new(Mutex::new(Scheduler::restore(data.clock, sched_actors)));

        // Player entity: the saved handle, restored into the live broker.
        let player =
            data.player
                .and_then(Entity::from_bits)
                .ok_or_else(|| SaveError::PartRestore {
                    name: "Player".into(),
                    msg: "save has no valid player entity".into(),
                })?;

        if let Some(b) = &bridge {
            b.replace_broker(restored_broker);
        } else {
            *broker.write().unwrap_or_else(|p| p.into_inner()) = restored_broker;
        }

        // Live context over the restored broker, re-registering the default
        // handlers (never saved — wiring is re-created at zone start).
        let mut ctx = SimContext::shared(broker.clone(), zone, data.rng);
        // Restore every named stream exactly where it stood, so a save taken
        // mid-fight resumes combat draws from the same point.
        ctx.restore_streams(data.streams.clone());
        let event_lines = Arc::new(Mutex::new(Vec::new()));
        install_default_handlers(&mut ctx, bridge.as_ref(), event_lines.clone());
        let recompute_handler =
            install_recompute_derived(&mut ctx, bridge.as_ref(), scheduler.clone());
        let stats_armed = bridge
            .as_ref()
            .is_some_and(|b| b.has_event_handler("RecomputeDerived"));

        // The reference enemy is the first scheduled non-player actor
        // (deterministic entity-id order); matches how `from_shared` treats the
        // demo's single scheduled goblin after a reload.
        let enemy = {
            let s = scheduler.lock().unwrap_or_else(|p| p.into_inner());
            s.actors().find(|e| *e != player)
        };
        if let Some(b) = &bridge {
            b.set_sim_world_state(ctx.map.clone(), scheduler.clone(), player);
            b.set_stats_armed(stats_armed);
        }

        let mut sim = Sim {
            ctx,
            scheduler,
            player,
            bridge,
            enemy,
            facing: data.facing,
            log: Vec::new(),
            event_lines,
            player_turns: data.player_turns,
            recompute_handler,
            stats_armed,
        };
        // Restore-recompute : a loaded world gets live derived
        // values for every restored stat-ful entity, in entity-id order,
        // before the next scheduled action.
        sim.recompute_live();
        Ok(sim)
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
        // A scripted `Attributes`/`Level` write from the previous turn's
        // dispatch recomputes before this action resolves (the dirtying
        // table's "pending set_part path", ).
        self.drain_recompute_queue();
        // The player may have respawned since the last action: re-sync the
        // live player entity from the shared bridge snapshot (a scripted
        // respawn creates a *new* Player entity).
        self.sync_player();
        // Advance until the player is ready. If a non-player actor becomes
        // ready first, resolve its turn via the script policy (ActorTurnStart
        // -> Step/Attack/Wander) and keep going. This lets the goblin act on
        // the scheduler's clock (chase, attack, wander, respawned goblins too)
        // while preserving the exactly-once player-action contract.
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
            // Resolve the turn of the first ready non-player actor. This is
            // the general rule (works for the original enemy and for a
            // respawned goblin scheduled later), not a fixed-entity shortcut.
            let other = sched.ready().into_iter().find(|e| *e != self.player);
            if let Some(actor) = other {
                drop(sched);
                self.take_scripted_turn(actor)?;
                self.sync_player();
                continue;
            }
            // No non-player actor and player not ready: can't happen (advance
            // guarantees somebody is ready), but bail rather than spin.
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
                self.step_or_bump(self.player, Point::new(from.0 + dx, from.1 + dy))
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
            PlayerAction::Bash => match self.enemy {
                // Shield Bash the reference enemy (the ability path: the
                // scheduler resolves the player's first granted ability and
                // the native effect runtime spends a charge and resolves the
                // declarative damage/status/push).
                Some(e) => TurnAction::UseAbility { target: e },
                // No reference enemy to bash (and the targeting pipeline is
                // later work): the turn is a no-op.
                None => TurnAction::Wait,
            },
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

    /// Re-point `self.player` at the live player entity. A scripted respawn
    /// (goblin policy on `Death`) despawns the old player and spawns a fresh
    /// one via `engine.spawn_at("Player", ...)`, so `self.player` must follow
    /// the shared snapshot's current player id between actions.
    fn sync_player(&mut self) {
        let Some(bridge) = &self.bridge else {
            return;
        };
        let Some(id) = bridge.player_id() else {
            return;
        };
        // The shared player id is the real broker entity bits; `from_bits`
        // reconstructs it exactly like `entity_to_id` produced it.
        if let Some(e) = Entity::from_bits(id) {
            self.player = e;
        }
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
                && pos.x == cell.x
                && pos.y == cell.y
            {
                out.push((entity, Point::new(pos.x, pos.y)));
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
        // Post-turn derived-speed net: status dirtying recomputed during the
        // turn (under the scheduler lock); sync the rows now that the lock is
        // free so the next accumulation uses the cache .
        self.sync_derived_speeds();
        self.log.push(self.format_log_line(&report, from));
        // Nested native event lines for this turn (the headless mapping:
        // `UseAbility`'s turn line, then the causal `TakeDamage`/`ApplyStatus`
        // lines the LATE observers queued during dispatch).
        self.log.extend(drain_event_lines(&self.event_lines));
        // Drains any deaths observed during the turn *after* the scheduler
        // lock is released, so the respawn policy's spawn/despawn can mutate
        // the scheduler without re-entering the lock.
        if let Some(b) = &self.bridge {
            b.drain_deaths();
        }
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

    /// Resolve a **non-player** actor's turn through the script policy: run the
    /// `ActorTurnStart` hook (forwarding to the host and collecting the queued
    /// Step/Attack/Wander turn actions), translate the first one into a
    /// scheduler [`TurnAction`], and take the turn. When no script handler
    /// queues an action (e.g. a Zapper whose script only hooks `Attack`, or a
    /// goblin with no AI script in a test), fall back to the legacy default —
    /// attack the player — so the existing Zapper / no-AI interactions are
    /// unchanged.
    fn take_scripted_turn(&mut self, actor: Entity) -> anyhow::Result<TurnReport> {
        // Same pre-turn recompute drain as the player path (a scripted stat
        // write during the player's turn recomputes before the enemy acts).
        self.drain_recompute_queue();
        let pending = self.forward_actor_turn_start(actor);
        let action = self
            .pending_to_turn_action(actor, &pending)
            .unwrap_or(TurnAction::Attack(self.player));
        let from = self
            .ctx
            .position(actor)
            .map(|p| (p.x, p.y))
            .unwrap_or((0, 0));
        let mut report = {
            let mut sched = self.scheduler.lock().unwrap_or_else(|p| p.into_inner());
            sched.take_turn(&mut self.ctx, actor, action)?
        };
        // Progress rule for scheduled actors: an action the native pipeline
        // rejects (out of weapon reach, a wall, a blocking occupant) charges
        // no energy, so the scheduler would hand this actor the same turn
        // again on the next pass and the clock would never advance past it —
        // the player would never act again. A scheduled turn is always a step
        // of the clock, so an actor whose action was rejected waits that turn
        // out instead. (The player's own cancelled actions stay free: the
        // player is externally driven, so nothing spins on them.)
        if !report.charged {
            report = {
                let mut sched = self.scheduler.lock().unwrap_or_else(|p| p.into_inner());
                sched.take_turn(&mut self.ctx, actor, TurnAction::Wait)?
            };
        }
        // Post-turn derived-speed net (same as the player path).
        self.sync_derived_speeds();
        self.log.push(self.format_log_line(&report, from));
        // Nested native event lines for this actor's turn (same mapping as the
        // player path — a staggered goblin, for instance, logs no Attack line).
        self.log.extend(drain_event_lines(&self.event_lines));
        // Drain deaths observed during the turn once the scheduler lock is
        // released (the respawn policy mutates the scheduler).
        if let Some(b) = &self.bridge {
            b.drain_deaths();
        }
        // Forward the *resolved* action (the scheduler may have replaced it —
        // e.g. a staggered actor's skip turns an Attack into a Wait), so
        // scripted LATE handlers only see what actually happened.
        if self.bridge.is_some() {
            self.forward_scripted_action(actor, report.action);
        }
        Ok(report)
    }

    /// Translate the first turn-action pending for `actor` into a scheduler
    /// [`TurnAction`]. Step -> MoveTo; Attack -> resolve defender and Attack;
    /// Wander -> a native seeded walkable-empty neighbor step (or Wait when the
    /// actor is boxed in). Actions queued for other entities are skipped.
    fn pending_to_turn_action(
        &mut self,
        actor: Entity,
        pending: &[tessera_script::PendingAction],
    ) -> Option<TurnAction> {
        use tessera_script::PendingAction;
        let actor_id = entity_to_id(actor);
        for pa in pending {
            match pa {
                PendingAction::Step { entity, x, y } if *entity == actor_id => {
                    return Some(self.step_or_bump(actor, Point::new(*x, *y)));
                }
                PendingAction::Attack { attacker, defender } if *attacker == actor_id => {
                    if let Some(bridge) = &self.bridge
                        && let Some(e) = bridge.resolve_script_entity(*defender)
                    {
                        return Some(TurnAction::Attack(e));
                    }
                    if let Some(e) = Entity::from_bits(*defender) {
                        return Some(TurnAction::Attack(e));
                    }
                    return None;
                }
                PendingAction::Wander { entity } if *entity == actor_id => {
                    return Some(self.native_wander(actor));
                }
                _ => {}
            }
        }
        None
    }

    /// Build the turn action for `actor` entering `to`: a Caves-of-Qud bump. A
    /// live, attackable actor at the destination makes it an `Attack` on that
    /// defender (the mover does NOT move, and the attack costs the normal
    /// attack energy); any other target is a plain `MoveTo`, which a wall or a
    /// blocking non-combat occupant cancels downstream with no energy cost.
    /// Used by both the player's move action and the scripted goblin `Step`,
    /// so a goblin stepping onto the player bumps it instead of overlapping.
    fn step_or_bump(&self, actor: Entity, to: Point) -> TurnAction {
        match self.ctx.tile_entry(to, actor) {
            TileEntry::Bump(defender) => TurnAction::Attack(defender),
            _ => TurnAction::MoveTo(to),
        }
    }

    /// Native seeded wander primitive: pick one of the actor's walkable-empty
    /// neighbours (fixed order) via a deterministic per-entity fork of the
    /// sim RNG, or `Wait` when none exists. This is the "one native call"
    /// behind `engine.wander` (the host surfaces the request as a pending
    /// Wander action; the sim owns the seeded pick).
    fn native_wander(&mut self, actor: Entity) -> TurnAction {
        use tessera_core::events::Position;
        let Some(pos) = self.ctx.position(actor) else {
            return TurnAction::Wait;
        };
        // Fixed 8-neighbour order (row-major by dy, then dx).
        const NB: [(i32, i32); 8] = [
            (0, -1),
            (1, -1),
            (1, 0),
            (1, 1),
            (0, 1),
            (-1, 1),
            (-1, 0),
            (-1, -1),
        ];
        let zone = self.ctx.map.clone();
        let mut opts: Vec<(i32, i32)> = Vec::new();
        for (dx, dy) in NB {
            let (nx, ny) = (pos.x + dx, pos.y + dy);
            if !zone.is_walkable(nx, ny) {
                continue;
            }
            let b = self.ctx.broker.read().unwrap_or_else(|p| p.into_inner());
            let occ = b
                .entities_with_part(tessera_core::events::PART_POSITION)
                .iter()
                .any(|e| {
                    b.typed_part::<Position>(
                        *e,
                        &tessera_core::ecs::PartTypeId::new(tessera_core::events::PART_POSITION),
                    )
                    .map(|p| p.x == nx && p.y == ny)
                    .unwrap_or(false)
                });
            if !occ {
                opts.push((nx, ny));
            }
        }
        if opts.is_empty() {
            return TurnAction::Wait;
        }
        let mut rng = self.ctx.rng.fork(entity_to_id(actor));
        let i = rng.below(opts.len() as u32) as usize;
        let (ox, oy) = opts[i];
        TurnAction::MoveTo(Point::new(ox, oy))
    }

    /// Forward the actor's `ActorTurnStart` to the script host: the exposed
    /// turn hook (`engine.on("ActorTurnStart", ...)`) that lets a script
    /// policy react to any actor's turn with the read/nav API. Returns the
    /// turn actions the handlers queued (Step/Attack/Wander), so the caller can
    /// resolve the goblin's turn from script policy instead of a hardcoded one.
    fn forward_actor_turn_start(&mut self, actor: Entity) -> Vec<tessera_script::PendingAction> {
        let Some(bridge) = &self.bridge else {
            return Vec::new();
        };
        let host = bridge.host.clone();
        let actor_id = entity_to_id(actor);
        let mut params = int_params(&[("actor", actor_id as i64)]);
        if let Some(w) = bridge.world_state_id() {
            params.push(("world_state", mlua::Value::Integer(w as i64)));
        }
        // The player lands in the dispatch snapshot too, so handlers can read
        // the player's parts (e.g. the Shield Bash assertion reads the
        // attacker's native `Charges` pool via `engine.get_part`).
        if let Some(p) = bridge.player_id() {
            params.push(("player", mlua::Value::Integer(p as i64)));
        }
        let mut h = match host.lock() {
            Ok(h) => h,
            Err(_) => return Vec::new(),
        };
        let res = h.dispatch("ActorTurnStart", actor_id, &params);
        for line in h.drain_log() {
            self.log.push(line);
        }
        res.pending_actions
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
        let host = bridge.host.clone();
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
    /// `KEY <Event::NAME> tick=..` with the outcome for moves and ability
    /// uses.
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
            TurnAction::UseAbility { target } => {
                let outcome = if report.charged {
                    "handled"
                } else {
                    "cancelled"
                };
                format!(
                    "EVENT UseAbility tick={} target={} {}",
                    report.tick,
                    entity_to_id(target),
                    outcome
                )
            }
            TurnAction::Wait => format!("EVENT Wait tick={}", report.tick),
            _ => format!("EVENT {:?} tick={}", report.action, report.tick),
        }
    }

    /// The headless map text: terrain grid with every renderable entity's
    /// tile glyph drawn on top at its cell (deterministic entity-id order).
    /// The player keeps `@` (its `creature.player` glyph); the goblin is
    /// visibly `g` at its position — the demo's draw mapping, not just a log
    /// entry. A pure reader like [`Self::renderables`].
    pub fn text_map(&self, tiles: &TileRegistry) -> String {
        let floor = tile_glyph(tiles, "terrain.floor");
        let wall = tile_glyph(tiles, "terrain.wall");
        let width = self.ctx.map.width;
        let height = self.ctx.map.height;
        let mut grid: Vec<char> = vec![floor; width * height];
        for y in 0..height {
            for x in 0..width {
                if !self.ctx.map.is_walkable(x as i32, y as i32) {
                    grid[y * width + x] = wall;
                }
            }
        }
        for t in self.renderables() {
            let (x, y) = (t.x as usize, t.y as usize);
            if x < width && y < height {
                grid[y * width + x] = tile_glyph(tiles, &t.tile);
            }
        }
        let mut s = String::with_capacity(grid.len() + height);
        for y in 0..height {
            s.extend(&grid[y * width..(y + 1) * width]);
            s.push('\n');
        }
        s
    }

    /// One line per rendered entity for the headless demo readout — the text
    /// that names the map's glyphs and their live numbers:
    ///
    /// `@ creature.player id=3 (2,2) HP 20/36  Acc 0  Dodge 0  Def 0  Spd 98`
    ///
    /// Glyph, draw tile, id and position come from the live parts; `HP
    /// cur/max` plus the five derived stats come from the cached [`Derived`]
    /// part (absent on legacy entities, which print `HP <hp>` only).
    /// Read-only: never dispatches, never mutates.
    pub fn demo_entity_lines(&self, tiles: &TileRegistry) -> String {
        use tessera_core::effects::{Derived, PART_DERIVED};
        use tessera_core::events::{Health, PART_HEALTH, PART_POSITION, Position};
        use tessera_core::world::RenderTile;
        let b = self.ctx.broker.read().unwrap_or_else(|p| p.into_inner());
        let mut rows: Vec<(u64, String)> = b
            .entities_with_part(PART_POSITION)
            .into_iter()
            .filter_map(|e| {
                let id = e.to_bits().get();
                let pos = b.typed_part::<Position>(e, &PART_POSITION.into())?;
                let rt = b.world().get::<&RenderTile>(e).ok()?;
                let tile = rt.tile.clone();
                let glyph = match tiles.get(&tile).visual {
                    tessera_core::tiles::TileVisual::Glyph { ch } => ch,
                };
                let hp = b
                    .typed_part::<Health>(e, &PART_HEALTH.into())
                    .map(|h| h.hp)
                    .unwrap_or(0);
                let line = match b.typed_part::<Derived>(e, &tessera_core::ecs::PartTypeId::new(PART_DERIVED)) {
                    Some(d) => {
                        let max = d.values.get("max_hp").copied().unwrap_or(0);
                        let v = |k: &str| d.values.get(k).copied().unwrap_or(0);
                        format!(
                            "{glyph} {tile} id={id} ({},{}) HP {hp}/{max}  Acc {}  Dodge {}  Def {}  Spd {}",
                            pos.x,
                            pos.y,
                            v("accuracy"),
                            v("dodge"),
                            v("defense"),
                            v("speed")
                        )
                    }
                    None => format!("{glyph} {tile} id={id} ({},{}) HP {hp}", pos.x, pos.y),
                };
                Some((id, line))
            })
            .collect();
        rows.sort_by_key(|(id, _)| *id);
        if rows.is_empty() {
            return "(no renderable entities)".to_string();
        }
        rows.into_iter()
            .map(|(_, line)| line)
            .collect::<Vec<_>>()
            .join("\n")
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

/// Build one frame's draw cells: terrain everywhere, then every renderable
/// entity drawn on top at its cell. The player keeps its behavior — its
/// background falls through to the terrain tile under it (an entity tile with
/// no `bg` inherits the terrain's). A pure reader: no event dispatch, no sim
/// mutation; `targets` must already be in deterministic (entity-id) order.
pub fn build_cells(
    map: &Zone,
    tiles: &TileRegistry,
    atlas: &Atlas,
    targets: &[RenderTarget],
) -> Vec<Cell> {
    let mut cells: Vec<Option<Cell>> = vec![None; map.width * map.height];
    let mut set = |x: usize, y: usize, id: &str, under: Option<&Tile>| {
        let t = tiles.get(id);
        let TileVisual::Glyph { ch } = t.visual;
        let bg = t.bg.or(under.and_then(|u| u.bg)).unwrap_or(Rgb::BLACK);
        let (f, b) = (t.fg.to_f32(), bg.to_f32());
        cells[y * map.width + x] = Some(Cell {
            x: x as u32,
            y: y as u32,
            glyph: atlas.slot(ch),
            fg: [f[0], f[1], f[2], 1.0],
            bg: [b[0], b[1], b[2], 1.0],
        });
    };
    for y in 0..map.height {
        for x in 0..map.width {
            set(x, y, map.tile_id(x, y), None);
        }
    }
    for t in targets {
        let (x, y) = (t.x as usize, t.y as usize);
        if x >= map.width || y >= map.height {
            continue;
        }
        let under = tiles.get(map.tile_id(x, y)).clone();
        set(x, y, &t.tile, Some(&under));
    }
    cells
        .into_iter()
        .map(|c| c.expect("every cell is set after terrain + overlay"))
        .collect()
}

/// Deterministic headless action script. One 12-step cycle: `[right, right,
/// down, down, wait, bash, right, wait × 5]`. Within a few steps the player
/// walks into a map collision (the pillar at x%8==4 && y%6==3) exercising
/// the cancellation path, and exactly one turn uses the Shield Bash ability —
/// the headless event-log mapping (`UseAbility` → `TakeDamage(blunt)` →
/// `ApplyStatus(Status_Staggered)` in order). One bash per run keeps the
/// goblin's HP inside the single-bash declared range the assertion script
/// checks (a second bash would deal cumulative damage past one bash's range).
fn headless_script(i: usize) -> PlayerAction {
    match i % 12 {
        0 => PlayerAction::Move { dx: 1, dy: 0 },
        1 => PlayerAction::Move { dx: 1, dy: 0 },
        2 => PlayerAction::Move { dx: 0, dy: 1 },
        3 => PlayerAction::Move { dx: 0, dy: 1 },
        4 => PlayerAction::Wait,
        5 => PlayerAction::Bash,
        6 => PlayerAction::Move { dx: 1, dy: 0 },
        _ => PlayerAction::Wait,
    }
}

/// Take the queued native event lines out of the shared sink (called after
/// each turn so nested events appear in the sim log in causal order).
fn drain_event_lines(sink: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    std::mem::take(&mut *sink.lock().unwrap_or_else(|p| p.into_inner()))
}

/// The glyph for a tile id, falling back to `'?'`.
fn tile_glyph(tiles: &TileRegistry, id: &str) -> char {
    match tiles.get(id).visual {
        tessera_core::tiles::TileVisual::Glyph { ch } => ch,
    }
}

/// The demo world's default content-level handlers, re-registered on EVERY sim
/// construction (zone start and save/load alike). Handlers capture *wiring*,
/// never saved state, so a loaded world re-runs this exact registration:
///
/// - a `Move` collision + occupancy cancel (no actor ever enters a wall or
///   another actor's tile);
/// - the reference demo combat (Attack → base TakeDamage → Health.hp);
/// - the native effect runtime (UseAbility/ApplyStatus declarative
///   resolution) when the bridge's content library is present, so the
///   campaign's Shield Bash works in the demo;
/// - the headless event-log observers (`TakeDamage`/`ApplyStatus` lines);
/// - when a script bridge is present, forward the reserved `Death` event to the
///   host so scripts can respond (respawn/policy).
fn install_default_handlers(
    ctx: &mut SimContext,
    bridge: Option<&ScriptBridge>,
    event_lines: Arc<Mutex<Vec<String>>>,
) {
    // Content-level collision + occupancy: cancel any Move whose target is
    // not walkable, or whose target is already occupied by a live actor other
    // than the mover. This is the shipped cancellation handler.
    ctx.register::<Move>(EARLY, |ctx, ev| {
        if !ctx.map.is_walkable(ev.to.x, ev.to.y) {
            return HandlerAction::Cancel;
        }
        // Occupancy: at most one actor per tile. The mover is excluded.
        match ctx.tile_entry(ev.to, ev.actor) {
            TileEntry::Clear => HandlerAction::Continue,
            _ => HandlerAction::Cancel,
        }
    });
    // Reference demo combat: the ordered attack pipeline (legality ->
    // BeforeHit -> hit/block/damage rolls -> Hit/Missed/Blocked ->
    // TakeDamage), and a TakeDamage that always reduces `Health.hp`. This is
    // the native half the Zapper's scripted zap stacks on.
    let combat_tuning = bridge
        .and_then(|b| b.world())
        .and_then(|w| w.tuning_values(tessera_core::combat::TUNING_TABLE))
        .and_then(|values| tessera_core::combat::CombatTuning::from_values(&values).ok())
        .unwrap_or_default();
    install_base_combat(ctx, combat_tuning);
    // The headless event-log mapping: deterministic `EVENT TakeDamage` /
    // `EVENT ApplyStatus` lines for the declarative ability path (the
    // `EVENT UseAbility` turn line is logged by the sim on the action turn).
    install_event_logging(ctx, event_lines);
    // Native effect runtime (declarative abilities/statuses): resolvers read
    // the shared content library, so reloads (in-place library swap) change
    // the definitions new applications resolve against without re-registration.
    if let Some(b) = bridge
        && let Some(world) = b.world()
    {
        tessera_core::effects::install_native_effects(ctx, world.library().clone());
    }
    // Forward the reserved `Death` event (dispatched by the native TakeDamage
    // applicator when hp clamps to zero) to the script host, so scripts can
    // react with respawn/policy (the turn hook contract).
    if let Some(b) = bridge {
        let fwd = b.clone();
        ctx.register_observer::<tessera_core::events::Death>(LATE, move |_ctx, ev| {
            // Defer forwarding: the respawn policy's spawn/despawn mutate the
            // scheduler, and this observer runs *under* the scheduler lock
            // during take_turn. Queue it; the sim drains post-turn.
            fwd.push_death(entity_to_id(ev.target));
        });
    }
}

/// The named `--headless` event-log mapping: LATE observers that queue one
/// deterministic line per dispatched `TakeDamage` / `ApplyStatus` /
/// `RecomputeDerived` event into the shared sink (drained into the sim log at
/// the end of the turn, so the nested events of one `UseAbility` appear in
/// causal order after the turn's own line). `UseAbility` itself is logged by
/// the turn line.
fn install_event_logging(ctx: &mut SimContext, event_lines: Arc<Mutex<Vec<String>>>) {
    let sink = event_lines.clone();
    ctx.register_observer::<tessera_core::events::TakeDamage>(LATE, move |_ctx, ev| {
        sink.lock().unwrap_or_else(|p| p.into_inner()).push(format!(
            "EVENT TakeDamage target={} amount={} dtype={}",
            entity_to_id(ev.target),
            ev.amount,
            ev.dtype
        ));
    });
    let sink = event_lines.clone();
    ctx.register_observer::<tessera_core::events::ApplyStatus>(LATE, move |_ctx, ev| {
        sink.lock().unwrap_or_else(|p| p.into_inner()).push(format!(
            "EVENT ApplyStatus target={} status={}",
            entity_to_id(ev.target),
            ev.status
        ));
    });
    // The attack-pipeline headless mapping: one line per resolution step, so a
    // demo run's log shows the ordered pipeline (hit roll and its chance, the
    // crit flag, a block, a miss) ahead of the damage line.
    let sink = event_lines.clone();
    ctx.register_observer::<tessera_core::events::BeforeHit>(LATE, move |_ctx, ev| {
        sink.lock().unwrap_or_else(|p| p.into_inner()).push(format!(
            "EVENT BeforeHit attacker={} defender={} hit_pct={}",
            entity_to_id(ev.attacker),
            entity_to_id(ev.defender),
            ev.hit_pct
        ));
    });
    let sink = event_lines.clone();
    ctx.register_observer::<tessera_core::events::Hit>(LATE, move |_ctx, ev| {
        sink.lock().unwrap_or_else(|p| p.into_inner()).push(format!(
            "EVENT Hit attacker={} defender={} roll={} hit_pct={} crit={}",
            entity_to_id(ev.attacker),
            entity_to_id(ev.defender),
            ev.roll,
            ev.hit_pct,
            ev.crit
        ));
    });
    let sink = event_lines.clone();
    ctx.register_observer::<tessera_core::events::Missed>(LATE, move |_ctx, ev| {
        sink.lock().unwrap_or_else(|p| p.into_inner()).push(format!(
            "EVENT Missed attacker={} defender={} roll={} hit_pct={}",
            entity_to_id(ev.attacker),
            entity_to_id(ev.defender),
            ev.roll,
            ev.hit_pct
        ));
    });
    let sink = event_lines.clone();
    ctx.register_observer::<tessera_core::events::Blocked>(LATE, move |_ctx, ev| {
        sink.lock().unwrap_or_else(|p| p.into_inner()).push(format!(
            "EVENT Blocked attacker={} defender={} roll={} block_pct={}",
            entity_to_id(ev.attacker),
            entity_to_id(ev.defender),
            ev.roll,
            ev.block_pct
        ));
    });
    // The stats headless demo mapping: one deterministic line per recompute, so
    // the event log shows `RecomputeDerived` after spawn and after each
    // dirtying event (Status_Staggered, part writes, tuning reloads).
    let sink = event_lines.clone();
    ctx.register_observer::<RecomputeDerived>(LATE, move |_ctx, ev| {
        sink.lock().unwrap_or_else(|p| p.into_inner()).push(format!(
            "EVENT RecomputeDerived entity={}",
            entity_to_id(ev.entity)
        ));
    });
}

/// The rule name a stat-ful entity's formula owner must register under; the
/// engine looks up `rules/stats.luau` in the loaded library at install time
/// .
pub const RULES_RECOMPUTE_EVENT: &str = "RecomputeDerived";

/// Register the stats recompute pipeline's native handling of `RecomputeDerived`
/// on `ctx` (called at sim install alongside `install_native_effects`):
///
/// 1. **invalidate first** — the native `invalidate_derived` hook
///    clears the cache so a failed recompute leaves an absent cache, never a
///    stale one ( step 5);
/// 2. **rules forward** — when a scripted `RecomputeDerived` handler (the
///    registered `rules/stats.luau`) exists, forward the event to the host:
///    the script computes the five formula bases from the settled snapshot
///    and writes them via the whitelisted pending `set_part` path, which the
///    host applies before this handler continues;
/// 3. **native finalize** — `finalize_derived` applies live status modifiers
///    , the invariant clamps  and the `Health.hp <= max_hp`
///    clamp ;
/// 4. **cache-only speed sync** — the scheduler's actor row reads speed from
///    the cache (`derived_or_energy_speed`); no read path ever calls Luau.
///
/// Entities without `Attributes` are left untouched (legacy behavior, ).
/// Returns the handler id so a rules reload can unregister and replace it.
fn install_recompute_derived(
    ctx: &mut SimContext,
    bridge: Option<&ScriptBridge>,
    scheduler: Arc<Mutex<Scheduler>>,
) -> HandlerId {
    let fwd = bridge.cloned();
    let sched = scheduler.clone();
    ctx.register::<RecomputeDerived>(NORMAL, move |ctx, ev| {
        let entity = ev.entity;
        // Step 1: clear the cache first. Absent is detectable; stale
        // is a lie.
        tessera_core::effects::invalidate_derived(ctx, entity);

        // Legacy entities (no `Attributes`) recompute nothing: absent cache,
        // `ActorEnergy.speed` fallback — exactly the pre-derivation behavior.
        let statful = {
            let broker = ctx.broker.read().unwrap_or_else(|p| p.into_inner());
            tessera_core::recompute::carries_attributes(&broker, entity)
        };
        if !statful {
            return HandlerAction::Continue;
        }

        // Step 2: rules forward (only when a rules handler is registered; an
        // unarmed engine skips the script call and leaves the cache absent —
        // recompute_live/spawn seams gate stat-ful spawns fail-closed).
        if let Some(b) = &fwd
            && b.has_event_handler(RULES_RECOMPUTE_EVENT)
        {
            let id = entity_to_id(entity);
            let mut host = b.host.lock().unwrap_or_else(|p| p.into_inner());
            let _ = host.dispatch(RULES_RECOMPUTE_EVENT, id, &int_params(&[]));
            // The host's dispatch already applied the script's pending
            // set_part (the cache write). Drain any engine.log lines so they
            // never leak into a later forward's drain out of causal order.
            let _ = host.drain_log();
        }

        // Step 3+4: native finalize and cache-only speed sync. The scheduler
        // may already be locked by the turn loop that is dispatching this
        // recompute (status dirtying runs under `take_turn`); `try_lock`
        // skips the inline sync in that case and the sim's post-turn
        // `sync_derived_speeds` net brings the rows up to date right after
        // the lock is released.
        tessera_core::recompute::finalize_derived(ctx, entity);
        let speed = tessera_core::recompute::derived_or_energy_speed(ctx, entity);
        if let Ok(mut s) = sched.try_lock() {
            s.set_speed(entity, speed);
        }
        HandlerAction::Continue
    })
}

/// Register the reference demo combat: an `Attack` on a Health-carrying
/// defender resolves to `TakeDamage` (base hit, `NORMAL`), and a `TakeDamage`
/// always reduces `Health.hp` by its amount. The base hit is the native half
/// of the Zapper demo's damage; the scripted zap (LATE) stacks on top.
///
/// When a `TakeDamage` clamps `hp` to zero, the native applicator dispatches
/// the reserved [`Death`] event exactly once: the "was_alive" guard means a
/// second hit on an already-dead entity (`hp == 0` at entry) does not re-emit.
fn install_base_combat(ctx: &mut SimContext, tuning: tessera_core::combat::CombatTuning) {
    use tessera_core::events::{Death, Health, PART_HEALTH};
    // The ordered attack pipeline: `Attack` legality (EARLY) and resolution
    // (NORMAL) — hit, block, damage rolls on the named combat stream.
    tessera_core::combat::install_combat(ctx, tuning);
    // TakeDamage applicator: the actual native damage the base hit inflicts.
    ctx.register::<TakeDamage>(NORMAL, |ctx, ev| {
        let mut reduce = false;
        {
            let b = ctx.broker.read().unwrap_or_else(|p| p.into_inner());
            if let Some(h) = b.typed_part::<Health>(ev.target, &PART_HEALTH.into())
                && h.hp > 0
            {
                reduce = true;
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
    use tessera_core::events::{Health, PART_HEALTH, PART_POSITION, Position};
    use tessera_core::turns::TURN_THRESHOLD;
    use tessera_core::zone::Zone;

    /// The demo room the fixture() runs in: bordered all-floor grid with a pillar
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
        let line = &sim.log[0];
        assert!(line.contains("Move") && line.contains("handled"), "{line}");
    }

    #[test]
    fn wall_blocked_move_is_cancelled_no_position_change() {
        let mut sim = fixture();
        // (0,?) is out of bounds to the left -> wall block at EARLY.
        sim.submit_action(PlayerAction::Move { dx: -1, dy: 0 })
            .unwrap();
        assert_eq!(sim.position(), (1, 1), "cancelled move leaves position");
        let line = &sim.log[0];
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
        assert!(sim.log[0].contains("Wait"), "{}", sim.log[0]);
    }

    #[test]
    fn headless_run_is_deterministic_and_covers_all_paths() {
        let tiles = TileRegistry::new();
        let run = || {
            let mut sim = fixture();
            let map = sim.run_headless(&tiles, 12);
            (map, sim.log.to_vec())
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
        for line in sim.log {
            assert!(line.starts_with("EVENT "), "unexpected log line: {line}");
        }
    }

    // ---- Caves-of-Qud bump combat ------------------------------------------

    /// A fixture() with a living player (Health 20) and an adjacent live goblin
    /// (Health `goblin_hp`) in the demo room. Only the player is scheduled
    /// (a goblin's independent turns would inject nondeterminism into these
    /// focused tests). Returns the sim, the player, and the goblin entity.
    fn combat_fixture(goblin_hp: i32) -> (Sim, hecs::Entity, hecs::Entity) {
        use tessera_core::ecs::Part;
        let mut broker = EntityBroker::new();
        let player = broker.spawn();
        broker
            .insert_part(player, PART_ENERGY, Box::new(ActorEnergy::new(100)))
            .unwrap();
        broker
            .insert_part(
                player,
                PART_POSITION,
                Box::new(Position { x: 2, y: 2 }) as Box<dyn Part>,
            )
            .unwrap();
        broker
            .insert_part(
                player,
                PART_HEALTH,
                Box::new(Health { hp: 20 }) as Box<dyn Part>,
            )
            .unwrap();
        let goblin = broker.spawn();
        broker
            .insert_part(goblin, PART_ENERGY, Box::new(ActorEnergy::new(100)))
            .unwrap();
        broker
            .insert_part(
                goblin,
                PART_POSITION,
                Box::new(Position { x: 3, y: 2 }) as Box<dyn Part>,
            )
            .unwrap();
        broker
            .insert_part(
                goblin,
                PART_HEALTH,
                Box::new(Health { hp: goblin_hp }) as Box<dyn Part>,
            )
            .unwrap();
        let mut sim = Sim::new(broker, demo_zone(20, 10), player);
        sim.log.clear();
        (sim, player, goblin)
    }

    #[test]
    fn move_onto_health_actor_bumps_and_charges_attack_energy() {
        let (mut sim, _player, _goblin) = combat_fixture(3);
        let report = sim
            .submit_action(PlayerAction::Move { dx: 1, dy: 0 })
            .unwrap();
        // (2,2) -> (3,2) is the goblin's tile: it becomes an Attack, not a move.
        assert_eq!(sim.position(), (2, 2), "a bump must NOT move the mover");
        assert!(
            report.charged,
            "a bump attack charges the normal attack energy"
        );
        assert_eq!(
            report.energy_after, 0,
            "one normal attack costs exactly one threshold from fresh energy"
        );
        assert!(
            sim.log.iter().any(|l| l.contains("EVENT Attack")),
            "expected an Attack log line, got {:?}",
            sim.log
        );
    }

    #[test]
    fn move_onto_non_health_blocking_actor_is_cancelled_free() {
        use tessera_core::ecs::Part;
        let mut broker = EntityBroker::new();
        let player = broker.spawn();
        broker
            .insert_part(player, PART_ENERGY, Box::new(ActorEnergy::new(100)))
            .unwrap();
        broker
            .insert_part(
                player,
                PART_POSITION,
                Box::new(Position { x: 2, y: 2 }) as Box<dyn Part>,
            )
            .unwrap();
        // A blocking actor that carries a Position but no Health (a non-combat
        // obstacle), standing where the player tries to move.
        let blocker = broker.spawn();
        broker
            .insert_part(
                blocker,
                PART_POSITION,
                Box::new(Position { x: 3, y: 2 }) as Box<dyn Part>,
            )
            .unwrap();
        let mut sim = Sim::new(broker, demo_zone(20, 10), player);
        sim.log.clear();

        let report = sim
            .submit_action(PlayerAction::Move { dx: 1, dy: 0 })
            .unwrap();
        assert_eq!(sim.position(), (2, 2), "blocked move leaves position");
        assert!(!report.charged, "a blocked move costs no energy");
        assert_eq!(report.energy_after, TURN_THRESHOLD, "energy untouched");
        let line = &sim.log[0];
        assert!(
            line.contains("Move") && line.contains("cancelled"),
            "{line}"
        );
    }

    #[test]
    fn scripted_step_onto_player_becomes_bump_attack() {
        let (mut sim, player, goblin) = combat_fixture(3);
        // A scripted goblin step aimed at the player's tile (2,2) resolves as
        // an Attack on the player, never an overlapping move.
        let goblin_id = crate::scripting::entity_to_id(goblin);
        let action = sim.pending_to_turn_action(
            goblin,
            &[tessera_script::PendingAction::Step {
                entity: goblin_id,
                x: 2,
                y: 2,
            }],
        );
        assert_eq!(action, Some(TurnAction::Attack(player)));
    }

    // ---- save/load round-trip ------------------------------------------

    /// Hooks covering the whitelist (Position/Health/ActorEnergy/Stats native,
    /// registered unconditionally by `build_save_hooks`) plus a `Charge` script
    /// part, mirroring the shipped demo. `build_save_hooks` only uses the
    /// registry to identify script-part names, so we register just the script
    /// schema.
    fn roundtrip_hooks() -> SaveHooks {
        use std::collections::BTreeMap;
        use tessera_core::ecs::PartRegistry;
        use tessera_core::script_part::{FieldDef, PartValue, ScriptPartType};
        let mut reg = PartRegistry::new();
        let mut schema = BTreeMap::new();
        schema.insert(
            "charges".to_string(),
            FieldDef::int().default(PartValue::Int(1)),
        );
        reg.register_script_part(ScriptPartType::new("Charge", schema))
            .unwrap();
        tessera_core::save::build_save_hooks(&reg).0
    }

    /// A populated boxed world: player + scheduled goblin carrying Health,
    /// Position, ActorEnergy and a `Charge` script part. Returns the sim and its
    /// player/goblin entities (scheduled together, no script bridge — native
    /// deterministic behavior only).
    fn populated_sim(hp: i32) -> (Sim, hecs::Entity, hecs::Entity) {
        use tessera_core::ecs::Part;
        use tessera_core::script_part::ScriptPart;
        let mut broker = EntityBroker::new();
        let player = broker.spawn();
        broker
            .insert_part(player, PART_ENERGY, Box::new(ActorEnergy::new(100)))
            .unwrap();
        broker
            .insert_part(
                player,
                PART_POSITION,
                Box::new(Position { x: 2, y: 2 }) as Box<dyn Part>,
            )
            .unwrap();
        broker
            .insert_part(
                player,
                PART_HEALTH,
                Box::new(Health { hp: 20 }) as Box<dyn Part>,
            )
            .unwrap();
        broker
            .world_mut()
            .insert_one(
                player,
                RenderTile {
                    tile: "creature.player()".into(),
                },
            )
            .unwrap();
        let goblin = broker.spawn();
        broker
            .insert_part(goblin, PART_ENERGY, Box::new(ActorEnergy::new(100)))
            .unwrap();
        broker
            .insert_part(
                goblin,
                PART_POSITION,
                Box::new(Position { x: 3, y: 2 }) as Box<dyn Part>,
            )
            .unwrap();
        broker
            .insert_part(
                goblin,
                PART_HEALTH,
                Box::new(Health { hp }) as Box<dyn Part>,
            )
            .unwrap();
        broker
            .world_mut()
            .insert_one(
                goblin,
                RenderTile {
                    tile: "creature.goblin".into(),
                },
            )
            .unwrap();
        let mut sp = ScriptPart::default();
        sp.insert(
            "charges".into(),
            tessera_core::script_part::PartValue::Int(3),
        );
        broker.insert_part(goblin, "Charge", Box::new(sp)).unwrap();
        let zone = demo_zone(20, 10);
        // Shared broker so the goblin is scheduled alongside the player (enemy).
        let broker = Arc::new(RwLock::new(broker));
        let sim = Sim::from_shared(broker, None, zone, player, Some(goblin));
        (sim, player, goblin)
    }

    #[test]
    fn save_load_round_trip_preserves_state_and_continues_identically() {
        let hooks = roundtrip_hooks();
        let (mut sim, _player, _goblin) = populated_sim(3);

        // Play a deterministic prefix: a couple of player moves (some wall
        // blocked, one handled) and waits.
        for a in [
            PlayerAction::Move { dx: 1, dy: 0 },
            PlayerAction::Move { dx: 1, dy: 0 },
            PlayerAction::Wait,
            PlayerAction::Move { dx: -1, dy: 0 }, // wall-blocked
        ] {
            sim.submit_action(a).unwrap();
        }
        let before = sim.save(&hooks).expect("save");
        let log_len_at_save = sim.log.len();
        let renderables_before = sim.renderables();

        // Build an identical-controls world and run it the same way, from the
        // same fresh state, and confirm the pre-save logs match (so the two
        // worlds are deterministically comparable before we continue).
        let (mut reference, _, _) = populated_sim(3);
        for a in [
            PlayerAction::Move { dx: 1, dy: 0 },
            PlayerAction::Move { dx: 1, dy: 0 },
            PlayerAction::Wait,
            PlayerAction::Move { dx: -1, dy: 0 },
        ] {
            reference.submit_action(a).unwrap();
        }
        assert_eq!(
            reference.log, sim.log,
            "two same-built worlds match pre-save"
        );

        // Load the save into a fresh sim.
        let zone = demo_zone(20, 10);
        let mut loaded = Sim::load(before, zone, &hooks, None).expect("load");
        assert_eq!(loaded.renderables(), renderables_before);

        // State restored: same entity ids in same (deterministic) order.
        let orig_ids: Vec<u64> = sim
            .ctx
            .broker
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .entities()
            .into_iter()
            .map(|e| e.to_bits().get())
            .collect();
        let loaded_ids: Vec<u64> = loaded
            .ctx
            .broker
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .entities()
            .into_iter()
            .map(|e| e.to_bits().get())
            .collect();
        assert_eq!(loaded_ids, orig_ids, "entity ids restored in exact order");
        assert_eq!(
            loaded.position(),
            sim.position(),
            "player position restored"
        );
        assert_eq!(
            loaded.player_turns_public(),
            sim.player_turns_public(),
            "player turn clock restored"
        );

        // Continue both worlds with the same script; the loaded world's fresh
        // log must exactly equal the original's post-save continuation.
        let continue_script = [
            PlayerAction::Move { dx: 0, dy: 1 },
            PlayerAction::Wait,
            PlayerAction::Move { dx: 1, dy: -1 },
            PlayerAction::Wait,
            PlayerAction::Move { dx: 0, dy: 1 },
        ];
        for a in continue_script {
            reference.submit_action(a).unwrap();
            loaded.submit_action(a).unwrap();
        }
        assert_eq!(
            &reference.log[log_len_at_save..],
            loaded.log,
            "loaded world continues with an identical event log"
        );
    }

    #[test]
    fn zone_mismatch_is_rejected() {
        let hooks = roundtrip_hooks();
        let (mut sim, _p, _g) = populated_sim(3);
        for a in [PlayerAction::Wait, PlayerAction::Move { dx: 1, dy: 0 }] {
            sim.submit_action(a).unwrap();
        }
        let data = sim.save(&hooks).expect("save");
        // Wrong zone id: the save belongs to "test" but we load into "other".
        let other = Zone::from_cells("other", 20, 10, Point::new(1, 1), {
            let mut cells = vec!["terrain.floor".to_string(); 20 * 10];
            for y in 0..10 {
                for x in 0..20 {
                    if x == 0 || y == 0 || x == 19 || y == 9 {
                        cells[y * 20 + x] = "terrain.wall".to_string();
                    }
                }
            }
            cells
        });
        match Sim::load(data, other, &hooks, None) {
            Err(SaveError::ZoneMismatch { saved, current }) => {
                assert_eq!(saved, "test");
                assert_eq!(current, "other");
            }
            Ok(_) => panic!("expected ZoneMismatch, but load succeeded"),
            Err(e) => panic!("expected ZoneMismatch, got {e:?}"),
        }
    }

    #[test]
    fn save_fails_closed_on_unknown_part() {
        use tessera_core::ecs::Part;
        let hooks = roundtrip_hooks();
        let mut broker = EntityBroker::new();
        let player = broker.spawn();
        broker
            .insert_part(player, PART_ENERGY, Box::new(ActorEnergy::new(100)))
            .unwrap();
        broker
            .insert_part(
                player,
                PART_POSITION,
                Box::new(Position { x: 1, y: 1 }) as Box<dyn Part>,
            )
            .unwrap();
        // A part outside the whitelist: the snapshot must fail closed.
        broker
            .insert_part(player, "NotWhitelisted", Box::new(1u32))
            .unwrap();
        let sim = Sim::new(broker, demo_zone(20, 10), player);
        match sim.save(&hooks) {
            Err(SaveError::UnserializablePart { name }) => assert_eq!(name, "NotWhitelisted"),
            other => panic!("expected UnserializablePart, got {other:?}"),
        }
    }

    #[test]
    fn rng_state_round_trips_after_save() {
        let hooks = roundtrip_hooks();
        let (mut sim, _p, _g) = populated_sim(5);
        for a in [
            PlayerAction::Wait,
            PlayerAction::Wait,
            PlayerAction::Move { dx: 1, dy: 0 },
        ] {
            sim.submit_action(a).unwrap();
        }
        let before = sim.save(&hooks).expect("save");
        let loaded = Sim::load(before.clone(), demo_zone(20, 10), &hooks, None).expect("load");
        // The ChaCha8 stream state round-trips through the save: post-load draws
        // match the pre-save continuation.
        let mut orig = before.rng.clone();
        let mut rel = loaded.ctx.rng.clone();
        for _ in 0..8 {
            assert_eq!(
                orig.next_u32(),
                rel.next_u32(),
                "RNG stream continues identically"
            );
        }
        // The named per-system streams are part of the snapshot too. The
        // goblin's attacks draw on the "combat" stream, so if the prefix
        // trained one, the load must resume it at the same word position —
        // otherwise a reloaded mid-fight world would replay different rolls
        // even though the parent stream matched.
        let label = tessera_core::combat::COMBAT_STREAM;
        let saved_stream = before
            .streams
            .get(label)
            .unwrap_or_else(|| panic!("the prefix must have drawn on {label:?}"));
        let restored = loaded
            .ctx
            .streams()
            .get(label)
            .unwrap_or_else(|| panic!("the loaded context must carry {label:?}"));
        assert_eq!(
            saved_stream, restored,
            "named stream state survives the save"
        );
        let mut orig_stream = saved_stream.clone();
        let mut rel_stream = restored.clone();
        for _ in 0..8 {
            assert_eq!(
                orig_stream.next_u32(),
                rel_stream.next_u32(),
                "named combat stream continues identically"
            );
        }
    }
}
