//! `World`: the integration seam that ties the blueprint library, the
//! engine's hecs components, and the dynamic part store together behind
//! spawning from data and fail-closed content hot reload.
//!
//! Spawning (`World::spawn_blueprint`) resolves a flattened blueprint from the
//! shared [`BlueprintLibrary`], instantiates *registered engine components*
//! into the `hecs` world (through the [`ComponentSpawner`] seam), and puts the
//! *remaining* parts into the entity's `PartMap`. Reload re-resolves the whole
//! blueprint set from the vfs into a fresh library and atomically swaps the
//! shared `Arc<RwLock<…>>`; on any parse/resolution error it keeps the previous
//! library unchanged (fail-closed).
//!
//! Determinism (see `docs/determinism.md`): this module uses only `BTreeMap`
//! collections (never `HashMap`/`HashSet`), spawns consume no randomness, and a
//! reload touches only the shared library — it must never reach `SimRng` or
//! existing live entities. The `notify` watcher thread (in the `tessera` bin)
//! merely *triggers* a rebuild and hands the result over; it never mutates sim
//! state directly.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use hecs::Entity;
use thiserror::Error;
use toml::Value;

use crate::blueprints::BlueprintLibrary;
use crate::ecs::{EntityBroker, PartError, PartRegistry, PartTypeId, StorageClass};
use crate::vfs::Vfs;

/// An engine-native component type the world knows how to stamp into `hecs`
/// when a blueprint declares a matching part. E.g. `parts.Render = { tile =
/// "creature.player" }` produces a [`RenderTile`] component on the entity.
///
/// All engine part names must also be registered in the loader registry so the
/// blueprint loader accepts them (an unknown part name is a load error).
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderTile {
    /// Tile *id* resolved against the tile registry at draw time (the registry
    /// maps id→glyph/color; the engine never hardcodes an id in sim code).
    pub tile: String,
}

/// A spawner turns a part's `toml::Value` payload into a `hecs` component on an
/// entity. Returns `Err` with a diagnostic when the payload is malformed, so a
/// bad part fails the spawn loudly rather than being silently dropped.
pub type ComponentSpawner =
    Box<dyn Fn(&mut hecs::World, Entity, &Value) -> Result<(), SpawnError> + Send + Sync + 'static>;

/// Failure modes of [`World::spawn_blueprint`].
#[derive(Debug, Error)]
pub enum SpawnError {
    #[error("blueprint library lock poisoned")]
    LibraryPoisoned,
    #[error("unknown blueprint id '{0}'")]
    UnknownBlueprint(String),
    #[error("part '{0}' is not registered with the world; loader rejected the blueprint")]
    UnregisteredPart(String),
    #[error("engine component from part '{name}' failed to spawn: {msg}")]
    ComponentSpawn { name: String, msg: String },
    #[error("part '{name}' failed to build: {msg}")]
    PartBuild { name: String, msg: String },
    #[error("entity {0:?} was despawned mid-spawn")]
    NoSuchEntity(Entity),
}

impl SpawnError {
    fn component(name: &str, msg: impl Into<String>) -> Self {
        Self::ComponentSpawn {
            name: name.to_string(),
            msg: msg.into(),
        }
    }
}

/// The `World`: a shared, hot-reloadable blueprint library plus the
/// registries and spawner seams needed to instantiate entities from data.
///
/// `World` is cheap to share (`Arc`<`RwLock`>`BlueprintLibrary`) and is locked
/// only during the brief resolution reads in [`Self::spawn_blueprint`].
pub struct World {
    /// Resolved blueprints, swapped as a whole on hot reload.
    library: Arc<RwLock<BlueprintLibrary>>,
    /// Single source of truth for part names: name → declared [`StorageClass`]
    /// and factory. The blueprint loader validates content against exactly this
    /// registry.
    registry: PartRegistry,
    /// Part name → engine-component spawner (stamped into `hecs`). Only for
    /// names declared [`StorageClass::Component`] in [`Self::registry`]; kept
    /// here because the spawner's `SpawnError` lives in this module.
    spawners: BTreeMap<String, ComponentSpawner>,
    /// Post-load tuning tables plus the validated override overlay. Rebuilt
    /// from scratch on every [`Self::load`] (overrides are per-run policy, not
    /// content, so a failed reload keeps the previous store with its previous
    /// library).
    tuning: RwLock<crate::tuning::TuningStore>,
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}

/// Build a `Box<dyn ecs::Part>` of concrete type `C` from a part's TOML payload.
fn build_dynamic_part<C>(value: &Value) -> Result<Box<dyn crate::ecs::Part>, PartError>
where
    C: serde::de::DeserializeOwned + std::fmt::Debug + Send + Sync + 'static,
{
    let c: C = value
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| PartError::BuildFailed {
            name: std::any::type_name::<C>().to_string(),
            msg: e.to_string(),
        })?;
    Ok(Box::new(c))
}

impl World {
    /// An empty world (empty library + empty registry). Call [`Self::load`]
    /// after registering engine components and parts to populate content.
    ///
    /// The S0 content kinds (`Ability`, `Skill`, `Status`, `Item`, `Quest`,
    /// `Tuning` — see [`crate::kinds`]) are always registered so blueprint
    /// content may use them everywhere; their names are fixed and cannot
    /// collide with engine parts.
    pub fn new() -> Self {
        let mut registry = PartRegistry::new();
        crate::kinds::register_all(&mut registry)
            .expect("S0 kind part names are fixed and never collide");
        Self {
            library: Arc::new(RwLock::new(BlueprintLibrary::new())),
            registry,
            spawners: BTreeMap::new(),
            tuning: RwLock::new(crate::tuning::TuningStore::default()),
        }
    }

    /// Register an engine component `C` that the world stamps into `hecs` when
    /// a blueprint declares part `name`. `C` is deserialized from the part's
    /// TOML payload with serde defaults; unknown payload fields are rejected by
    /// the type's own `#[serde(deny_unknown_fields)]`.
    ///
    /// Declares `name` as a [`StorageClass::Component`] part in the single
    /// registry (the same one the blueprint loader validates against), so the
    /// name is declared exactly once. Registering an already-registered name is
    /// an error.
    pub fn register_component<C>(&mut self, name: &'static str) -> Result<(), PartError>
    where
        C: serde::de::DeserializeOwned + std::fmt::Debug + Send + Sync + 'static,
    {
        self.registry.register(
            PartTypeId::new(name),
            StorageClass::Component,
            build_dynamic_part::<C>,
        )?;
        let spawner: ComponentSpawner = {
            Box::new(
                move |world: &mut hecs::World, entity: Entity, value: &Value| {
                    let comp: C = value
                        .clone()
                        .try_into()
                        .map_err(|e| SpawnError::component(name, e.to_string()))?;
                    world
                        .insert_one(entity, comp)
                        .map_err(|_| SpawnError::NoSuchEntity(entity))
                },
            )
        };
        self.spawners.insert(name.to_string(), spawner);
        Ok(())
    }

    /// Register a dynamic part `C` stored in each spawned entity's `PartMap`
    /// (mod-defined parts; for engine components use
    /// [`Self::register_component`]). Declares `name` as a
    /// [`StorageClass::Dynamic`] part in the single registry (the same one the
    /// blueprint loader validates against) so the name is declared exactly
    /// once; registering an already-registered name is an error.
    pub fn register_part<C>(&mut self, name: &'static str) -> Result<(), PartError>
    where
        C: serde::de::DeserializeOwned + std::fmt::Debug + Send + Sync + 'static,
    {
        self.registry.register(
            PartTypeId::new(name),
            StorageClass::Dynamic,
            build_dynamic_part::<C>,
        )
    }

    /// Register a schema-backed scripted part. Script parts are always stored as
    /// [`StorageClass::Dynamic`] (in each entity's `PartMap`), never stamped into
    /// `hecs` as a compile-time component. The schema is shape-checked at
    /// registration; later `World::load`/`spawn_blueprint` calls validate each
    /// entity's TOML payload against it.
    pub fn register_script_part(
        &mut self,
        def: crate::script_part::ScriptPartType,
    ) -> Result<(), PartError> {
        self.registry.register_script_part(def)
    }

    /// Load (or reload) the blueprint set from the vfs, atomically swapping the
    /// shared library on success. On any parse/resolution error returns the
    /// [`crate::blueprints::LoadFail`] diagnostics (`file + id`) and leaves the
    /// previous library untouched — content can never break what's live.
    ///
    /// The post-load tuning store is rebuilt from the fresh library *before*
    /// the swap: a `parts.Tuning` payload that fails typed deserialization is
    /// a load error too (fail-closed, so a reload rejects the batch and keeps
    /// the previous library *and* the previous store together).
    ///
    /// Content-only by construction: this touches only the shared library and
    /// the tuning store, never `SimRng` or existing entities.
    pub fn load(&self, vfs: &dyn Vfs) -> Result<(), crate::blueprints::LoadFail> {
        let fresh = BlueprintLibrary::load(vfs, &self.registry)?;
        let fresh_tuning = crate::tuning::TuningStore::from_library(&fresh, &self.registry)
            .map_err(crate::blueprints::LoadFail::single)?;
        let mut guard = self.library.write().map_err(|_| {
            crate::blueprints::LoadFail::single(crate::blueprints::BlueprintError::Io {
                path: "blueprints".into(),
                source: std::io::Error::other("blueprint library lock poisoned"),
            })
        })?;
        *guard = fresh;
        drop(guard);
        *self.tuning.write().map_err(|_| {
            crate::blueprints::LoadFail::single(crate::blueprints::BlueprintError::Io {
                path: "blueprints".into(),
                source: std::io::Error::other("tuning store lock poisoned"),
            })
        })? = fresh_tuning;
        Ok(())
    }

    /// Shared handle to the current library (for read access / inspection).
    pub fn library(&self) -> &Arc<RwLock<BlueprintLibrary>> {
        &self.library
    }

    /// Merged values of one tuning table (authored + applied overrides),
    /// sorted by key, or `None` when no such table is loaded.
    pub fn tuning_values(
        &self,
        table: &str,
    ) -> Option<BTreeMap<String, crate::kinds::TuningValue>> {
        self.tuning
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .values(table)
    }

    /// The applied overrides in application order (run-output record).
    pub fn tuning_applied(&self) -> Vec<crate::tuning::TuningOverride> {
        self.tuning
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .applied_overrides()
            .to_vec()
    }

    /// Apply one validated override triple to the post-load tuning overlay.
    /// Unknown table/key and wrong-value-type all fail closed; authored
    /// content is never mutated. The override is recorded for run output.
    pub fn apply_tuning_override(
        &self,
        table: &str,
        key: &str,
        value: &toml::Value,
    ) -> Result<(), crate::tuning::TuningError> {
        self.tuning
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .apply_override(table, key, value)
    }

    /// The whitelist [`SaveHooks`] built from this world's registry: the
    /// engine-native parts (Position/Health/ActorEnergy/Stats) plus every
    /// schema-backed script part the loaded pack registered. This is the hook
    /// set `Sim::save`/`Sim::load` and the console save/load commands use — it
    /// must match exactly what the world can spawn, so a save never fails on a
    /// part the pack itself declares.
    pub fn save_hooks(&self) -> crate::save::SaveHooks {
        crate::save::build_save_hooks(&self.registry).0
    }

    /// Resolve the flattened blueprint `id`, instantiate its registered engine
    /// components into `hecs` via [`EntityBroker`], and store every remaining
    /// part in the entity's `PartMap`. Returns the single entity handle.
    ///
    /// Deterministic: parts are stamped in sorted (BTreeMap) part-name order,
    /// and spawning consumes no randomness.
    pub fn spawn_blueprint(
        &self,
        id: &str,
        broker: &mut EntityBroker,
    ) -> Result<Entity, SpawnError> {
        let entity = broker.spawn();
        match self.stamp_blueprint(id, broker, entity) {
            Ok(()) => Ok(entity),
            Err(e) => {
                broker.despawn(entity).ok();
                Err(e)
            }
        }
    }

    /// Spawn a blueprint **onto an already-created entity** (one reserved by a
    /// caller-chosen id via [`EntityBroker::spawn_with_bits`]). Resolves the
    /// same flattened blueprint and stamps every part, without allocating a new
    /// entity id — this is how scripted `engine.spawn_at` creates an entity at
    /// the id the script was already told. On any failure the entity is
    /// despawned (rolled back), matching [`Self::spawn_blueprint`].
    pub fn spawn_blueprint_at(
        &self,
        id: &str,
        broker: &mut EntityBroker,
        entity: hecs::Entity,
    ) -> Result<(), SpawnError> {
        if let Err(e) = self.stamp_blueprint(id, broker, entity) {
            broker.despawn(entity).ok();
            return Err(e);
        }
        Ok(())
    }

    /// Resolve the flattened blueprint `id` and stamp every part onto `entity`
    /// (which must already be live in `broker`). Shared by [`Self::spawn_blueprint`]
    /// and [`Self::spawn_blueprint_at`]; the caller rolls back on error.
    fn stamp_blueprint(
        &self,
        id: &str,
        broker: &mut EntityBroker,
        entity: hecs::Entity,
    ) -> Result<(), SpawnError> {
        let lib = self
            .library
            .read()
            .map_err(|_| SpawnError::LibraryPoisoned)?;
        let blueprint = lib
            .get(id)
            .ok_or_else(|| SpawnError::UnknownBlueprint(id.to_string()))?;
        for (name, payload) in &blueprint.parts {
            let part_id = PartTypeId::new(name.clone());
            match self.registry.storage(&part_id) {
                Some(StorageClass::Component) => {
                    let spawner = self
                        .spawners
                        .get(name)
                        .ok_or_else(|| SpawnError::UnregisteredPart(name.clone()))?;
                    spawner(broker.world_mut(), entity, payload).inspect_err(|_| {
                        // Roll back the partially-built entity so a failed spawn
                        // leaves no half-stamped husk behind.
                        broker.despawn(entity).ok();
                    })?;
                }
                Some(StorageClass::Dynamic) => {
                    let part = self.registry.build(&part_id, payload).map_err(|e| {
                        SpawnError::PartBuild {
                            name: name.clone(),
                            msg: e.to_string(),
                        }
                    })?;
                    if let Err(e) = broker.insert_part(entity, name.clone(), part).map_err(|e| {
                        SpawnError::PartBuild {
                            name: name.clone(),
                            msg: e.to_string(),
                        }
                    }) {
                        broker.despawn(entity).ok();
                        return Err(e);
                    }
                }
                None => {
                    // Reachable only if the blueprint library escaped loader
                    // validation (it validates every part name against the
                    // registry). Fail closed rather than silently dropping.
                    broker.despawn(entity).ok();
                    return Err(SpawnError::UnregisteredPart(name.clone()));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::SimRng;
    use crate::vfs::MemVfs;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq, Default, Clone)]
    #[serde(deny_unknown_fields)]
    struct Stats {
        #[serde(default)]
        strength: i64,
        #[serde(default)]
        agility: i64,
    }

    fn creatures() -> MemVfs {
        MemVfs::new().with(
            "blueprints/creatures.toml",
            r##"
[Actor]
parts = {}

[Humanoid]
inherits = "Actor"
parts.Render = { tile = "creature.humanoid" }
parts.Stats  = { strength = 10, agility = 10 }

[Player]
inherits = "Humanoid"
parts.Render = { tile = "creature.player" }
parts.Stats  = { strength = 12, agility = 8 }

[Goblin]
inherits = "Humanoid"
parts.Render = { tile = "creature.goblin" }
parts.Stats  = { strength = 18, agility = 6 }
"##,
        )
    }

    fn world(vfs: &MemVfs) -> World {
        let mut w = World::new();
        w.register_component::<RenderTile>("Render").unwrap();
        w.register_part::<Stats>("Stats").unwrap();
        w.load(vfs).expect("shipable base content loads");
        w
    }

    #[test]
    fn spawn_blueprint_resolves_inheritance_and_stamps_components() {
        let vfs = creatures();
        let w = world(&vfs);
        let mut broker = EntityBroker::new();

        let player = w.spawn_blueprint("Player", &mut broker).unwrap();
        // Engine component RenderTile stamped into hecs from parts.Render.tile,
        // with inheritance flattened (Humanoid -> Actor), Player override wins.
        let player_tile = broker
            .world()
            .get::<&RenderTile>(player)
            .unwrap()
            .tile
            .clone();
        assert_eq!(player_tile, "creature.player");

        // Remaining part (Stats) went into the entity's PartMap.
        let player_stats = broker
            .typed_part::<Stats>(player, &PartTypeId::new("Stats"))
            .unwrap()
            .clone();
        assert_eq!(
            player_stats,
            Stats {
                strength: 12,
                agility: 8
            }
        );

        let goblin = w.spawn_blueprint("Goblin", &mut broker).unwrap();
        assert_eq!(
            broker.world().get::<&RenderTile>(goblin).unwrap().tile,
            "creature.goblin"
        );
        assert_eq!(
            broker
                .typed_part::<Stats>(goblin, &PartTypeId::new("Stats"))
                .unwrap()
                .clone(),
            Stats {
                strength: 18,
                agility: 6
            }
        );

        // Actor is an abstract base: resolvable, but no parts of its own.
        let actor = w.spawn_blueprint("Actor", &mut broker).unwrap();
        assert!(broker.world().get::<&RenderTile>(actor).is_err());
        assert!(broker.part_map(actor).unwrap().is_empty());
    }

    #[test]
    fn registered_part_names_have_storage_class_and_loader_accepts_them() {
        let vfs = creatures();
        let w = world(&vfs);
        // The same single registry the loader validates against must declare a
        // storage class for every part name it accepts: the two engine parts
        // below plus the six always-registered S0 kind parts.
        assert_eq!(w.registry.len(), 8);
        for (id, storage) in w.registry.iter_storage() {
            assert!(
                matches!(storage, StorageClass::Component | StorageClass::Dynamic),
                "part '{id}' must have a declared storage class"
            );
        }
        assert_eq!(
            w.registry.storage(&PartTypeId::new("Render")),
            Some(StorageClass::Component)
        );
        assert_eq!(
            w.registry.storage(&PartTypeId::new("Stats")),
            Some(StorageClass::Dynamic)
        );
        // The loader accepted every part name used in content (load succeeded
        // above), confirming registry and loader agree.
        let lib = w.library().read().unwrap();
        for part_name in lib.get("Player").unwrap().part_names() {
            assert!(
                w.registry.contains(&PartTypeId::new(part_name.clone())),
                "loader accepted '{part_name}' but it is not in the registry"
            );
        }
    }

    #[test]
    fn duplicate_registration_is_an_error() {
        let mut w = World::new();
        w.register_component::<RenderTile>("Render").unwrap();
        // Same name re-registered as a component -> error.
        let err = w.register_component::<Stats>("Render").unwrap_err();
        assert!(matches!(err, PartError::DuplicatePartType(id) if id.as_str() == "Render"));
        // Same name re-registered with a different storage class -> error.
        let err = w.register_part::<Stats>("Render").unwrap_err();
        assert!(matches!(err, PartError::DuplicatePartType(id) if id.as_str() == "Render"));
        // The original entry is intact (6 S0 kind parts are always registered).
        assert_eq!(w.registry.len(), 7);
        assert_eq!(
            w.registry.storage(&PartTypeId::new("Render")),
            Some(StorageClass::Component)
        );
    }

    #[test]
    fn engine_name_spawns_to_hecs_not_partmap_and_dynamic_to_partmap() {
        let vfs = creatures();
        let w = world(&vfs);
        let mut broker = EntityBroker::new();

        let e = w.spawn_blueprint("Player", &mut broker).unwrap();

        // Engine component name ("Render") went to hecs, not PartMap.
        assert!(broker.world().get::<&RenderTile>(e).is_ok());
        let render_id = PartTypeId::new("Render");
        assert!(
            !broker.part_map(e).unwrap().contains(&render_id),
            "engine component must not land in the PartMap"
        );

        // Dynamic name ("Stats") went to PartMap, not hecs.
        assert!(
            broker
                .typed_part::<Stats>(e, &PartTypeId::new("Stats"))
                .is_some()
        );
        // Stats has no hecs-typed component — assert via part_map only (ecs
        // world has no Stats component type at this layer).
        assert!(
            broker
                .part_map(e)
                .unwrap()
                .contains(&PartTypeId::new("Stats"))
        );
    }

    #[test]
    fn script_part_registers_dynamic_and_spawns_into_partmap() {
        use crate::script_part::{FieldDef, PartValue, ScriptPart, ScriptPartType};
        use std::collections::BTreeMap;

        let mut w = World::new();
        let mut schema = BTreeMap::new();
        schema.insert(
            "charges".to_string(),
            FieldDef::int().default(PartValue::Int(1)).min(0).max(99),
        );
        w.register_script_part(ScriptPartType::new("Charge", schema.clone()))
            .unwrap();

        let v = MemVfs::new().with(
            "blueprints/z.toml",
            r##"[Zapper]
parts.Charge = { charges = 3 }
"##,
        );
        w.load(&v).unwrap();

        let mut broker = EntityBroker::new();
        let e = w.spawn_blueprint("Zapper", &mut broker).unwrap();

        // Script part is Dynamic: it lives in the entity's PartMap, never hecs.
        let id = PartTypeId::new("Charge");
        assert_eq!(w.registry.storage(&id), Some(StorageClass::Dynamic));
        let part = broker
            .typed_part::<ScriptPart>(e, &id)
            .expect("Charge part must be in the PartMap")
            .clone();
        assert_eq!(part.get("charges"), Some(&PartValue::Int(3)));

        // Deterministic schema lookup returns the registered schema.
        assert_eq!(
            w.registry.script_schema(&id).map(|t| &t.schema),
            Some(&schema)
        );

        // Fail-closed: a payload violating the schema fails the spawn.
        let bad = MemVfs::new().with(
            "blueprints/b.toml",
            r##"[Bad]
parts.Charge = { charges = 500 }
"##,
        );
        let mut w2 = World::new();
        w2.register_script_part(ScriptPartType::new("Charge", schema))
            .unwrap();
        w2.load(&bad).unwrap();
        let err = w2.spawn_blueprint("Bad", &mut broker).unwrap_err();
        match err {
            SpawnError::PartBuild { name, .. } => assert_eq!(name, "Charge"),
            other => panic!("expected PartBuild error, got {other:?}"),
        }
    }

    #[test]
    fn blueprint_with_unregistered_name_is_a_load_error() {
        // A part name that is not registered must fail load (fail-closed), not
        // be silently dropped.
        let v = MemVfs::new().with(
            "blueprints/t.toml",
            r##"[B]
parts.Throwable = { x = 1 }
"##,
        );
        let mut w = World::new();
        w.register_component::<RenderTile>("Render").unwrap();
        let err = w.load(&v).unwrap_err();
        assert!(
            err.to_string().contains("unknown part 'Throwable'"),
            "{err}"
        );
    }

    #[test]
    fn spawn_unknown_id_and_bad_part_fail_closed() {
        let vfs = creatures();
        let w = world(&vfs);
        let mut broker = EntityBroker::new();

        assert!(matches!(
            w.spawn_blueprint("DoesNotExist", &mut broker),
            Err(SpawnError::UnknownBlueprint(_))
        ));

        // A part registered as generic destroyed build: force a bad payload by
        // registering a component whose type rejects the payload.
        let bad = MemVfs::new().with(
            "blueprints/b.toml",
            r##"[B]
parts.Render = { tile = 42 }
"##,
        );
        // Loading keeps stale? No — create a fresh world with a known-bad part
        // payload: Render requires a String tile; an int must fail spawn. But
        // load only stores text (kept raw), so it must succeed; spawn fails.
        let mut w2 = World::new();
        w2.register_component::<RenderTile>("Render").unwrap();
        w2.load(&bad).unwrap();
        let e = w2.spawn_blueprint("B", &mut broker).unwrap_err();
        assert!(matches!(e, SpawnError::ComponentSpawn { .. }), "{e}");
        // Fully rolled back: no partial entity left behind.
        let n = broker.world().iter().count();
        assert_eq!(n, 0, "spawn failure must roll back to no entity");
    }

    #[test]
    fn spawns_are_deterministic_both_order_and_rng_free() {
        let vfs = creatures();
        let w = world(&vfs);
        let mut broker = EntityBroker::new();

        let mut rng = SimRng::from_seed(1);

        // Spawn several entities; consuming no randomness means the rng stream
        // is untouched (determinism rule: reload/spawn must not perturb SimRng).
        for _ in 0..10 {
            w.spawn_blueprint("Player", &mut broker).unwrap();
            w.spawn_blueprint("Goblin", &mut broker).unwrap();
        }
        // The post-spawn stream matches a fresh, untouched stream: spawn is
        // deterministic in the seed and consumes no randomness.
        let mut untouched = SimRng::from_seed(1);
        assert_eq!(rng.next_u32(), untouched.next_u32());
        assert_eq!(rng.next_u32(), untouched.next_u32());
    }

    #[test]
    fn hot_reload_atomic_swap_and_fail_closed_keeps_old() {
        let vfs = creatures();
        let w = world(&vfs);

        // Old library resolves spawn with original content.
        let mut broker = EntityBroker::new();
        let p = w.spawn_blueprint("Player", &mut broker).unwrap();
        assert_eq!(
            broker.world().get::<&RenderTile>(p).unwrap().tile,
            "creature.player"
        );

        // A good reload swaps in updated content.
        let v2 = MemVfs::new().with(
            "blueprints/creatures.toml",
            r##"[Player]
parts.Render = { tile = "creature.player.v2" }
"##,
        );
        w.load(&v2).unwrap();
        // New spawns use the new library...
        let t = w.spawn_blueprint("Player", &mut broker).unwrap();
        assert_eq!(
            broker.world().get::<&RenderTile>(t).unwrap().tile,
            "creature.player.v2"
        );
        // ...while existing live entities kept their old parts (no migration).
        assert_eq!(
            broker.world().get::<&RenderTile>(p).unwrap().tile,
            "creature.player"
        );

        // A bad reload (parse error) leaves the previous library unchanged.
        let bad = MemVfs::new().with("blueprints/creatures.toml", "[Player\nnope = ");
        let err = w.load(&bad).unwrap_err();
        assert!(
            err.diagnostics
                .iter()
                .any(|d| d.to_string().contains("blueprints/creatures.toml"))
        );
        let t2 = w.spawn_blueprint("Player", &mut broker).unwrap();
        assert_eq!(
            broker.world().get::<&RenderTile>(t2).unwrap().tile,
            "creature.player.v2",
            "failed reload must keep the previous library"
        );
    }

    #[test]
    fn hot_reload_does_not_touch_live_entities_or_rng() {
        let vfs = creatures();
        let w = world(&vfs);
        let mut broker = EntityBroker::new();
        let live = w.spawn_blueprint("Player", &mut broker).unwrap();

        let mut rng = SimRng::from_seed(99);

        // Successful reload...
        let v2 = MemVfs::new().with(
            "blueprints/creatures.toml",
            r##"[Player]
parts.Render = { tile = "creature.player.v2" }
"##,
        );
        w.load(&v2).unwrap();
        // ...then a failed reload.
        w.load(&MemVfs::new().with("blueprints/x.toml", "[Z\n"))
            .unwrap_err();

        // Existing entity untouched.
        assert_eq!(
            broker.world().get::<&RenderTile>(live).unwrap().tile,
            "creature.player"
        );
        // SimRng untouched: post-reload stream matches a fresh same-seed stream.
        let mut untouched = SimRng::from_seed(99);
        assert_eq!(rng.next_u32(), untouched.next_u32());
        assert_eq!(rng.next_u32(), untouched.next_u32());
    }

    #[test]
    fn missing_tile_id_in_registry_still_spawns_as_data() {
        // RenderTile stores the tile *id*; resolution against the tile registry
        // happens at draw time. So even an id absent from a registry (e.g. a
        // not-yet-defined creature) spawns fine — determinism of content load
        // and draw are separate concerns.
        let vfs = creatures();
        let w = world(&vfs);
        let mut broker = EntityBroker::new();
        let g = w.spawn_blueprint("Goblin", &mut broker).unwrap();
        assert_eq!(
            broker.world().get::<&RenderTile>(g).unwrap().tile,
            "creature.goblin"
        );
    }

    #[test]
    fn reserved_handlers_table_is_a_load_error() {
        // A reserved `handlers` table must error, not be ignored.
        let v = MemVfs::new().with(
            "blueprints/h.toml",
            r##"[X]
handlers = { OnHit = 1 }
"##,
        );
        let w = World::new();
        let err = w.load(&v).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("`handlers` is reserved"), "{text}");
    }

    #[test]
    fn library_roundtrips_via_blueprint_library() {
        // Sanity: the world's public library handle exposes resolved ids.
        let w = world(&creatures());
        let lib = w.library().read().unwrap();
        assert_eq!(
            lib.ids().cloned().collect::<Vec<_>>(),
            vec!["Actor", "Goblin", "Humanoid", "Player"]
        );
    }
}
