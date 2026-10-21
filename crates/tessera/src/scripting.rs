//! Script integration: bridge the `tessera-script` host's [`WorldAccess`] to
//! the live sim broker, and drive script handlers from within the native event
//! bus.
//!
//! The broker is shared via `Arc<RwLock<EntityBroker>>` (the same handle the
//! [`SimContext`] was built with), so script snapshots and native handlers
//! observe the *same* live entity state — the "one dispatch path" seam.
//! Entity ids cross the boundary as `Entity::to_bits().get()` values
//! and are recovered with [`hecs::Entity::from_bits`], so no side table is
//! needed.
//!
//! The world slice adds a **shared sim snapshot** ([`SharedSim`]) the native
//! sim populates after construction (zone, scheduler, player id, player-turn
//! clock, blueprint world). The script bridge holds it and both implements the
//! `WorldAccess` apply seam (`set_part`, `spawn_at`, ...) and publishes a
//! [`tessera_script::WorldRead`] handle into every `engine.*` closure, so
//! scripts answer map/nav queries with one native call each — the "no map
//! loops in scripts" rule. Everything is a pure read returning owned data; no
//! borrow of the world crosses into Luau (the value boundary).
//!
//! Script-declared schemas are registered into the host's own registry at
//! script load (via `engine.define_part`); this module additionally mirrors
//! every host-registered script part into the [`World`] registry so that
//! blueprint `parts.<ScriptPart>` keys validate against the same registry the
//! loader checks (this is a hard ordering requirement: the world registry must
//! know `Charge` before `zapper.toml`'s `parts.Charge` is validated).

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::Result;
use hecs::Entity;
use tessera_core::ecs::{EntityBroker, Part, PartTypeId};
use tessera_core::events::Health;
use tessera_core::kinds::TuningValue;
use tessera_core::script_part::{PartValue, ScriptPart};
use tessera_core::turns::Scheduler;
use tessera_core::world::World;
use tessera_core::zone::Zone;
use tessera_script::{HostRng, ScriptError, ScriptHost, WorldAccess, WorldRead};

/// Live, shared simulation state both the native sim and the script bridge's
/// read/apply impls reference. Populated by [`crate::sim::Sim`] after it is
/// built (the zone, scheduler and player are not known earlier), except for
/// `world`, which the loader sets once blueprints are ready.
///
/// All fields are interior-mutable (`AtomicU64` / `RwLock`) so the state is
/// cheaply shared as an `Arc` between the host's writer box and the `WorldRead`
/// handle published into engine closures.
pub struct SharedSim {
    /// The zone currently simulated (read-only after load).
    zone: RwLock<Option<Zone>>,
    /// The sim's energy-clock scheduler, shared so a `spawn_at` actor with
    /// `ActorEnergy` lands on the *same* schedule the sim drives.
    scheduler: RwLock<Option<Arc<Mutex<Scheduler>>>>,
    /// The demo player's entity id, as script-facing bits (0 = none).
    player: AtomicU64,
    /// The sim's player-turn clock (increments once per submitted player
    /// action, cancelled or not).
    player_turns: AtomicU64,
    /// Script-facing id of the world-state singleton entity (the respawn /
    /// policy anchor scripts read and write parts on it), 0 = not installed.
    world_state: AtomicU64,
    /// The blueprint `World` used to resolve and stamp `engine.spawn_at`.
    world: RwLock<Option<Arc<World>>>,
    /// Script-facing entity id -> real broker entity bits. `engine.spawn_at`
    /// hands a script a host-assigned id; the actual entity is created by the
    /// broker at its own sequential low id (hecs ids are bounded and can't be
    /// forced high), so reads translate through this map. Deterministic
    /// (`BTreeMap`).
    script_entities: RwLock<BTreeMap<u64, u64>>,
    /// Death events observed while a scheduler turn held its lock. Forwarded to
    /// the host only after the lock is released (the respawn policy's
    /// `spawn_at`/`despawn` re-lock the scheduler, so forwarding under the lock
    /// would deadlock).
    pending_deaths: Mutex<Vec<u64>>,
}

impl SharedSim {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the pieces known only after the sim is constructed. `scheduler` is
    /// the *same* `Arc<Mutex<Scheduler>>` the sim drives, so scripted spawns
    /// register on the live clock.
    pub fn set_world_state(&self, zone: Zone, scheduler: Arc<Mutex<Scheduler>>, player: Entity) {
        *self.zone.write().unwrap_or_else(|p| p.into_inner()) = Some(zone);
        *self.scheduler.write().unwrap_or_else(|p| p.into_inner()) = Some(scheduler);
        self.player.store(entity_to_id(player), Ordering::Relaxed);
    }

    /// Install the blueprint `World` once it is loaded.
    pub fn set_world(&self, world: Arc<World>) {
        *self.world.write().unwrap_or_else(|p| p.into_inner()) = Some(world);
    }

    pub fn read_zone(&self) -> Zone {
        self.zone
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .unwrap_or_default()
    }

    /// The shared scheduler handle, if installed. Callers lock it to add or
    /// remove actors (the scripted spawn/despawn path).
    pub fn scheduler(&self) -> Option<Arc<Mutex<Scheduler>>> {
        self.scheduler
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    pub fn read_world(&self) -> Option<Arc<World>> {
        self.world.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Record a script-facing id -> real broker entity id mapping established
    /// by a successful scripted `spawn_at`.
    pub fn map_script_entity(&self, script_id: u64, real: u64) {
        self.script_entities
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .insert(script_id, real);
    }

    /// Remove a script id mapping when its real entity is despawned.
    pub fn unmap_script_entity(&self, script_id: u64) {
        self.script_entities
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&script_id);
    }

    /// Translate a script-facing entity id (one a script learned from
    /// `engine.spawn_at`) to the real broker entity, if it exists.
    pub fn resolve_script_entity(&self, script_id: u64) -> Option<Entity> {
        let mapped = self
            .script_entities
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .get(&script_id)
            .copied()?;
        Entity::from_bits(mapped)
    }

    pub fn player(&self) -> Option<u64> {
        let p = self.player.load(Ordering::Relaxed);
        (p != 0).then_some(p)
    }

    pub fn player_turns(&self) -> u64 {
        self.player_turns.load(Ordering::Relaxed)
    }

    pub fn set_player_turns(&self, n: u64) {
        self.player_turns.store(n, Ordering::Relaxed);
    }

    /// Install the script-facing id of the world-state singleton entity (the
    /// respawn anchor), set once at load wiring.
    pub fn set_world_state_id(&self, id: u64) {
        self.world_state.store(id, Ordering::Relaxed);
    }

    pub fn world_state(&self) -> Option<u64> {
        let w = self.world_state.load(Ordering::Relaxed);
        (w != 0).then_some(w)
    }

    /// Update the shared player id (used when a scripted `Player` respawn
    /// replaces the live player entity).
    pub fn set_player_id(&self, id: u64) {
        self.player.store(id, Ordering::Relaxed);
    }

    /// Queue a death for forwarding once the scheduler lock is released.
    pub fn push_death(&self, id: u64) {
        self.pending_deaths
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(id);
    }

    /// Drain the queued deaths (caller must not hold the scheduler lock).
    pub fn drain_deaths(&self) -> Vec<u64> {
        self.pending_deaths
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain(..)
            .collect()
    }

    /// The current occupancy predicate over the shared broker: a `Position`
    /// part whose cell matches `(x, y)`.
    pub fn occupied(&self, broker: &EntityBroker, x: i32, y: i32) -> bool {
        broker
            .entities_with_part(tessera_core::events::PART_POSITION)
            .iter()
            .any(|e| {
                broker
                    .typed_part::<tessera_core::events::Position>(
                        *e,
                        &PartTypeId::new(tessera_core::events::PART_POSITION),
                    )
                    .map(|p| p.x == x && p.y == y)
                    .unwrap_or(false)
            })
    }
}

impl Default for SharedSim {
    fn default() -> Self {
        Self {
            zone: RwLock::new(None),
            scheduler: RwLock::new(None),
            player: AtomicU64::new(0),
            player_turns: AtomicU64::new(0),
            world_state: AtomicU64::new(0),
            world: RwLock::new(None),
            script_entities: RwLock::new(BTreeMap::new()),
            pending_deaths: Mutex::new(Vec::new()),
        }
    }
}

/// A [`WorldAccess`] over the live, shared broker + [`SharedSim`] snapshot.
/// Immutable reads lock the broker's read guard per call; mutations take the
/// write guard per call. No handles must be held across a call into the host
/// (value boundary).
#[derive(Clone)]
struct SimWorldAccess {
    broker: Arc<RwLock<EntityBroker>>,
    shared: Arc<SharedSim>,
}

impl SimWorldAccess {
    /// Serialize a part's visible fields into the plain `PartValue` form the
    /// host snapshot and `engine.get_part` consume. A scripted `ScriptPart`
    /// returns its closed field map; the demo's writable native part
    /// (`Health`) returns its `hp` field; the S0 effect runtime's native
    /// parts expose read-only snapshots: `Charges` (max/recharge/charges)
    /// and `StatusEffects` (status id → skip-turn credit owed). Other native
    /// parts read as `None` (invisible to scripts in v1).
    fn serialize(part: &dyn Part) -> Option<BTreeMap<String, PartValue>> {
        if let Some(sp) = part.as_any().downcast_ref::<ScriptPart>() {
            return Some(sp.0.clone());
        }
        if let Some(h) = part.as_any().downcast_ref::<Health>() {
            return Some(BTreeMap::from([(
                "hp".to_string(),
                PartValue::Int(h.hp as i64),
            )]));
        }
        if let Some(c) = part
            .as_any()
            .downcast_ref::<tessera_core::effects::Charges>()
        {
            return Some(BTreeMap::from([
                ("max".to_string(), PartValue::Int(c.max)),
                ("recharge".to_string(), PartValue::Int(c.recharge)),
                ("charges".to_string(), PartValue::Int(c.charges)),
            ]));
        }
        if let Some(st) = part
            .as_any()
            .downcast_ref::<tessera_core::effects::StatusEffects>()
        {
            return Some(
                st.skip_turn_map()
                    .into_iter()
                    .map(|(k, v)| (k, PartValue::Int(v)))
                    .collect(),
            );
        }
        None
    }

    /// Resolve an id the host/snapshot hands us to the live broker entity. A
    /// script-facing id from `spawn_at` maps through [`SharedSim`]; ordinary
    /// entity bits resolve directly.
    fn resolve(&self, id: u64) -> Option<Entity> {
        if let Some(e) = self.shared.resolve_script_entity(id) {
            return Some(e);
        }
        Entity::from_bits(id)
    }

    /// The cell of an entity, if it carries a `Position` part.
    fn cell_of(&self, entity: u64) -> Option<(i32, i32)> {
        let e = self.resolve(entity)?;
        let b = self.broker.read().unwrap_or_else(|p| p.into_inner());
        let pos = b.typed_part::<tessera_core::events::Position>(
            e,
            &PartTypeId::new(tessera_core::events::PART_POSITION),
        )?;
        Some((pos.x, pos.y))
    }
}

impl WorldAccess for SimWorldAccess {
    fn try_entity(&self, entity: u64) -> bool {
        let Some(e) = self.resolve(entity) else {
            return false;
        };
        self.broker
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .is_alive(e)
    }

    fn snapshot_part(&self, entity: u64, part: &str) -> Option<BTreeMap<String, PartValue>> {
        let e = self.resolve(entity)?;
        let id = PartTypeId::new(part);
        let p = self.broker.read().unwrap_or_else(|p| p.into_inner());
        let part_obj = p.part_map(e)?.get(&id)?;
        Self::serialize(part_obj)
    }

    fn entity_parts(&self, entity: u64) -> Vec<String> {
        let Some(e) = self.resolve(entity) else {
            return Vec::new();
        };
        let p = self.broker.read().unwrap_or_else(|p| p.into_inner());
        p.part_map(e)
            .map(|m| m.names().map(|n| n.to_string()).collect())
            .unwrap_or_default()
    }

    fn apply_set_part(
        &mut self,
        entity: u64,
        name: &str,
        fields: BTreeMap<String, PartValue>,
    ) -> Result<(), ScriptError> {
        let e = self
            .resolve(entity)
            .ok_or(ScriptError::UnknownEntity { entity })?;
        let mut b = self.broker.write().unwrap_or_else(|p| p.into_inner());
        let id = PartTypeId::new(name);
        if let Some(sp) = b.typed_part_mut::<ScriptPart>(e, &id) {
            sp.0 = fields;
            return Ok(());
        }
        if name == tessera_core::events::PART_HEALTH {
            if let Some(h) = b.typed_part_mut::<Health>(e, &id) {
                if let Some(PartValue::Int(hp)) = fields.get("hp") {
                    h.hp = (*hp).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
                }
                return Ok(());
            }
            return Err(ScriptError::ImmutablePart {
                entity,
                name: name.to_string(),
            });
        }
        // Not a currently-held part: treat as a scripted upsert (the host
        // already validated against its schema).
        b.insert_part(e, id, Box::new(ScriptPart(fields)))
            .map_err(|er| ScriptError::Internal(format!("insert_part: {er}")))?;
        Ok(())
    }

    fn apply_spawn(&mut self, _id: u64, _blueprint: &str) -> Result<(), ScriptError> {
        // `engine.spawn` (the earlier positionless spawn) has no defined
        // placement in the world slice; the positioned `engine.spawn_at` is
        // the real path.
        Err(ScriptError::Internal(
            "scripted spawn without a position is not wired; use engine.spawn_at(blueprint, x?, y?)".into(),
        ))
    }

    fn apply_spawn_at(
        &mut self,
        id: u64,
        blueprint: &str,
        x: Option<i32>,
        y: Option<i32>,
    ) -> Result<(), ScriptError> {
        let world = self
            .shared
            .read_world()
            .ok_or_else(|| ScriptError::Internal("no blueprint world loaded".into()))?;
        let entity = {
            let mut b = self.broker.write().unwrap_or_else(|p| p.into_inner());
            // Create a real broker entity (its own sequential low id — hecs ids
            // cannot be forced high) and stamp the blueprint onto it.
            let e = b.spawn();
            world
                .spawn_blueprint_at(blueprint, &mut b, e)
                .map_err(|er| ScriptError::Internal(format!("spawn_at '{blueprint}': {er}")))?;
            e
        };
        // Record the script-facing id -> real entity mapping so every read the
        // script performs on the id it was told resolves to this live entity.
        self.shared.map_script_entity(id, entity_to_id(entity));
        // A scripted `Player` respawn replaces the live player: keep the
        // shared player id in sync so `engine.player()` and the sim's
        // turn resolution follow the new entity (the old one is despawned by
        // the policy's Death handler first).
        if blueprint == "Player" {
            self.shared.set_player_id(entity_to_id(entity));
        }

        // Place the `Position` part: at the caller's cell, or a seeded random
        // walkable empty cell when none was given (the positionless spawn used
        // by respawn policy). A full-map fallback is `None`, which the caller
        // retries on the next player turn.
        if let (Some(x), Some(y)) = (x, y) {
            let mut b = self.broker.write().unwrap_or_else(|p| p.into_inner());
            b.insert_part(
                entity,
                tessera_core::events::PART_POSITION,
                Box::new(tessera_core::events::Position { x, y }),
            )
            .map_err(|er| ScriptError::Internal(format!("set_position: {er}")))?;
        } else {
            use tessera_core::path;
            let zone = self.shared.read_zone();
            let to = {
                let b = self.broker.read().unwrap_or_else(|p| p.into_inner());
                let occupied = |cx: i32, cy: i32| self.shared.occupied(&b, cx, cy);
                path::random_walkable_empty(&zone, &occupied, &mut new_spawn_rng(&zone.id))
            };
            if let Some(p) = to {
                let mut b = self.broker.write().unwrap_or_else(|p| p.into_inner());
                b.insert_part(
                    entity,
                    tessera_core::events::PART_POSITION,
                    Box::new(tessera_core::events::Position { x: p.x, y: p.y }),
                )
                .map_err(|er| ScriptError::Internal(format!("set_position: {er}")))?;
            }
        }
        // Register with the scheduler if the new entity carries `ActorEnergy`
        // (spawning with an energy part makes it schedulable).
        {
            let b = self.broker.read().unwrap_or_else(|p| p.into_inner());
            if let Some(ae) = b.typed_part::<tessera_core::turns::ActorEnergy>(
                entity,
                &PartTypeId::new(tessera_core::turns::PART_ENERGY),
            ) && let Some(sched) = self.shared.scheduler()
                && let Ok(mut s) = sched.lock()
            {
                s.add_actor(entity, ae.clone());
            }
        }
        Ok(())
    }

    fn apply_despawn(&mut self, entity: u64) -> Result<(), ScriptError> {
        let e = self
            .resolve(entity)
            .ok_or(ScriptError::UnknownEntity { entity })?;
        let mut b = self.broker.write().unwrap_or_else(|p| p.into_inner());
        b.despawn(e)
            .map_err(|er| ScriptError::Internal(format!("despawn: {er}")))?;
        // Keep the scheduler in lockstep: a despawned scheduled actor must not
        // remain on the clock.
        if let Some(sched) = self.shared.scheduler()
            && let Ok(mut s) = sched.lock()
        {
            s.remove_actor(e);
        }
        // Drop any script-facing alias for this entity.
        if self.shared.resolve_script_entity(entity).is_some() {
            self.shared.unmap_script_entity(entity);
        }
        Ok(())
    }
}

impl WorldRead for SimWorldAccess {
    fn map_size(&self) -> (u32, u32) {
        let z = self.shared.read_zone();
        (z.width as u32, z.height as u32)
    }

    fn is_walkable(&self, x: i32, y: i32) -> bool {
        self.shared.read_zone().is_walkable(x, y)
    }

    fn is_occupied(&self, x: i32, y: i32) -> bool {
        let b = self.broker.read().unwrap_or_else(|p| p.into_inner());
        self.shared.occupied(&b, x, y)
    }

    fn position(&self, entity: u64) -> Option<(i32, i32)> {
        self.cell_of(entity)
    }

    fn player(&self) -> Option<u64> {
        self.shared.player()
    }

    fn is_alive(&self, entity: u64) -> bool {
        self.try_entity(entity)
    }

    fn distance(&self, a: u64, b: u64) -> Option<u32> {
        let (ax, ay) = self.cell_of(a)?;
        let (bx, by) = self.cell_of(b)?;
        use tessera_core::events::Point;
        Some(tessera_core::path::dist2(
            Point::new(ax, ay),
            Point::new(bx, by),
        ))
    }

    fn path(&self, from: u64, to: u64) -> Option<Vec<(i32, i32)>> {
        let (fx, fy) = self.cell_of(from)?;
        let (tx, ty) = self.cell_of(to)?;
        use tessera_core::events::Point;
        use tessera_core::path;
        let zone = self.shared.read_zone();
        // A* is over walkable terrain (occupancy is a spawn-target concern,
        // not a pathing one — the goblin's step is cancelled if the target
        // cell is occupied at step time).
        path::path(&zone, Point::new(fx, fy), Point::new(tx, ty), 4096)
            .map(|pts| pts.into_iter().map(|p| (p.x, p.y)).collect())
    }

    fn nearest_walkable(&self, x: i32, y: i32) -> Option<(i32, i32)> {
        use tessera_core::path;
        let zone = self.shared.read_zone();
        let b = self.broker.read().unwrap_or_else(|p| p.into_inner());
        let occupied = |cx: i32, cy: i32| self.shared.occupied(&b, cx, cy);
        path::nearest_walkable_empty(&zone, &occupied, x, y).map(|p| (p.x, p.y))
    }

    fn random_walkable_tile(&self, rng: &mut HostRng) -> Option<(i32, i32)> {
        let zone = self.shared.read_zone();
        let b = self.broker.read().unwrap_or_else(|p| p.into_inner());
        let occupied = |cx: i32, cy: i32| self.shared.occupied(&b, cx, cy);
        let mut cells: Vec<(i32, i32)> = Vec::new();
        for y in 0..zone.height as i32 {
            for x in 0..zone.width as i32 {
                if zone.is_walkable(x, y) && !occupied(x, y) {
                    cells.push((x, y));
                }
            }
        }
        if cells.is_empty() {
            return None;
        }
        let idx = rng.below(cells.len() as u64) as usize;
        Some(cells[idx])
    }

    fn player_turns(&self) -> u64 {
        self.shared.player_turns()
    }

    fn zone_start(&self) -> Option<(i32, i32)> {
        Some((
            self.shared.read_zone().start.x,
            self.shared.read_zone().start.y,
        ))
    }

    fn world_state(&self) -> Option<u64> {
        self.shared.world_state()
    }

    fn tuning_table(&self, table: &str) -> Option<BTreeMap<String, TuningValue>> {
        let world = self.shared.read_world()?;
        world.tuning_values(table)
    }
}

/// A deterministic seed for `engine.spawn_at(blueprint)` random placement.
/// Derives from the zone id string so two runs with the same pack place the
/// respawn on the same seeded cell (determinism: one seeded call, stable for a
/// zone). An entity's own per-entity AI stream is a later refinement; the base
/// contract here is "one seeded call, deterministic for a zone".
fn new_spawn_rng(zone_id: &str) -> tessera_core::rng::SimRng {
    let mut acc: u64 = 0x6a09_e667;
    for b in zone_id.bytes() {
        acc = acc
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(b as u64);
    }
    tessera_core::rng::SimRng::from_seed(acc)
}

/// Owns the [`ScriptHost`] and its schema-sync to the [`World`] registry.
#[derive(Clone)]
pub struct ScriptBridge {
    /// The host is single-threaded (the sim drives it synchronously on one
    /// thread; mlua's `Lua` is not `Send`/`Sync`), so an `Rc` — not `Arc` —
    /// is the correct shared wrapper. Cloning the bridge shares the handle.
    host: Rc<Mutex<ScriptHost>>,
    /// The broker handle shared by every host world-access view.
    broker: Arc<RwLock<EntityBroker>>,
    /// The shared simulation snapshot the world access + reader reference.
    shared: Arc<SharedSim>,
}

impl ScriptBridge {
    /// Create the bridge over a shared broker. The host is empty until
    /// [`Self::load`] is called; the sim's world state is installed via
    /// [`Self::set_sim_world_state`] once the sim is built.
    pub fn new(broker: Arc<RwLock<EntityBroker>>) -> ScriptBridge {
        let shared = Arc::new(SharedSim::new());
        let wa = SimWorldAccess {
            broker: broker.clone(),
            shared: shared.clone(),
        };
        let read: Arc<dyn WorldRead> = Arc::new(wa.clone());
        let host = ScriptHost::with_reader(Box::new(wa), read.clone());
        ScriptBridge {
            host: Rc::new(Mutex::new(host)),
            broker,
            shared,
        }
    }

    /// Install the sim's live world state (zone, scheduler, player id) into the
    /// shared snapshot. `scheduler` is the same `Arc<Mutex<Scheduler>>` the sim
    /// drives. Called by the sim after construction; must precede any script
    /// dispatch that reads the map/nav API.
    pub fn set_sim_world_state(
        &self,
        zone: Zone,
        scheduler: Arc<Mutex<Scheduler>>,
        player: Entity,
    ) {
        self.shared.set_world_state(zone, scheduler, player);
    }

    /// Return the broker handle used by script world access.
    pub fn broker(&self) -> Arc<RwLock<EntityBroker>> {
        self.broker.clone()
    }

    /// Replace broker contents without changing the shared handle.
    pub fn replace_broker(&self, broker: EntityBroker) {
        *self.broker.write().unwrap_or_else(|p| p.into_inner()) = broker;
    }

    /// Install the blueprint `World` (once loaded) for `engine.spawn_at`.
    pub fn set_world(&self, world: Arc<World>) {
        self.shared.set_world(world);
    }

    /// The currently installed blueprint `World` (hot-reloadable; the native
    /// effect runtime resolves through its library, so reloads swap the
    /// definitions new applications see).
    pub fn world(&self) -> Option<Arc<World>> {
        self.shared.read_world()
    }

    /// Bump the shared player-turn clock (once per submitted player action).
    pub fn set_player_turns(&self, n: u64) {
        self.shared.set_player_turns(n);
    }

    /// The script-facing id of the installed world-state singleton entity.
    pub fn world_state_id(&self) -> Option<u64> {
        self.shared.world_state()
    }

    /// Install the world-state singleton entity (the respawn / policy anchor)
    /// into the shared snapshot. The demo creates this non-scheduled entity at
    /// load and the goblin policy reads/writes its parts through
    /// `engine.world_state()`.
    pub fn set_world_state_entity(&self, entity: Entity) {
        self.shared.set_world_state_id(entity_to_id(entity));
    }

    /// The current shared player entity id (as stored by the sim's
    /// `set_sim_world_state`, updated on player respawn).
    pub fn player_id(&self) -> Option<u64> {
        self.shared.player()
    }

    /// Queue a native `Death` observation to be forwarded to scripts once the
    /// scheduler lock is released (avoids re-entrant scheduler locking).
    pub fn push_death(&self, target: u64) {
        self.shared.push_death(target);
    }

    /// Forward every queued death to the script host (safe without the
    /// scheduler lock, since the respawn policy mutates the scheduler).
    pub fn drain_deaths(&self) {
        let deaths = self.shared.drain_deaths();
        for target in deaths {
            self.forward_death(target);
        }
    }

    /// Forward a native `Death` event (target entity id) to the script host so
    /// `engine.on("Death", ...)` handlers can react. Deterministic event
    /// dispatch, same value boundary as every other script dispatch. Passes
    /// the world-state singleton id as a param so it is in the handler's read
    /// snapshot (`engine.get_part` on the respawn anchor works).
    pub fn forward_death(&self, target: u64) {
        let host = self.host.clone();
        let ws = self.shared.world_state().map(|w| w as i64);
        let mut params = int_params(&[("target", target as i64)]);
        if let Some(w) = ws {
            params.push(("world_state", mlua::Value::Integer(w)));
        }
        let mut h = match host.lock() {
            Ok(h) => h,
            Err(_) => return,
        };
        let _ = h.dispatch("Death", target, &params);
    }

    /// Translate a script-facing entity id (returned by `engine.spawn_at`) to the
    /// live broker entity, if it exists. Used by tests and by the turn seam.
    pub fn resolve_script_entity(&self, script_id: u64) -> Option<Entity> {
        self.shared.resolve_script_entity(script_id)
    }

    /// Load one script (registers its `define_part` schemas + `engine.on`
    /// handlers). Fail-closed like the host.
    pub fn load(&self, script: &str, blueprint: &str, source: &str) -> Result<(), ScriptError> {
        let mut h = self
            .host
            .lock()
            .map_err(|e| ScriptError::Internal(format!("host lock: {e}")))?;
        h.load_script(script, blueprint, source)
    }

    /// Mirror every script-declared part schema from the host registry into the
    /// world registry (required *before* `World::load` so blueprint
    /// `parts.<Name>` validates). Deterministic (sorted names).
    pub fn sync_schemas(&self, world: &mut World) -> Result<(), anyhow::Error> {
        let h = self
            .host
            .lock()
            .map_err(|e| anyhow::anyhow!("host lock: {e}"))?;
        let reg = h
            .registry()
            .lock()
            .map_err(|e| anyhow::anyhow!("registry lock: {e}"))?;
        let names: Vec<PartTypeId> = reg
            .iter_storage()
            .filter(|(id, _)| reg.script_schema(id).is_some())
            .map(|(id, _)| id.clone())
            .collect();
        for name in names {
            if let Some(schema) = reg.script_schema(&name) {
                world
                    .register_script_part(schema.clone())
                    .map_err(|e| anyhow::anyhow!("registering scripted part {name}: {e}"))?;
            }
        }
        Ok(())
    }

    /// Access the wrapped host.
    pub fn host(&self) -> &Rc<Mutex<ScriptHost>> {
        &self.host
    }
}

/// Convert a core entity to its script-facing u64 id (stable for a live
/// entity; matches what the `WorldAccess` recovers via `from_bits`).
pub fn entity_to_id(e: Entity) -> u64 {
    e.to_bits().get()
}

/// Build mlua `Value` params for [`ScriptHost::dispatch`] from integer fields
/// (the script surface passes entity ids and integer scalars as integers).
pub fn int_params<'a>(pairs: &[(&'a str, i64)]) -> Vec<(&'a str, mlua::Value)> {
    pairs
        .iter()
        .map(|(k, v)| (*k, mlua::Value::Integer(*v)))
        .collect()
}
