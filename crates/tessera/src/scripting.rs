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
//! Script-declared schemas are registered into the host's own registry at
//! script load (via `engine.define_part`); this module additionally mirrors
//! every host-registered script part into the [`World`] registry so that
//! blueprint `parts.<ScriptPart>` keys validate against the same registry the
//! loader checks (this is a hard ordering requirement: the world registry must
//! know `Charge` before `zapper.toml`'s `parts.Charge` is validated).

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex, RwLock};

use anyhow::Result;
use hecs::Entity;
use tessera_core::ecs::{EntityBroker, Part, PartTypeId};
use tessera_core::events::Health;
use tessera_core::script_part::{PartValue, ScriptPart};
use tessera_core::world::World;
use tessera_script::{ScriptError, ScriptHost, WorldAccess};

/// A [`WorldAccess`] over the live, shared broker. Immutable reads lock the
/// broker's read guard per call; mutations take the write guard per call. No
/// handles must be held across a call into the host (value boundary).
struct SimWorldAccess {
    broker: Arc<RwLock<EntityBroker>>,
}

impl SimWorldAccess {
    /// Serialize a part's visible fields into the plain `PartValue` form the
    /// host snapshot and `engine.get_part` consume. A scripted `ScriptPart`
    /// returns its closed field map; the demo's writable native part
    /// (`Health`) returns its `hp` field. Other native parts read as `None`
    /// (invisible to scripts in v1).
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
        None
    }
}

impl WorldAccess for SimWorldAccess {
    fn try_entity(&self, entity: u64) -> bool {
        let Some(e) = Entity::from_bits(entity) else {
            return false;
        };
        self.broker
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .is_alive(e)
    }

    fn snapshot_part(&self, entity: u64, part: &str) -> Option<BTreeMap<String, PartValue>> {
        let e = Entity::from_bits(entity)?;
        let id = PartTypeId::new(part);
        let p = self.broker.read().unwrap_or_else(|p| p.into_inner());
        let part_obj = p.part_map(e)?.get(&id)?;
        Self::serialize(part_obj)
    }

    fn entity_parts(&self, entity: u64) -> Vec<String> {
        let Some(e) = Entity::from_bits(entity) else {
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
        let e = Entity::from_bits(entity).ok_or(ScriptError::UnknownEntity { entity })?;
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
        // Spawning is an application-level action (entities are spawned from
        // blueprints in Rust); script-triggered spawn is not part of the base demo.
        Err(ScriptError::Internal(
            "scripted spawn not wired in base demo; spawn entities via blueprints in Rust".into(),
        ))
    }

    fn apply_despawn(&mut self, entity: u64) -> Result<(), ScriptError> {
        let e = Entity::from_bits(entity).ok_or(ScriptError::UnknownEntity { entity })?;
        let mut b = self.broker.write().unwrap_or_else(|p| p.into_inner());
        b.despawn(e)
            .map_err(|er| ScriptError::Internal(format!("despawn: {er}")))
    }
}

/// Owns the [`ScriptHost`] and its schema-sync to the [`World`] registry.
#[derive(Clone)]
pub struct ScriptBridge {
    /// The host is single-threaded (the sim drives it synchronously on one
    /// thread; mlua's `Lua` is not `Send`/`Sync`), so an `Rc` — not `Arc` —
    /// is the correct shared wrapper. Cloning the bridge shares the handle.
    host: Rc<Mutex<ScriptHost>>,
}

impl ScriptBridge {
    /// Create the bridge over a shared broker. The host is empty until
    /// [`Self::load`] is called.
    pub fn new(broker: Arc<RwLock<EntityBroker>>) -> ScriptBridge {
        let wa = SimWorldAccess { broker };
        let host = ScriptHost::new(Box::new(wa));
        ScriptBridge {
            host: Rc::new(Mutex::new(host)),
        }
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
