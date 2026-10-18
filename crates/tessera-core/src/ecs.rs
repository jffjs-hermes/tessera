//! ECS world plus dynamic, mod-defined part storage.
//!
//! The engine keeps its compile-time-known components in [`hecs::World`] (fast
//! archetype queries for the hot simulation/render paths) and its mod-defined
//! parts in a per-entity [`PartMap`] keyed by name. [`EntityBroker`] pairs the
//! two behind a single entity handle so game code never touches them
//! separately; despawning removes both.
//!
//! Determinism (see `docs/determinism.md`): every collection in this module is
//! a [`BTreeMap`] keyed by id/name, part lists iterate in sorted name order,
//! and there is **no** `HashMap`/`HashSet` anywhere in this module.

use std::any::Any;
use std::collections::BTreeMap;
use std::fmt;

use hecs::Entity;
use thiserror::Error;

use crate::script_part::ScriptPartType;

/// The string name that identifies a part type (e.g. `"Stats"`), used as the
/// `BTreeMap` key in [`PartMap`] and [`PartRegistry`].
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PartTypeId(String);

impl PartTypeId {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for PartTypeId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl From<String> for PartTypeId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl AsRef<str> for PartTypeId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PartTypeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A mod-defined part. Types the engine core does not know at compile time are
/// stored boxed in a [`PartMap`]; access goes through downcasting (see
/// [`PartMap::get_typed`] and [`EntityBroker::typed_part`]). Any
/// `'static + Send + Sync` type can be a part (blanket impl), which is exactly
/// what lets mods add new part types at runtime.
pub trait Part: Any + Send + Sync {
    /// Upcast to `&dyn Any` for downcasting.
    ///
    /// Prefer the typed accessors ([`PartMap::get_typed`],
    /// [`EntityBroker::typed_part`]) over calling this directly. When calling it
    /// yourself, do so on a `&dyn Part`, *not* on an owned `Box<dyn Part>`:
    /// the blanket [`Part`] impl also covers `Box<dyn Part>` / `&dyn Part`, and
    /// method resolution on the box resolves to that box impl, returning the box
    /// as the `Any` instead of the inner value. `part.as_ref().as_any()` works.
    fn as_any(&self) -> &dyn Any;

    fn as_any_mut(&mut self) -> &mut dyn Any;
}

impl<T: Any + Send + Sync> Part for T {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// Per-entity storage for mod-defined parts, keyed by part name.
///
/// Iteration order is the sorted (`BTreeMap`) part-name order, per determinism
/// rule 3 — iterating a part list is reproducible.
#[derive(Default)]
pub struct PartMap {
    parts: BTreeMap<PartTypeId, Box<dyn Part>>,
}

impl PartMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a part, returning the part previously stored under `id`, if any.
    pub fn insert(&mut self, id: PartTypeId, part: Box<dyn Part>) -> Option<Box<dyn Part>> {
        self.parts.insert(id, part)
    }

    /// Remove and return the part stored under `id`, if any.
    pub fn remove(&mut self, id: &PartTypeId) -> Option<Box<dyn Part>> {
        self.parts.remove(id)
    }

    pub fn get(&self, id: &PartTypeId) -> Option<&dyn Part> {
        self.parts.get(id).map(|p| &**p)
    }

    pub fn get_mut(&mut self, id: &PartTypeId) -> Option<&mut dyn Part> {
        self.parts.get_mut(id).map(|p| &mut **p)
    }

    /// Typed access via downcasting. Returns `None` if the part is absent or is
    /// not an instance of `T`.
    pub fn get_typed<T: Part>(&self, id: &PartTypeId) -> Option<&T> {
        self.get(id)?.as_any().downcast_ref::<T>()
    }

    pub fn get_typed_mut<T: Part>(&mut self, id: &PartTypeId) -> Option<&mut T> {
        self.get_mut(id)?.as_any_mut().downcast_mut::<T>()
    }

    pub fn contains(&self, id: &PartTypeId) -> bool {
        self.parts.contains_key(id)
    }

    pub fn len(&self) -> usize {
        self.parts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    /// Iterate `(name, part)` pairs in sorted name order (deterministic).
    pub fn iter(&self) -> impl Iterator<Item = (&PartTypeId, &dyn Part)> {
        self.parts.iter().map(|(k, v)| (k, &**v))
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&PartTypeId, &mut dyn Part)> {
        self.parts.iter_mut().map(|(k, v)| (k, &mut **v))
    }

    /// Sorted part names.
    pub fn names(&self) -> impl Iterator<Item = &PartTypeId> {
        self.parts.keys()
    }
}

/// Error produced by part construction or entity lifecycle operations.
#[derive(Debug, Error)]
pub enum PartError {
    #[error("unknown part type '{0}'")]
    UnknownPartType(PartTypeId),
    #[error("part type '{0}' is already registered; registering the same name twice is an error")]
    DuplicatePartType(PartTypeId),
    #[error("building part '{name}' failed: {msg}")]
    BuildFailed { name: String, msg: String },
    #[error("part '{name}' schema validation failed: {msg}")]
    Schema { name: String, msg: String },
    #[error("entity {0:?} does not exist")]
    NoSuchEntity(Entity),
}

/// A factory that builds one concrete part instance from its raw TOML payload
/// (the table under `parts.<Name>` in a blueprint). Engine parts register here
/// at startup; mods add entries through the same registry.
pub type PartFactory = fn(&toml::Value) -> Result<Box<dyn Part>, PartError>;

/// Where a registered part is stored on a spawned entity.
///
/// Each part name declares exactly one storage class at registration
/// (single source of truth); routing on spawn uses this declaration, never
/// which spawners happen to be registered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StorageClass {
    /// An engine-native component, stamped into the `hecs` world (fast
    /// archetype queries for the hot render/sim paths).
    Component,
    /// A mod-defined / dynamic part, stored boxed in the entity's `PartMap`.
    Dynamic,
}

/// Registry of the part types the engine knows about, keyed by part name.
///
/// This is the *single source of truth* for part names: each name is declared
/// once, together with its [`StorageClass`] and a factory that deserializes a
/// TOML payload into the concrete part. The blueprint loader validates content
/// against this same registry. Iteration is in sorted name order.
#[derive(Default)]
pub struct PartRegistry {
    /// Part name → its declared factory and storage class.
    entries: BTreeMap<PartTypeId, PartEntry>,
    /// Part name → a schema-backed scripted part type. A name may exist in
    /// *either* `entries` or `script_types`, never both (single source of
    /// truth). Sorted by name, like `entries`.
    script_types: BTreeMap<PartTypeId, ScriptPartType>,
}

/// One declared part type.
#[derive(Clone, Copy)]
struct PartEntry {
    storage: StorageClass,
    factory: PartFactory,
}

impl PartRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a factory for `id` with a declared `storage` class.
    ///
    /// Registering the same name twice is an error
    /// ([`PartError::DuplicatePartType`]): there must be exactly one declared
    /// definition per part name.
    pub fn register(
        &mut self,
        id: impl Into<PartTypeId>,
        storage: StorageClass,
        factory: PartFactory,
    ) -> Result<(), PartError> {
        let id = id.into();
        if self.contains(&id) {
            return Err(PartError::DuplicatePartType(id));
        }
        self.entries.insert(id, PartEntry { storage, factory });
        Ok(())
    }

    /// The declared storage class for `id`, if registered.
    pub fn storage(&self, id: &PartTypeId) -> Option<StorageClass> {
        if let Some(e) = self.entries.get(id) {
            return Some(e.storage);
        }
        // Scripted parts are always stored as Dynamic (PartMap), never hecs.
        self.script_types
            .contains_key(id)
            .then_some(StorageClass::Dynamic)
    }

    pub fn get(&self, id: &PartTypeId) -> Option<&PartFactory> {
        self.entries.get(id).map(|e| &e.factory)
    }

    pub fn contains(&self, id: &PartTypeId) -> bool {
        self.entries.contains_key(id) || self.script_types.contains_key(id)
    }

    /// Register a schema-backed scripted part type. The schema is shape-checked
    /// at registration (fail-closed: malformed defaults or ranges are rejected
    /// before content can depend on them). A scripted part is always stored as
    /// `Dynamic` and is never a compile-time ECS component.
    pub fn register_script_part(&mut self, def: ScriptPartType) -> Result<(), PartError> {
        def.validate_schema()?;
        let id = def.name.clone();
        if self.contains(&id) {
            return Err(PartError::DuplicatePartType(id));
        }
        self.script_types.insert(id, def);
        Ok(())
    }

    /// The schema registered for a scripted part name, if any (deterministic;
    /// names are BTreeMap-sorted).
    pub fn script_schema(&self, id: &PartTypeId) -> Option<&ScriptPartType> {
        self.script_types.get(id)
    }

    /// Build a part of the registered type `id` from `config`. An unregistered
    /// name is an error — unknown part names in content must fail loudly, never
    /// be silently ignored. Scripted parts validate the TOML payload against
    /// their schema; native parts dispatch to their registered factory.
    pub fn build(&self, id: &PartTypeId, config: &toml::Value) -> Result<Box<dyn Part>, PartError> {
        if let Some(def) = self.script_types.get(id) {
            return def
                .validate(config)
                .map(|part| Box::new(part) as Box<dyn Part>);
        }
        let entry = self
            .entries
            .get(id)
            .ok_or_else(|| PartError::UnknownPartType(id.clone()))?;
        (entry.factory)(config)
    }

    /// Iterate `(name, factory)` pairs in sorted name order (native parts only;
    /// scripted parts have no factory and are surfaced via [`Self::script_schema`]).
    pub fn iter(&self) -> impl Iterator<Item = (&PartTypeId, &PartFactory)> {
        self.entries.iter().map(|(k, e)| (k, &e.factory))
    }

    /// Iterate `(name, storage)` pairs in sorted name order, scripted parts
    /// included (they are always `StorageClass::Dynamic`).
    pub fn iter_storage(&self) -> impl Iterator<Item = (&PartTypeId, StorageClass)> {
        let native = self.entries.iter().map(|(k, e)| (k, e.storage));
        let scripted = self.script_types.keys().map(|k| (k, StorageClass::Dynamic));
        // Both maps are BTreeMap-sorted and disjoint; merge and re-sort by key
        // so the combined iteration is sorted across the two tables.
        let mut items: Vec<(&PartTypeId, StorageClass)> = native.chain(scripted).collect();
        items.sort_by(|a, b| a.0.cmp(b.0));
        items.into_iter()
    }

    pub fn len(&self) -> usize {
        self.entries.len() + self.script_types.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.script_types.is_empty()
    }
}

/// Pairs the [`hecs::World`] of engine-known components with per-entity
/// [`PartMap`]s of mod-defined parts behind a single entity handle.
///
/// Spawning creates an entity in *both* the world and the part store, and
/// despawning removes it from both, so game code never deals with them
/// separately.
#[derive(Default)]
pub struct EntityBroker {
    world: hecs::World,
    /// Per-entity dynamic parts, kept in lockstep with [`Self::world`].
    part_maps: BTreeMap<Entity, PartMap>,
}

impl EntityBroker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Spawn an empty entity plus its `PartMap`, returning the single handle
    /// game code uses for both. Spawning consumes no randomness.
    pub fn spawn(&mut self) -> Entity {
        let entity = self.world.spawn(());
        self.part_maps.insert(entity, PartMap::new());
        entity
    }

    /// Spawn an entity carrying one part.
    pub fn spawn_with(&mut self, id: impl Into<PartTypeId>, part: Box<dyn Part>) -> Entity {
        let entity = self.spawn();
        self.insert_part(entity, id, part)
            .expect("spawned entity is alive");
        entity
    }

    /// Remove the entity from both the `hecs` world and the part store.
    pub fn despawn(&mut self, entity: Entity) -> Result<(), PartError> {
        self.world
            .despawn(entity)
            .map_err(|_| PartError::NoSuchEntity(entity))?;
        self.part_maps.remove(&entity);
        Ok(())
    }

    pub fn is_alive(&self, entity: Entity) -> bool {
        self.world.contains(entity)
    }

    /// Accessor for engine-known components in the underlying `hecs` world.
    pub fn world(&self) -> &hecs::World {
        &self.world
    }

    pub fn world_mut(&mut self) -> &mut hecs::World {
        &mut self.world
    }

    pub fn part_map(&self, entity: Entity) -> Option<&PartMap> {
        self.part_maps.get(&entity)
    }

    pub fn part_map_mut(&mut self, entity: Entity) -> Option<&mut PartMap> {
        self.part_maps.get_mut(&entity)
    }

    pub fn get_part(&self, entity: Entity, id: &PartTypeId) -> Option<&dyn Part> {
        self.part_map(entity).and_then(|m| m.get(id))
    }

    pub fn get_part_mut(&mut self, entity: Entity, id: &PartTypeId) -> Option<&mut dyn Part> {
        self.part_map_mut(entity).and_then(|m| m.get_mut(id))
    }

    /// Typed part access through a single entity handle.
    pub fn typed_part<T: Part>(&self, entity: Entity, id: &PartTypeId) -> Option<&T> {
        self.part_map(entity).and_then(|m| m.get_typed::<T>(id))
    }

    pub fn typed_part_mut<T: Part>(&mut self, entity: Entity, id: &PartTypeId) -> Option<&mut T> {
        self.part_map_mut(entity)
            .and_then(|m| m.get_typed_mut::<T>(id))
    }

    /// Attach a part to a live entity. Errors if the entity does not exist.
    pub fn insert_part(
        &mut self,
        entity: Entity,
        id: impl Into<PartTypeId>,
        part: Box<dyn Part>,
    ) -> Result<Option<Box<dyn Part>>, PartError> {
        let map = self
            .part_map_mut(entity)
            .ok_or(PartError::NoSuchEntity(entity))?;
        Ok(map.insert(id.into(), part))
    }

    pub fn remove_part(
        &mut self,
        entity: Entity,
        id: &PartTypeId,
    ) -> Result<Option<Box<dyn Part>>, PartError> {
        self.part_map_mut(entity)
            .ok_or(PartError::NoSuchEntity(entity))
            .map(|m| m.remove(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Clone, Debug, PartialEq, Deserialize)]
    struct TestStats {
        strength: i64,
        agility: i64,
    }

    fn build_stats(config: &toml::Value) -> Result<Box<dyn Part>, PartError> {
        let s: TestStats = config
            .clone()
            .try_into()
            .map_err(|e| PartError::BuildFailed {
                name: "Stats".into(),
                msg: e.to_string(),
            })?;
        Ok(Box::new(s))
    }

    #[test]
    fn part_map_typed_access_and_downcast() {
        let mut map = PartMap::new();
        let stats = PartTypeId::new("Stats");
        map.insert(
            stats.clone(),
            Box::new(TestStats {
                strength: 10,
                agility: 5,
            }),
        );

        assert!(map.contains(&stats));
        assert_eq!(
            map.get_typed::<TestStats>(&stats),
            Some(&TestStats {
                strength: 10,
                agility: 5
            })
        );
        // Wrong type downcasts to None.
        assert!(map.get_typed::<i32>(&stats).is_none());

        map.get_typed_mut::<TestStats>(&stats).unwrap().strength = 99;
        assert_eq!(map.get_typed::<TestStats>(&stats).unwrap().strength, 99);

        let removed = map.remove(&stats);
        assert!(removed.is_some());
        assert!(!map.contains(&stats));
        assert!(map.get_typed::<TestStats>(&stats).is_none());
    }

    #[test]
    fn part_map_iterates_in_sorted_name_order() {
        let mut map = PartMap::new();
        map.insert(PartTypeId::new("Zebra"), Box::new(1i32));
        map.insert(PartTypeId::new("Apple"), Box::new(2i32));
        map.insert(PartTypeId::new("mango"), Box::new(3i32));
        let names: Vec<String> = map.iter().map(|(id, _)| id.as_str().to_string()).collect();
        // BTreeMap orders by bytes: uppercase sorts before lowercase.
        assert_eq!(names, vec!["Apple", "Zebra", "mango"]);
    }

    #[test]
    fn registry_builds_typed_part_from_toml() {
        let mut reg = PartRegistry::new();
        let stats = PartTypeId::new("Stats");
        reg.register(stats.clone(), StorageClass::Dynamic, build_stats)
            .unwrap();
        let cfg: toml::Value = toml::from_str("strength = 18\nagility = 6").unwrap();
        let part = reg.build(&stats, &cfg).unwrap();
        // Downcast through `&dyn Part` (calling `as_any` directly on the Box
        // would resolve to the `Part` impl of `Box<dyn Part>`).
        let s = part.as_ref().as_any().downcast_ref::<TestStats>().unwrap();
        assert_eq!(
            s,
            &TestStats {
                strength: 18,
                agility: 6
            }
        );
    }

    #[test]
    fn registry_records_declared_storage_class() {
        let mut reg = PartRegistry::new();
        reg.register(
            PartTypeId::new("Render"),
            StorageClass::Component,
            build_stats,
        )
        .unwrap();
        reg.register(PartTypeId::new("Stats"), StorageClass::Dynamic, build_stats)
            .unwrap();
        assert_eq!(
            reg.storage(&PartTypeId::new("Render")),
            Some(StorageClass::Component)
        );
        assert_eq!(
            reg.storage(&PartTypeId::new("Stats")),
            Some(StorageClass::Dynamic)
        );
        assert_eq!(reg.storage(&PartTypeId::new("Nope")), None);
    }

    #[test]
    fn registry_rejects_duplicate_registration() {
        let mut reg = PartRegistry::new();
        reg.register(PartTypeId::new("Stats"), StorageClass::Dynamic, build_stats)
            .unwrap();
        let err = reg
            .register(PartTypeId::new("Stats"), StorageClass::Dynamic, build_stats)
            .unwrap_err();
        assert!(matches!(err, PartError::DuplicatePartType(id) if id.as_str() == "Stats"));
        // The original entry is untouched.
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn registry_build_unknown_type_errors() {
        let reg = PartRegistry::new();
        let cfg: toml::Value = toml::from_str("x = 1").unwrap();
        assert!(matches!(
            reg.build(&PartTypeId::new("Nope"), &cfg),
            Err(PartError::UnknownPartType(_))
        ));
    }

    #[test]
    fn broker_spawn_despawn_removes_both() {
        let mut broker = EntityBroker::new();
        let e = broker.spawn();
        assert!(broker.is_alive(e));

        let stats = PartTypeId::new("Stats");
        broker
            .insert_part(
                e,
                stats.clone(),
                Box::new(TestStats {
                    strength: 1,
                    agility: 1,
                }),
            )
            .unwrap();
        assert!(broker.typed_part::<TestStats>(e, &stats).is_some());

        broker.despawn(e).unwrap();
        assert!(!broker.is_alive(e));
        assert!(broker.part_map(e).is_none());
        assert!(matches!(broker.despawn(e), Err(PartError::NoSuchEntity(_))));
    }

    #[test]
    fn broker_pairs_hecs_components_and_parts() {
        #[derive(Debug, PartialEq)]
        struct RenderTile {
            tile: String,
        }

        let mut broker = EntityBroker::new();
        let e = broker.spawn();
        broker
            .world_mut()
            .insert(
                e,
                (RenderTile {
                    tile: "creature.player".into(),
                },),
            )
            .unwrap();
        broker
            .insert_part(
                e,
                "Stats",
                Box::new(TestStats {
                    strength: 10,
                    agility: 5,
                }),
            )
            .unwrap();

        // Both reachable through the single handle:
        {
            let tile = broker.world().get::<&RenderTile>(e).unwrap();
            assert_eq!(tile.tile, "creature.player");
        }
        assert_eq!(
            broker
                .typed_part::<TestStats>(e, &PartTypeId::new("Stats"))
                .unwrap()
                .strength,
            10
        );

        broker.despawn(e).unwrap();
        assert!(broker.world().get::<&RenderTile>(e).is_err());
        assert!(broker.part_map(e).is_none());
    }

    #[test]
    fn broker_entities_are_isolated() {
        let mut broker = EntityBroker::new();
        let a = broker.spawn_with(
            "Stats",
            Box::new(TestStats {
                strength: 1,
                agility: 1,
            }),
        );
        let b = broker.spawn_with(
            "Stats",
            Box::new(TestStats {
                strength: 2,
                agility: 2,
            }),
        );
        assert_ne!(a, b);
        let stats = PartTypeId::new("Stats");
        assert_eq!(
            broker.typed_part::<TestStats>(a, &stats).unwrap().strength,
            1
        );
        assert_eq!(
            broker.typed_part::<TestStats>(b, &stats).unwrap().strength,
            2
        );
    }

    #[test]
    fn insert_part_on_missing_entity_errors() {
        let mut broker = EntityBroker::new();
        // A dummy handle that was never spawned.
        let e = Entity::DANGLING;
        assert!(matches!(
            broker.insert_part(
                e,
                "Stats",
                Box::new(TestStats {
                    strength: 1,
                    agility: 1
                })
            ),
            Err(PartError::NoSuchEntity(_))
        ));
    }

    // --- scripted part registry integration tests ---

    /// A `Charge` script part schema with an `int charges` field (default 1,
    /// clamped to [0, 99]), matching the Zapper demo data shape.
    fn charge_schema() -> crate::script_part::ScriptPartType {
        use crate::script_part::{FieldDef, PartValue};
        use std::collections::BTreeMap;
        let mut schema = BTreeMap::new();
        schema.insert(
            "charges".to_string(),
            FieldDef::int().default(PartValue::Int(1)).min(0).max(99),
        );
        crate::script_part::ScriptPartType::new("Charge", schema)
    }

    #[test]
    fn register_script_part_builds_dynamic_script_part() {
        use crate::script_part::{PartValue, ScriptPart};

        let mut reg = PartRegistry::new();
        reg.register_script_part(charge_schema()).unwrap();

        let id = PartTypeId::new("Charge");
        // Storage class is always Dynamic for scripted parts.
        assert_eq!(reg.storage(&id), Some(StorageClass::Dynamic));
        // Deterministic schema lookup returns the registered schema.
        assert_eq!(reg.script_schema(&id), Some(&charge_schema()));

        // build() validates a TOML payload into a ScriptPart via the registry.
        let cfg: toml::Value = toml::from_str("charges = 3").unwrap();
        let part = reg.build(&id, &cfg).unwrap();
        let sp = part.as_ref().as_any().downcast_ref::<ScriptPart>().unwrap();
        assert_eq!(sp.get("charges"), Some(&PartValue::Int(3)));

        // The registry entry counts toward len/is_empty and reports Dynamic.
        assert_eq!(reg.len(), 1);
        assert!(!reg.is_empty());
    }

    #[test]
    fn script_part_duplicate_names_are_rejected() {
        use crate::script_part::ScriptPartType;

        let mut reg = PartRegistry::new();
        reg.register_script_part(charge_schema()).unwrap();

        // Same scripted name -> duplicate.
        let err = reg.register_script_part(charge_schema()).unwrap_err();
        assert!(matches!(err, PartError::DuplicatePartType(id) if id.as_str() == "Charge"));

        // A native part cannot steal a scripted name and vice versa (one
        // declared definition per name).
        fn build_factory(_: &toml::Value) -> Result<Box<dyn Part>, PartError> {
            Ok(Box::new(1i32))
        }
        let err = reg
            .register(
                PartTypeId::new("Charge"),
                StorageClass::Component,
                build_factory,
            )
            .unwrap_err();
        assert!(matches!(err, PartError::DuplicatePartType(id) if id.as_str() == "Charge"));

        // Registering a script part over an existing native name is likewise an
        // error.
        let mut reg2 = PartRegistry::new();
        reg2.register(
            PartTypeId::new("Stats"),
            StorageClass::Dynamic,
            build_factory,
        )
        .unwrap();
        let err = reg2
            .register_script_part(ScriptPartType::new(
                "Stats",
                std::collections::BTreeMap::new(),
            ))
            .unwrap_err();
        assert!(matches!(err, PartError::DuplicatePartType(id) if id.as_str() == "Stats"));
    }

    #[test]
    fn script_schema_lookups_are_sorted_and_deterministic() {
        use crate::script_part::ScriptPartType;

        let mut reg = PartRegistry::new();
        // Register in mixed, non-sorted order.
        for name in ["zeta", "alpha", "mango", "Bravo"] {
            reg.register_script_part(ScriptPartType::new(name, std::collections::BTreeMap::new()))
                .unwrap();
        }
        // script_types-backed iteration is BTreeMap-sorted by name (bytes order:
        // uppercase sorts before lowercase).
        let schema_names: Vec<String> = ["Bravo", "alpha", "mango", "zeta"]
            .iter()
            .map(|n| {
                reg.script_schema(&PartTypeId::new(*n))
                    .unwrap()
                    .name
                    .as_str()
                    .to_string()
            })
            .collect();
        assert_eq!(schema_names, vec!["Bravo", "alpha", "mango", "zeta"]);
        // Names cover native + scripted in sorted order via iter_storage.
        let all: Vec<&str> = reg.iter_storage().map(|(id, _)| id.as_str()).collect();
        assert_eq!(all, vec!["Bravo", "alpha", "mango", "zeta"]);
        // Every scripted name reports Dynamic storage.
        for name in all {
            assert_eq!(
                reg.storage(&PartTypeId::new(name)),
                Some(StorageClass::Dynamic)
            );
        }
    }
}
