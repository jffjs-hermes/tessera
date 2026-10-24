//! Save → load round-trip for the stats parts (the stats design's save treatment).
//!
//! Drives the full save treatment through the public `Sim::save`/`Sim::load`
//! seams over the real content path:
//!
//! - `Attributes`/`Level`/`Bonuses` are whitelisted persistent parts: the
//!   snapshot carries them and a load restores them bit-for-bit;
//! - `Derived` is Rebuilt-class: `snapshot_entities` skips it (the explicit
//!   `NON_PERSISTENT_PARTS` skip-list), the save never serializes it, and the
//!   load path recomputes every restored stat-ful entity so the loaded world
//!   has live derived values equal to the pre-save ones;
//! - `SAVE_FORMAT_VERSION` is 2 and a v1 save (the header patched back to the
//!   pre-stats format) rejects as `UnsupportedVersion { found: 1, supported:
//!   2 }` — the named behavior, not an accident.
//!
//! Applied statuses stay non-persistent in this slice (status persistence is a
//! later slice),
//! so the round-trip fixture derives from persistent parts only — which is
//! exactly what the save must preserve.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use tessera::mods::{Overlay, apply_mod_patches, discover_sources};
use tessera::scripting::ScriptBridge;
use tessera::sim::Sim;
use tessera_core::ecs::{EntityBroker, PartTypeId};
use tessera_core::events::{PART_POSITION, Point, Position};
use tessera_core::save::{
    SAVE_FORMAT_VERSION, SaveError, load_from_bytes, restore_broker, save_to_bytes,
};
use tessera_core::stats::{Bonuses, PART_BONUSES, PART_LEVEL};
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
        .register_part::<tessera_core::events::Health>(tessera_core::events::PART_HEALTH)
        .expect("register Health");
}

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
    Zone::from_cells("arena", 12, 12, Point::new(1, 1), cells)
}

fn derived_map(sim: &Sim, e: hecs::Entity) -> BTreeMap<String, i64> {
    let broker = sim.broker().read().unwrap_or_else(|p| p.into_inner());
    broker
        .typed_part::<tessera_core::effects::Derived>(e, &PartTypeId::new("Derived"))
        .expect("entity must carry a Derived cache")
        .values
        .clone()
}

/// Build the demo sim with a player whose Level/Bonuses are non-trivial (L3,
/// `accuracy = 3` — the Knight-chain inputs from the acceptance table) so the
/// round-trip proves
/// the parts are actually carried, not just present at defaults.
fn round_trip_sim(
    world: &World,
    broker: Arc<RwLock<EntityBroker>>,
    bridge: ScriptBridge,
) -> (Sim, ScriptBridge, hecs::Entity, hecs::Entity) {
    let mut b = broker.write().unwrap_or_else(|p| p.into_inner());
    let player = spawn_at(world, &mut b, "Player", 2, 2);
    let goblin = spawn_at(world, &mut b, "Goblin", 3, 2);
    drop(b);
    let mut sim = Sim::from_shared(
        broker,
        Some(bridge.clone()),
        demo_zone(),
        player,
        Some(goblin),
    );
    // Level 3 (T 13 → HP 50) with `Bonuses.accuracy = 3` (the Knight chain
    // input) — persisted state the load must not lose.
    sim.set_part_console(
        player,
        PART_LEVEL,
        &BTreeMap::from([("level".to_string(), 3)]),
    )
    .expect("player level 3");
    sim.broker()
        .write()
        .unwrap_or_else(|p| p.into_inner())
        .insert_part(
            player,
            PART_BONUSES.to_string(),
            Box::new(Bonuses {
                accuracy: 3,
                ..Bonuses::default()
            }),
        )
        .expect("player bonuses");
    sim.recompute_live();
    (sim, bridge, player, goblin)
}

/// Save → load ⇒ identical `Attributes`/`Level`/`Bonuses`, no `Derived` in
/// the snapshot, and rebuilt derived values equal to the pre-save ones.
#[test]
fn save_load_round_trip_preserves_stat_parts_and_rebuilds_derived() {
    let (world, _tiles, broker, bridge) = load_real_content(Path::new(MODS));
    let hooks = world.save_hooks();
    let (sim, bridge, player, goblin) = round_trip_sim(&world, broker, bridge);

    let pre_player_derived = derived_map(&sim, player);
    let pre_goblin_derived = derived_map(&sim, goblin);
    assert_eq!(
        pre_player_derived["max_hp"], 50,
        "10 + 2*13 + 2*(3 + 13//3)"
    );
    assert_eq!(pre_player_derived["accuracy"], 4, "3 + 2*(9-10) + 3");
    assert_eq!(pre_goblin_derived["defense"], 12, "shipped goblin fixture");

    // Save: the snapshot carries Attributes/Level/Bonuses, never Derived.
    let data = sim.save(&hooks).expect("save");
    for saved in &data.entities {
        let names: Vec<&str> = saved.parts.iter().map(|(n, _)| n.as_str()).collect();
        assert!(
            !names.contains(&"Derived"),
            "Derived must be omitted from the snapshot: {names:?}"
        );
        for required in ["Attributes", "Level"] {
            assert!(
                names.contains(&required),
                "{required} is whitelisted and persistent: {names:?}"
            );
        }
    }
    // The goblin's authored `Bonuses.ac = 2` survives too.
    let goblin_saved = data
        .entities
        .iter()
        .find(|s| {
            s.parts
                .iter()
                .any(|(n, t)| n == "Bonuses" && t.contains("ac = 2"))
        })
        .expect("goblin Bonuses.ac = 2 must be in the save");
    assert!(
        goblin_saved
            .parts
            .iter()
            .any(|(n, t)| n == "Attributes" && t.contains("agility = 13")),
        "goblin Attributes must round-trip"
    );

    // Load: same zone + bridge, restored parts equal, derived rebuilt equal.
    let sim2 = Sim::load(data, demo_zone(), &hooks, Some(bridge)).expect("load");
    let (p2, g2) = (
        sim2.player_entity(),
        sim2.broker()
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .entities()
            .into_iter()
            .find(|e| *e != sim2.player_entity())
            .expect("goblin restored"),
    );
    assert_eq!(
        derived_map(&sim2, p2),
        pre_player_derived,
        "player Derived rebuilt equal"
    );
    assert_eq!(
        derived_map(&sim2, g2),
        pre_goblin_derived,
        "goblin Derived rebuilt equal"
    );
    assert_eq!(
        sim2.actor_speed(g2).unwrap(),
        pre_goblin_derived["speed"] as u32,
        "the scheduler row follows the rebuilt cache"
    );

    // The restored parts themselves (spot-check through the live broker):
    // Attributes/Level/Bonuses exact, Derived absent-then-rebuilt (the load
    // path recomputes; no `Derived` is ever restored from the snapshot).
    let b = sim2.broker().read().unwrap_or_else(|p| p.into_inner());
    let lvl = b
        .typed_part::<tessera_core::stats::Level>(p2, &PartTypeId::new(PART_LEVEL))
        .expect("Level restored");
    assert_eq!(lvl.level, 3);
    let bns = b
        .typed_part::<Bonuses>(p2, &PartTypeId::new(PART_BONUSES))
        .expect("Bonuses restored");
    assert_eq!(bns.accuracy, 3);
    // The campaign's granted ability + full charge pool survive too (the
    // save contract records charges as persistent; without the
    // Abilities/Charges hooks every save of a campaign-loaded demo failed
    // with UnserializablePart).
    let ab = b
        .typed_part::<tessera_core::effects::Abilities>(p2, &PartTypeId::new("Abilities"))
        .expect("Abilities restored");
    assert_eq!(ab.list, vec!["Ability_ShieldBash"]);
    let pool = b
        .typed_part::<tessera_core::effects::Charges>(p2, &PartTypeId::new("Charges"))
        .expect("Charges pool restored");
    assert_eq!(pool.charges, 3, "no bash was spent pre-save; pool at full");
    drop(b);
}

/// A v1 save (pre-stats format) loads as `UnsupportedVersion { found: 1,
/// supported: SAVE_FORMAT_VERSION }` — the named rejection, and
/// `SAVE_FORMAT_VERSION` is the bump the design doc pins.
#[test]
fn v1_saves_are_rejected_as_unsupported_version() {
    let (world, _tiles, broker, bridge) = load_real_content(Path::new(MODS));
    let hooks = world.save_hooks();
    let (sim, _bridge, _player, _goblin) = round_trip_sim(&world, broker, bridge);

    assert_eq!(
        SAVE_FORMAT_VERSION, 3,
        "the format bump is the named constant"
    );
    let data = sim.save(&hooks).expect("save");
    let mut bytes = save_to_bytes(&data).expect("encode");

    // Rewind the format-version field to 1 (the pre-stats format) — the
    // header is `TESS <u32 LE version> <u32 LE length> <zstd frame>`.
    bytes[4..8].copy_from_slice(&1u32.to_le_bytes());
    match load_from_bytes(&bytes) {
        Err(SaveError::UnsupportedVersion { found, supported }) => {
            assert_eq!(found, 1);
            assert_eq!(supported, 2);
        }
        other => panic!("expected UnsupportedVersion for a v1 save, got {other:?}"),
    }
}

/// The save-hook restore path enforces the load-time invariants for the
/// restored parts: a v2 save whose `Attributes` payload was corrupted out of
/// range fails closed on restore (same fail-closed style as every native
/// part hook), and `restore_broker` never produces a `Derived` part.
#[test]
fn restore_enforces_bounds_and_never_produces_derived() {
    let (world, _tiles, broker, bridge) = load_real_content(Path::new(MODS));
    let hooks = world.save_hooks();
    let (sim, _bridge, _player, _goblin) = round_trip_sim(&world, broker, bridge);
    let data = sim.save(&hooks).expect("save");

    // Corrupt the first Attributes snapshot to an out-of-range toughness.
    let mut corrupted = data.clone();
    for saved in &mut corrupted.entities {
        for (name, text) in &mut saved.parts {
            if name == "Attributes" {
                *text = text.replacen("toughness = 13", "toughness = 99", 1);
            }
        }
    }
    match restore_broker(&corrupted, &hooks) {
        Err(SaveError::PartRestore { name, .. }) => assert_eq!(name, "Attributes"),
        Ok(_) => panic!("restore must fail closed on an out-of-range attribute"),
        Err(other) => panic!("expected PartRestore, got {other}"),
    }

    // A clean restore produces no Derived part anywhere (the cache is rebuilt
    // by the sim's load path, never restored).
    let restored = restore_broker(&data, &hooks).expect("clean restore");
    for e in restored.entities() {
        assert!(
            restored.get_part(e, &PartTypeId::new("Derived")).is_none(),
            "restore_broker must never produce a Derived part"
        );
    }
}
