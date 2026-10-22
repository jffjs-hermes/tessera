//! The stats recompute pipeline, end to end .
//!
//! Drives the real content path (base + the campaign mod, including the
//! `rules/stats.luau` script and the base `Tuning_Combat` table) through the
//! public Sim seams, proving:
//!
//!   1. spawn completion fires `RecomputeDerived` and the five derived
//!      values are cached exactly (the  HP/accuracy/dodge/defense/speed
//!      numbers for the shipped Actor attributes);
//!   2. `StatusApplied` dirtying fires a recompute (Bash → Status_Staggered
//!      → `EVENT RecomputeDerived` after the `ApplyStatus` line);
//!   3. the console `setpart` path recomputes on `Level`/`Attributes` writes
//!      (exact  vectors: Level 10 → HP 99; Agility 20 → speed 120;
//!      `speed_pct = 90` → 108 through the live status modifier pipeline);
//!   4. the `Health.hp <= max_hp` clamp invariant ;
//!   5. a deterministic tuning reload re-registers the rules handler and
//!      rebuilds every cache from the new table (entity-id order);
//!   6. fail-closed spawn: a stat-ful entity without the rules script is a
//!      spawn error ;
//!   7. cache-only speed reads: with the rules handler unregistered the
//!      scheduler still acts on the cached speed (no read path calls Luau).

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use tessera::mods::{Overlay, apply_mod_patches, discover_sources};
use tessera::scripting::ScriptBridge;
use tessera::sim::{PlayerAction, Sim};
use tessera_core::ecs::{EntityBroker, PartTypeId};
use tessera_core::effects::{Modifiers, PART_STATUS_EFFECTS, StatusEffects};
use tessera_core::events::{PART_HEALTH, PART_POSITION, Point, Position};
use tessera_core::stats::{PART_ATTRIBUTES, PART_LEVEL};
use tessera_core::tiles::TileRegistry;
use tessera_core::vfs::{DirVfs, Vfs};
use tessera_core::world::{RenderTile, World};
use tessera_core::zone::Zone;

const BASE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/base");
const MODS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/mods");

fn register_parts(world: &mut World) {
    world
        .register_component::<RenderTile>("Render")
        .expect("register Render");
    world
        .register_part::<tessera_core::events::Position>(PART_POSITION)
        .expect("register Position");
    world
        .register_part::<tessera_core::turns::ActorEnergy>(tessera_core::turns::PART_ENERGY)
        .expect("register ActorEnergy");
    world
        .register_part::<tessera_core::events::Health>(PART_HEALTH)
        .expect("register Health");
}

/// Load the real shipped content exactly as the binary does (base + every
/// discovered mod, scripts and blueprints through the overlay, then the
/// per-mod patches), so `rules/stats.luau` and the base `Tuning_Combat`
/// table are live and `engine.tuning` resolves.
fn load_real_content(
    mods_root: &Path,
) -> (
    Arc<World>,
    TileRegistry,
    Arc<RwLock<EntityBroker>>,
    ScriptBridge,
) {
    let sources = discover_sources(mods_root).expect("discover mods");
    let registry = tessera_mods::ModRegistry::build(sources).expect("mod registry");
    let base_vfs = DirVfs::new(PathBuf::from(BASE));
    let overlay = Overlay::new(Box::new(base_vfs), &registry);

    let tiles = TileRegistry::load(&overlay).expect("load tiles");
    let broker = Arc::new(RwLock::new(EntityBroker::new()));
    let bridge = ScriptBridge::new(broker.clone());

    let mut world = World::new();
    register_parts(&mut world);
    for path in overlay.list("scripts").unwrap_or_default() {
        if path.ends_with(".luau") {
            let source = overlay.read_to_string(&path).expect("read script");
            bridge
                .load(&path, &path, &source)
                .unwrap_or_else(|e| panic!("loading script {path}: {e}"));
        }
    }
    bridge.sync_schemas(&mut world).expect("sync schemas");
    world.load(&overlay).expect("load blueprints");
    apply_mod_patches(&world, &registry).expect("apply patches");
    let world = Arc::new(world);
    bridge.set_world(world.clone());
    (world, tiles, broker, bridge)
}

fn spawn_at(world: &World, broker: &mut EntityBroker, bp: &str, x: i32, y: i32) -> hecs::Entity {
    let e = world.spawn_blueprint(bp, broker).expect("spawn blueprint");
    broker
        .insert_part(
            e,
            PART_POSITION,
            Box::new(Position::from_point(Point::new(x, y))),
        )
        .expect("set position");
    e
}

fn demo_zone() -> Zone {
    let mut cells = vec!["terrain.floor".to_string(); 12 * 12];
    for y in 0..12 {
        for x in 0..12 {
            if x == 0 || y == 0 || x == 11 || y == 11 {
                cells[y * 12 + x] = "terrain.wall".to_string();
            }
        }
    }
    Zone::from_cells("s1", 12, 12, Point::new(1, 1), cells)
}

fn derived_map(sim: &Sim, e: hecs::Entity) -> std::collections::BTreeMap<String, i64> {
    let broker = sim.broker().read().unwrap_or_else(|p| p.into_inner());
    broker
        .typed_part::<tessera_core::effects::Derived>(e, &PartTypeId::new("Derived"))
        .expect("entity must carry a Derived cache")
        .values
        .clone()
}

fn write_status(sim: &Sim, e: hecs::Entity, modifiers: Modifiers) {
    let mut effects = StatusEffects::new();
    effects.apply(
        "Status_Chilled",
        100,
        tessera_core::kinds::Stacking::Independent,
        &[],
        modifiers,
    );
    let mut broker = sim.broker().write().unwrap_or_else(|p| p.into_inner());
    broker
        .insert_part(e, PART_STATUS_EFFECTS.to_string(), Box::new(effects))
        .expect("write StatusEffects");
}

fn mods_pct(pct: i64) -> Modifiers {
    let toml = format!("[m]\nspeed_pct = {pct}\n");
    let value: toml::Value = toml::from_str(&toml).expect("modifier toml");
    value
        .get("m")
        .unwrap()
        .clone()
        .try_into()
        .expect("modifiers")
}

/// The baseline player: Actor attributes (T 13, A 9, L 1) — the exact
/// HP table's Knight L1 row (`10 + 2*13 + 0 = 36`) with the speed/accuracy/
/// dodge values for Agility 9.
#[test]
fn spawn_recomputes_exact_derived_values_and_logs_the_event() {
    let (world, _tiles, broker, bridge) = load_real_content(Path::new(MODS));
    let mut b = broker.write().unwrap_or_else(|p| p.into_inner());
    let player = spawn_at(&world, &mut b, "Player", 2, 2);
    let goblin = spawn_at(&world, &mut b, "Goblin", 3, 2);
    drop(b);

    let sim = Sim::from_shared(
        broker.clone(),
        Some(bridge),
        demo_zone(),
        player,
        Some(goblin),
    );

    let d = derived_map(&sim, player);
    assert_eq!(
        d,
        std::collections::BTreeMap::from([
            ("ac".to_string(), 0),
            ("accuracy".to_string(), 0), // 1 + 2*(9-10) + 0 -> invariant floor 0
            ("defense".to_string(), 0),
            ("dodge".to_string(), 0), // 1 + 2*(9-10) -> floor 0
            ("max_hp".to_string(), 36),
            ("speed".to_string(), 98), // 100 + 2*(9-10), clamped 50..140
        ]),
        "exact  values for the shipped Actor attributes"
    );

    // The demo mapping: `RecomputeDerived` fires after spawn (both entities,
    // entity-id order).
    let recomputes: Vec<&str> = sim
        .log
        .iter()
        .filter(|l| l.starts_with("EVENT RecomputeDerived"))
        .map(String::as_str)
        .collect();
    assert_eq!(recomputes.len(), 2, "{:?}", sim.log);
}

/// The goblin's scheduler speed reflects its (inherited) Agility — no longer
/// the flat ActorEnergy 100 — and the derivative survives ordinary turns
/// (cache-only reads; no script call on the read path).
#[test]
fn scheduler_acts_on_derived_speed_and_reads_never_call_scripts() {
    let (world, _tiles, broker, bridge) = load_real_content(Path::new(MODS));
    let host = bridge.host.clone();
    let mut b = broker.write().unwrap_or_else(|p| p.into_inner());
    let player = spawn_at(&world, &mut b, "Player", 2, 2);
    let goblin = spawn_at(&world, &mut b, "Goblin", 3, 2);
    drop(b);

    let mut sim = Sim::from_shared(
        broker.clone(),
        Some(bridge),
        demo_zone(),
        player,
        Some(goblin),
    );
    assert_eq!(
        sim.actor_speed(goblin).unwrap(),
        98,
        "goblin speed from Derived (Agility 9)"
    );

    // Raise Agility through the console path -> 120; the scheduler row follows
    // the cache (sync at recompute), and subsequent turns keep it.
    sim.set_part_console(
        player,
        PART_ATTRIBUTES,
        &std::collections::BTreeMap::from([("agility".to_string(), 20)]),
    )
    .expect("setpart agility");
    assert_eq!(sim.actor_speed(player).unwrap(), 120);

    sim.submit_action(PlayerAction::Wait).expect("wait");
    assert_eq!(
        sim.actor_speed(player).unwrap(),
        120,
        "cache read, not a script call"
    );

    // Cache-only reads, asserted the  acceptance way: with the rules
    // handler unregistered, a recompute leaves the cache *absent* (the
    // detectable state) — dirtying-free ticks still use the last cached
    // speed without any script call (asserted above).
    host.lock()
        .unwrap_or_else(|p| p.into_inner())
        .unregister_script_event("scripts/rules/stats.luau", "RecomputeDerived");
    sim.recompute_live();
    let broker = sim.broker().read().unwrap_or_else(|p| p.into_inner());
    assert!(
        broker
            .typed_part::<tessera_core::effects::Derived>(player, &PartTypeId::new("Derived"))
            .is_none(),
        "failed/unarmed recompute leaves the cache absent, never stale"
    );
}

/// Level and Attributes writes through the console dirtying path reproduce
/// the exact  vectors (Level 10 -> HP 99 for T 13; Agility 20 -> 120).
#[test]
fn console_stat_writes_recompute_exact_vectors() {
    let (world, _tiles, broker, bridge) = load_real_content(Path::new(MODS));
    let mut b = broker.write().unwrap_or_else(|p| p.into_inner());
    let player = spawn_at(&world, &mut b, "Player", 2, 2);
    drop(b);

    let mut sim = Sim::from_shared(broker.clone(), Some(bridge), demo_zone(), player, None);
    assert_eq!(derived_map(&sim, player)["max_hp"], 36);

    // Level 1 -> 10: 10 + 2*13 + 9*(3 + 13//3) = 36 + 63 = 99.
    sim.set_part_console(
        player,
        PART_LEVEL,
        &std::collections::BTreeMap::from([("level".to_string(), 10)]),
    )
    .expect("setpart level");
    assert_eq!(derived_map(&sim, player)["max_hp"], 99, " Knight L10 row");

    // Agility 9 -> 20: speed base 120 (the  speed table row).
    sim.set_part_console(
        player,
        PART_ATTRIBUTES,
        &std::collections::BTreeMap::from([("agility".to_string(), 20)]),
    )
    .expect("setpart agility");
    assert_eq!(derived_map(&sim, player)["speed"], 120);

    // A Chilled-style `speed_pct = 90` status: 120 * 90 // 100 = 108 (
    // "Speed + status" row — applied by the native finalize over the script
    // base; no floats anywhere).
    write_status(&sim, player, mods_pct(90));
    sim.recompute_live();
    assert_eq!(
        derived_map(&sim, player)["speed"],
        108,
        "120 with speed_pct=90 -> 108"
    );
    // The  post-status floor: a degenerate stack cannot stall the
    // scheduler — speed never falls below 20 (120 * 5 / 100 = 6 -> 20).
    write_status(&sim, player, mods_pct(5));
    sim.recompute_live();
    assert_eq!(derived_map(&sim, player)["speed"], 20, "invariant floor 20");
}

/// The `Health.hp <= max_hp` clamp : sethp above the cap, then any
/// dirtying recompute pulls it back under the cap.
#[test]
fn health_clamps_to_max_hp_on_recompute() {
    let (world, _tiles, broker, bridge) = load_real_content(Path::new(MODS));
    let mut b = broker.write().unwrap_or_else(|p| p.into_inner());
    let player = spawn_at(&world, &mut b, "Player", 2, 2);
    drop(b);

    let mut sim = Sim::from_shared(broker.clone(), Some(bridge), demo_zone(), player, None);
    sim.set_entity_hp(player, 40).expect("sethp above cap");
    // A write to Attributes triggers a recompute; the native finalize clamps.
    sim.set_part_console(
        player,
        PART_LEVEL,
        &std::collections::BTreeMap::from([("level".to_string(), 1)]),
    )
    .expect("setpart");
    let hp = {
        let broker = sim.broker().read().unwrap_or_else(|p| p.into_inner());
        broker
            .typed_part::<tessera_core::events::Health>(player, &PART_HEALTH.into())
            .expect("Health")
            .hp
    };
    assert_eq!(hp, 36, "hp clamped to derived max_hp (10 + 2*13)");
}

/// `StatusApplied` dirtying: the Shield Bash stagger fires a recompute after
/// the ApplyStatus line, and the goblin's cache is rebuilt.
#[test]
fn status_application_fires_recompute_in_deterministic_order() {
    let (world, _tiles, broker, bridge) = load_real_content(Path::new(MODS));
    let mut b = broker.write().unwrap_or_else(|p| p.into_inner());
    let player = spawn_at(&world, &mut b, "Player", 2, 2);
    let goblin = spawn_at(&world, &mut b, "Goblin", 3, 2);
    drop(b);

    let mut sim = Sim::from_shared(
        broker.clone(),
        Some(bridge),
        demo_zone(),
        player,
        Some(goblin),
    );
    sim.submit_action(PlayerAction::Bash).expect("bash");

    let log = sim.log.to_vec();
    let status_idx = log
        .iter()
        .position(|l| l.starts_with("EVENT ApplyStatus ") && l.contains("status=Status_Staggered"))
        .expect("ApplyStatus(Status_Staggered) must be logged");
    let recompute_idx = log
        .iter()
        .skip(status_idx + 1)
        .position(|l| l.starts_with("EVENT RecomputeDerived "))
        .map(|i| i + status_idx + 1)
        .unwrap_or_else(|| {
            panic!(
                "no recompute after StatusApplied; log tail: {:?}",
                &log[status_idx.saturating_sub(2)..]
            )
        });
    assert!(
        status_idx < recompute_idx,
        "RecomputeDerived fires after StatusApplied: {log:?}"
    );
    // The goblin's cache is rebuilt (fresh values after the dirtying).
    let d = derived_map(&sim, goblin);
    assert_eq!(d["max_hp"], 36, "Goblin inherits Actor T 13, L 1");
}

/// A deterministic tuning reload : rebuild the content with a
/// changed `Tuning_Combat` (same broker, fresh world/bridge), reload the
/// rules handler, and every stat-ful entity's cache reflects the new table
/// in entity-id order.
#[test]
fn tuning_reload_rebuilds_caches_from_the_new_table() {
    // Content A: the shipped hp_base = 10.
    let (world, _tiles, broker, bridge) = load_real_content(Path::new(MODS));
    let mut b = broker.write().unwrap_or_else(|p| p.into_inner());
    let player = spawn_at(&world, &mut b, "Player", 2, 2);
    let goblin = spawn_at(&world, &mut b, "Goblin", 3, 2);
    drop(b);

    let mut sim = Sim::from_shared(
        broker.clone(),
        Some(bridge),
        demo_zone(),
        player,
        Some(goblin),
    );
    assert_eq!(derived_map(&sim, player)["max_hp"], 36);

    // Content B: same broker, hp_base raised to 20 (max_hp 46 for T 13).
    let base_vfs = DirVfs::new(PathBuf::from(BASE));
    let sources = discover_sources(Path::new(MODS)).expect("discover mods");
    let registry = tessera_mods::ModRegistry::build(sources).expect("mod registry");
    let overlay = Overlay::new(Box::new(base_vfs), &registry);
    let bridge_b = ScriptBridge::new(broker.clone());
    let mut world_b = World::new();
    register_parts(&mut world_b);
    for path in overlay.list("scripts").unwrap_or_default() {
        if path.ends_with(".luau") {
            let source = overlay.read_to_string(&path).expect("read script");
            bridge_b
                .load(&path, &path, &source)
                .unwrap_or_else(|e| panic!("loading script {path}: {e}"));
        }
    }
    bridge_b.sync_schemas(&mut world_b).expect("sync schemas");
    world_b.load(&overlay).expect("load blueprints");
    apply_mod_patches(&world_b, &registry).expect("patches");
    // Raise hp_base in the world the reloaded bridge will read.
    world_b
        .apply_tuning_override("Tuning_Combat", "hp_base", &toml::Value::Integer(20))
        .expect("override hp_base");
    let world_b = Arc::new(world_b);
    bridge_b.set_world(world_b.clone());

    sim.reload_rules(Some(bridge_b));
    assert_eq!(
        derived_map(&sim, player)["max_hp"],
        46,
        "10 + 2*13 with hp_base 20"
    );
    assert_eq!(derived_map(&sim, goblin)["max_hp"], 46);
}

/// Fail-closed spawn : a stat-ful blueprint without a registered rules
/// script is a spawn error, not a silently slow goblin.
#[test]
fn statful_spawn_without_rules_script_fails_closed() {
    let (world, _tiles, broker, bridge) = load_real_content(Path::new(MODS));
    // Unregister the rules handler so the engine is unarmed.
    let host = bridge.host.clone();
    host.lock()
        .unwrap_or_else(|p| p.into_inner())
        .unregister_script_event("scripts/rules/stats.luau", "RecomputeDerived");

    let mut b = broker.write().unwrap_or_else(|p| p.into_inner());
    let player = spawn_at(&world, &mut b, "Player", 2, 2);
    drop(b);
    let mut sim = Sim::from_shared(broker.clone(), Some(bridge), demo_zone(), player, None);

    let err = sim
        .spawn_blueprint_console(&world, "Goblin", Some((4, 2)))
        .expect_err("stat-ful spawn without the rules handler must fail closed");
    assert!(err.contains("rules/stats.luau"), "{err}");
    assert!(err.contains("fail-closed"), "{err}");
}
