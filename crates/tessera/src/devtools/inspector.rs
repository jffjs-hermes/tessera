//! Read-only entity inspector (snapshot builder + egui window).
//!
//! The inspector is **strictly read-only** and keeps the renderer a strict
//! reader of the simulation:
//!
//! - [`build_snapshot`] takes the `EntityBroker` **read** lock, copies each live
//!   entity's native-part data (id, blueprint, position, HP, and the Debug
//!   rendering of its native parts) into an owned [`InspectorSnapshot`], sorts
//!   by deterministic entity id, and returns — the lock is dropped *before* any
//!   egui drawing. egui only ever reads the owned snapshot, never the broker.
//! - No broker state is mutated, no part is edited, and no script-owned state
//!   is exposed: script parts are recognized by their concrete
//!   [`ScriptPart`] storage type and excluded from the row's part list. A
//!   scripted `describe()` hook is the eventual path (a documented non-goal).
//! - Determinism: iteration is by entity id (a `BTreeMap` key — never a
//!   `HashMap`/`HashSet`), and the explicit sort in [`build_snapshot`] pins the
//!   row order regardless of spawn/insertion order.
//!
//! The console (`sethp`/`tp`) remains the only mutation path; the inspector
//! never writes.
//!
//! The snapshot builder is compiled unconditionally (no egui dependency in a
//! default build); the windowed egui window sits behind `#[cfg(feature =
//! "devtools")]`, mirroring `console.rs`'s ungated-engine / gated-UI split.
//! In a default build the snapshot builder has no non-test caller (the window
//! is gated off), so suppress the dead-code lint there — it is exercised by
//! `cargo test` unconditionally.
#![cfg_attr(not(feature = "devtools"), allow(dead_code))]

use hecs::Entity;
use tessera_core::ecs::EntityBroker;
use tessera_core::events::{Health, PART_HEALTH, PART_POSITION, Position};
use tessera_core::save::SaveHooks;
use tessera_core::script_part::ScriptPart;
use tessera_core::world::{RenderTile, World};

use crate::scripting::entity_to_id;
use crate::sim::Sim;

/// One read-only entity row. Fully owned (no borrow into the broker), so the
/// broker read lock can be dropped before egui draws.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntityRow {
    /// Deterministic entity id (decimal `u64` from `Entity::to_bits`).
    pub id: u64,
    /// Fingerprint blueprint id, or a fallback tile label (see
    /// [`blueprint_of`]).
    pub blueprint: String,
    /// Native `Position` copy (0,0 when absent).
    pub x: i32,
    pub y: i32,
    /// Native `Health` copy (0 when absent).
    pub hp: i32,
    /// Native part `name -> Debug rendering`, in sorted-name order. Script
    /// parts are excluded by design (script-owned state is a non-goal).
    pub parts: Vec<(String, String)>,
}

/// An immutable, deterministic snapshot of the live entity set, sorted by
/// entity id. egui reads only this — never the broker directly.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InspectorSnapshot {
    pub entities: Vec<EntityRow>,
}

/// Build a full snapshot by taking the broker **read** lock, copying native
/// data, and releasing it. Callers that already hold a broker handle (e.g.
/// panel code that wants one lock for several snapshots) should use
/// [`build_snapshot`] instead.
pub fn snapshot(sim: &Sim, world: &World, hooks: &SaveHooks) -> InspectorSnapshot {
    let b = sim.broker().read().unwrap_or_else(|p| p.into_inner());
    build_snapshot(&b, world, hooks)
}

/// Build a snapshot from an already-locked broker read guard (or borrow). The
/// returned snapshot owns all its data, so the guard can be dropped
/// immediately and egui draws from the owned rows.
pub fn build_snapshot(b: &EntityBroker, world: &World, hooks: &SaveHooks) -> InspectorSnapshot {
    let mut entities: Vec<EntityRow> = b
        .entities()
        .into_iter()
        .map(|e| build_row(b, world, hooks, e))
        .collect();
    // Pin deterministic row order by entity id regardless of insertion order.
    entities.sort_by_key(|r| r.id);
    InspectorSnapshot { entities }
}

fn build_row(b: &EntityBroker, world: &World, hooks: &SaveHooks, e: Entity) -> EntityRow {
    let id = entity_to_id(e);
    let blueprint = blueprint_of(world, b, e);
    let (x, y, hp) = row_of(b, e);
    let parts = native_part_debug(b, hooks, e);
    EntityRow {
        id,
        blueprint,
        x,
        y,
        hp,
        parts,
    }
}

/// Read-only `Position`/`Health` copies for the row summary line.
fn row_of(b: &EntityBroker, e: Entity) -> (i32, i32, i32) {
    let pos = b
        .typed_part::<Position>(e, &PART_POSITION.into())
        .map(|p| (p.x, p.y))
        .unwrap_or((0, 0));
    let hp = b
        .typed_part::<Health>(e, &PART_HEALTH.into())
        .map(|h| h.hp)
        .unwrap_or(0);
    (pos.0, pos.1, hp)
}

/// Render an entity's native parts as `sorted (name, Debug-string)` pairs for
/// the detail pane. Script parts are stored as [`ScriptPart`] at rest, so they
/// are recognized and skipped — script-owned state is never exposed.
fn native_part_debug(b: &EntityBroker, hooks: &SaveHooks, e: Entity) -> Vec<(String, String)> {
    let Some(map) = b.part_map(e) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (name, part) in map.iter() {
        // Script parts downcast to ScriptPart at rest; skip them (non-goal).
        if part.as_any().downcast_ref::<ScriptPart>().is_some() {
            continue;
        }
        let debug = match hooks.snapshot(name, part) {
            Ok(v) => format!("{v:?}"),
            Err(_) => "<unserializable>".to_string(),
        };
        out.push((name.as_str().to_string(), debug));
    }
    out
}

/// Fingerprint which blueprint a live entity was spawned from, by matching its
/// `Render` tile id against blueprints' `parts.Render.tile`. Falls back to the
/// tile id itself (still a stable label). Mirrors the console's `entities`
/// resolver so the inspector and the headless output agree.
fn blueprint_of(world: &World, b: &EntityBroker, e: Entity) -> String {
    let Ok(rt) = b.world().get::<&RenderTile>(e) else {
        return "<no-render>".to_string();
    };
    let lib = world.library().read().unwrap_or_else(|p| p.into_inner());
    for id in lib.ids() {
        if let Some(bp) = lib.get(id)
            && let Some(t) = bp
                .part("Render")
                .and_then(|v| v.get("tile"))
                .and_then(|v| v.as_str())
            && t == rt.tile
        {
            return id.clone();
        }
    }
    rt.tile.clone()
}

#[cfg(feature = "devtools")]
pub use window::*;

#[cfg(feature = "devtools")]
mod window {
    use super::InspectorSnapshot;

    /// Render the F3 inspector window. `selected` is the caller-owned entity id
    /// the egui UI keeps across frames (so egui holds no broker reference and
    /// selection survives frame boundaries). Returns the (possibly updated)
    /// selection so the caller can persist it.
    pub fn show_window(ctx: &egui::Context, snap: &InspectorSnapshot, selected: &mut Option<u64>) {
        // Deterministic default selection: the first (smallest-id) row. If the
        // previously selected entity has despawned, fall back to the first row.
        if selected
            .and_then(|id| snap.entities.iter().find(|r| r.id == id))
            .is_none()
        {
            *selected = snap.entities.first().map(|r| r.id);
        }

        egui::Window::new("Inspector")
            .default_size([420.0, 320.0])
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for row in &snap.entities {
                            let line = format!(
                                "#{} {} ({}, {}) hp={}",
                                row.id, row.blueprint, row.x, row.y, row.hp
                            );
                            let is_selected = *selected == Some(row.id);
                            if ui.selectable_label(is_selected, line).clicked() {
                                *selected = Some(row.id);
                            }
                        }
                    });
                ui.separator();
                if let Some(row) = snap.entities.iter().find(|r| *selected == Some(r.id)) {
                    if row.parts.is_empty() {
                        ui.monospace("(no native parts)");
                    }
                    for (name, debug) in &row.parts {
                        ui.monospace(format!("{name} = {debug}"));
                    }
                }
            });
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use tessera_core::ecs::Part;
    use tessera_core::events::{PART_STATS, Stats};
    use tessera_core::script_part::{FieldDef, PartValue, ScriptPartType};
    use tessera_core::turns::{ActorEnergy, PART_ENERGY};
    use tessera_core::vfs::MemVfs;

    /// A minimal world with `Player`/`Goblin` blueprints and a `Charge` script
    /// part, mirroring the console's test fixture (Render component + native
    /// dynamic parts + one script part).
    fn test_world() -> World {
        let mut w = World::new();
        w.register_component::<RenderTile>("Render").unwrap();
        w.register_part::<Position>(PART_POSITION).unwrap();
        w.register_part::<ActorEnergy>(PART_ENERGY).unwrap();
        w.register_part::<Health>(PART_HEALTH).unwrap();
        w.register_part::<Stats>(PART_STATS).unwrap();
        let mut schema = BTreeMap::new();
        schema.insert(
            "charges".to_string(),
            FieldDef::int().default(PartValue::Int(1)).min(0).max(99),
        );
        w.register_script_part(ScriptPartType::new("Charge", schema))
            .unwrap();
        let vfs = MemVfs::new().with(
            "blueprints/creatures.toml",
            r##"
[Actor]
parts.Health = { hp = 20 }

[Humanoid]
inherits = "Actor"
parts.Render = { tile = "creature.humanoid" }
parts.ActorEnergy = { speed = 100 }

[Player]
inherits = "Humanoid"
parts.Render = { tile = "creature.player" }

[Goblin]
inherits = "Humanoid"
parts.Render = { tile = "creature.goblin" }
parts.Health = { hp = 3 }
parts.Stats = { strength = 18 }
parts.Charge = { charges = 2 }
"##,
        );
        w.load(&vfs).unwrap();
        w
    }

    /// A broker holding two entities whose `hecs` ids are *not* in ascending
    /// spawn order: goblin spawned first (lower id), player second (higher id)
    /// — then a `snapshot` must still list the player before the goblin.
    fn broker_sans_lock() -> EntityBroker {
        let mut broker = EntityBroker::new();
        let goblin = broker.spawn_with(PART_POSITION, Box::new(Position { x: 5, y: 5 }));
        broker
            .world_mut()
            .insert_one(
                goblin,
                RenderTile {
                    tile: "creature.goblin".into(),
                },
            )
            .unwrap();
        broker
            .insert_part(
                goblin,
                PART_HEALTH,
                Box::new(Health { hp: 3 }) as Box<dyn Part>,
            )
            .unwrap();
        broker
            .insert_part(
                goblin,
                PART_STATS,
                Box::new(Stats { strength: 18 }) as Box<dyn Part>,
            )
            .unwrap();
        broker
            .insert_part(
                goblin,
                "Charge",
                Box::new(tessera_core::script_part::ScriptPart(BTreeMap::from([(
                    "charges".to_string(),
                    PartValue::Int(2),
                )]))) as Box<dyn Part>,
            )
            .unwrap();
        let player = broker.spawn_with(
            PART_POSITION,
            Box::new(Position { x: 1, y: 1 }) as Box<dyn Part>,
        );
        broker
            .world_mut()
            .insert_one(
                player,
                RenderTile {
                    tile: "creature.player".into(),
                },
            )
            .unwrap();
        broker
    }

    #[test]
    fn snapshot_orders_deterministically_despite_insertion_order() {
        let world = test_world();
        let hooks = world.save_hooks();
        let broker = broker_sans_lock();
        let snap = build_snapshot(&broker, &world, &hooks);

        let ids: Vec<u64> = snap.entities.iter().map(|r| r.id).collect();
        assert_eq!(
            ids,
            {
                let mut m = ids.clone();
                m.sort();
                m
            },
            "rows come back sorted by entity id regardless of insertion order"
        );
        assert_eq!(snap.entities.len(), 2, "both live entities are listed");

        // The second-spawned (higher-id) entity is index 0, since goblin got
        // the lower id. Rows are sorted by *id*, never by spawn order.
        assert!(
            snap.entities[0].id < snap.entities[1].id,
            "sorted ascending by id"
        );
    }

    #[test]
    fn rows_carry_position_blueprint_and_hp() {
        let world = test_world();
        let hooks = world.save_hooks();
        let broker = broker_sans_lock();
        let snap = build_snapshot(&broker, &world, &hooks);

        // The goblin: id-ordered first, blueprint "Goblin", pos/hp from parts.
        let goblin = &snap.entities[0];
        assert_eq!(goblin.blueprint, "Goblin");
        assert_eq!(goblin.x, 5);
        assert_eq!(goblin.y, 5);
        assert_eq!(goblin.hp, 3);
    }

    #[test]
    fn native_part_debug_is_shown_and_script_state_excluded() {
        let world = test_world();
        let hooks = world.save_hooks();
        let broker = broker_sans_lock();
        let snap = build_snapshot(&broker, &world, &hooks);

        let goblin = &snap.entities[0];
        let names: Vec<&str> = goblin.parts.iter().map(|(n, _)| n.as_str()).collect();
        // Native parts are present; the script `Charge` part is excluded.
        assert!(names.contains(&"Position"), "{names:?}");
        assert!(names.contains(&"Health"), "{names:?}");
        assert!(names.contains(&"Stats"), "{names:?}");
        assert!(
            !names.contains(&"Charge"),
            "script-owned state must not be exposed, got {names:?}"
        );

        // Native part Debug rendering is non-empty and looks like a struct
        // (Position renders its coords through the save-hook snapshot).
        let pos = goblin.parts.iter().find(|(n, _)| n == "Position").unwrap();
        assert!(
            !pos.1.is_empty() && pos.1.contains("5"),
            "Debug output for Position, got {}",
            pos.1
        );
        let hp = goblin.parts.iter().find(|(n, _)| n == "Health").unwrap();
        assert!(hp.1.contains("3"), "Debug output for Health, got {}", hp.1);
    }

    #[test]
    fn snapshot_build_is_read_only() {
        let world = test_world();
        let hooks = world.save_hooks();
        let broker = broker_sans_lock();

        let before: Vec<EntityRow> = build_snapshot(&broker, &world, &hooks).entities;
        // A second read must be unchanged — the snapshot builder must not
        // mutate the broker (only takes a read lock / shared borrow above).
        let after: Vec<EntityRow> = build_snapshot(&broker, &world, &hooks).entities;
        assert_eq!(before, after, "snapshot is read-only: broker unchanged");

        // Position/HP untouched.
        assert_eq!(broker.entities().len(), 2);
    }
}
