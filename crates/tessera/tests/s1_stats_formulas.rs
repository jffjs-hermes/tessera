//! The 4.6 acceptance formula fixtures, headless and seeded — reproduced *exactly*.
//!
//! Drives the real content path (base + campaign mod, `rules/stats.luau`,
//! base `Tuning_Combat`) through the public Sim seams and asserts the
//! acceptance table value-for-value, grounded in BOTH design prototypes
//! (`prototype_formula_sim.py` and `prototype_class_sim.py` —
//! the same `hp_formula`/`clamp`/`K`/`P` numbers the merged design doc pins):
//!
//! - the HP table: 16 values (4 classes × L1/L5/L10/L20 — the design table's
//!   L1/L10/L20 rows plus the L5 row completing the class arcs);
//! - the four accuracy/defense/hit-chance chains (Knight acc 4 → 42%,
//!   Goblin acc 8/def 12 → 37% vs the Knight's def 21, Wizard def 5 → 53%,
//!   Rogue acc 16 → 54%);
//! - the speed table (Agility 8/10/15/20/25 → 96/100/110/120/130) with the
//!   `speed_pct = 90` → 108 status row and the post-status floor 20;
//! - the same seed producing byte-identical `Derived` across two full runs;
//! - the shipped base Goblin reproducing the acceptance-table goblin fixture by
//!   construction (blueprint-authored attributes, not test plumbing).
//!
//! Hit chances are computed through the cached `accuracy`/`defense` values
//! with the base-scope `base_hit`/`hit_min`/`hit_max` keys read from the live
//! `Tuning_Combat` table (the d100 roll itself belongs to the later damage slice).
//!
//! No floats anywhere: every expectation is `i64` from the cached derived
//! values, and the same-seed comparison is byte-level.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use tessera::mods::{Overlay, apply_mod_patches, discover_sources};
use tessera::scripting::ScriptBridge;
use tessera::sim::{PlayerAction, Sim};
use tessera_core::ecs::{EntityBroker, PartTypeId};
use tessera_core::effects::{Modifiers, PART_STATUS_EFFECTS, StatusEffects};
use tessera_core::events::{PART_POSITION, Point, Position};
use tessera_core::kinds::TuningValue;
use tessera_core::stats::{Bonuses, PART_ATTRIBUTES, PART_LEVEL};
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

/// Load the real shipped content exactly as the binary does (base + every
/// discovered mod through the overlay, then per-mod patches), so
/// `rules/stats.luau` and the base `Tuning_Combat` table are live.
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

fn derived_map(sim: &Sim, e: hecs::Entity) -> BTreeMap<String, i64> {
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

/// One integer key from the live `Tuning_Combat` table (authored base value;
/// hit-chance clamp keys the hit% rows consume). Missing key → panic:
/// the fixture must fail loudly, not guess.
fn tuning_int(world: &World, key: &str) -> i64 {
    let tuning = world
        .tuning_values("Tuning_Combat")
        .expect("Tuning_Combat table must be loaded");
    match tuning
        .get(key)
        .unwrap_or_else(|| panic!("Tuning_Combat key '{key}' must exist"))
    {
        TuningValue::Int(v) => *v,
        other => panic!("Tuning_Combat key '{key}' must be an integer, found {other:?}"),
    }
}

/// The headless hit chance:
/// `clamp(base_hit + accuracy − defense, hit_min, hit_max)` — the base-scope
/// `prototype_formula_sim.py` (`clamp(P["base"]+a.acc()-d.defense(),5,95)`)
/// and `prototype_class_sim.py` (`clamp(K["base"]+a.accuracy(..)-d.defense(..),5,95)`)
/// verbatim, with the keys read from the live table rather than hardcoded.
fn hit_chance(world: &World, accuracy: i64, defense: i64) -> i64 {
    let base = tuning_int(world, "base_hit");
    let min = tuning_int(world, "hit_min");
    let max = tuning_int(world, "hit_max");
    (base + accuracy - defense).clamp(min, max)
}

/// Spawn a fresh "Player" fixture, overwrite its Attributes/Level/Bonuses
/// through the public seams, and recompute so the cache settles. Distinct
/// `at` per fixture keeps every entity on its own cell.
fn fixture(
    sim: &mut Sim,
    world: &World,
    at: (i32, i32),
    agility: i64,
    toughness: i64,
    level: i64,
    bonuses: Bonuses,
) -> hecs::Entity {
    let e = sim
        .spawn_blueprint_console(world, "Player", Some(at))
        .expect("spawn fixture");
    sim.set_part_console(
        e,
        PART_ATTRIBUTES,
        &BTreeMap::from([
            ("might".to_string(), 10),
            ("agility".to_string(), agility),
            ("toughness".to_string(), toughness),
            ("will".to_string(), 10),
            ("intellect".to_string(), 10),
        ]),
    )
    .expect("set fixture attributes");
    sim.set_part_console(
        e,
        PART_LEVEL,
        &BTreeMap::from([("level".to_string(), level)]),
    )
    .expect("set fixture level");
    sim.broker()
        .write()
        .unwrap_or_else(|p| p.into_inner())
        .insert_part(
            e,
            tessera_core::stats::PART_BONUSES.to_string(),
            Box::new(bonuses),
        )
        .expect("set fixture bonuses");
    sim.recompute_live();
    e
}

/// The demo sim: player (Actor attributes, T 13/A 9/L 1) + the shipped
/// goblin (the acceptance fixture: T 9/A 13/L 2, `Bonuses.ac = 2`).
fn demo_sim(
    world: &World,
    broker: Arc<RwLock<EntityBroker>>,
    bridge: ScriptBridge,
) -> (Sim, hecs::Entity, hecs::Entity) {
    let mut b = broker.write().unwrap_or_else(|p| p.into_inner());
    let player = spawn_at(world, &mut b, "Player", 2, 2);
    let goblin = spawn_at(world, &mut b, "Goblin", 3, 2);
    drop(b);
    let sim = Sim::from_shared(
        broker.clone(),
        Some(bridge),
        demo_zone(),
        player,
        Some(goblin),
    );
    (sim, player, goblin)
}

// ---------------------------------------------------------------------------
// 4.6 acceptance vectors
// ---------------------------------------------------------------------------

/// The 16 HP values: `10 + 2*T + (L-1)*(3 + T//3)` (floor division —
/// `prototype_formula_sim.py`'s `hp()` and `prototype_class_sim.py`'s
/// `hp_formula()` are the same expression) for the four classes at
/// L1/L5/L10/L20. The three design-table rows (L1/L10/L20) are the pinned ones; the L5
/// row completes each class's arc so all sixteen values in the fixture are
/// asserted, not a subset. All four L1/L10/L20 triples match the Levels table in
/// `Attributes and Levels.md` exactly.
#[test]
fn hp_table_reproduces_all_16_values() {
    let (world, _tiles, broker, bridge) = load_real_content(Path::new(MODS));
    let (mut sim, _player, _goblin) = demo_sim(&world, broker, bridge);

    // (label, toughness) — the class starting Toughness values.
    let classes = [
        ("Knight", 13i64),
        ("Cleric", 12),
        ("Rogue", 9),
        ("Wizard", 10),
    ];
    // Expected max_hp per class at L1/L5/L10/L20 (floor division, i64).
    let expected = [
        [36i64, 64, 99, 169], // Knight T13: 36; +4*7 L5=64; +9*7 L10=99; +19*7 L20=169
        [34, 62, 97, 167],    // Cleric T12: 34; +4*7=62; +9*7=97; +19*7=167
        [28, 52, 82, 142],    // Rogue T9: 28; +4*6=52; +9*6=82; +19*6=142
        [30, 54, 84, 144],    // Wizard T10: 30; +4*6=54; +9*6=84; +19*6=144
    ];
    let levels = [1i64, 5, 10, 20];

    for (ci, (label, toughness)) in classes.iter().enumerate() {
        let e = fixture(
            &mut sim,
            &world,
            (4, 2 + ci as i32),
            10,
            *toughness,
            1,
            Bonuses::default(),
        );
        for (li, level) in levels.iter().enumerate() {
            // Level writes flow through the console dirtying path.
            sim.set_part_console(
                e,
                PART_LEVEL,
                &BTreeMap::from([("level".to_string(), *level)]),
            )
            .expect("set level");
            let max_hp = derived_map(&sim, e)["max_hp"];
            assert_eq!(
                max_hp, expected[ci][li],
                "{label} L{level}: 10 + 2*{toughness} + ({level}-1)*(3 + {toughness}//3) must be {}",
                expected[ci][li]
            );
        }
    }
}

/// The four accuracy/defense/hit-chance chains (the acceptance table rows
/// "Knight accuracy", "Goblin defense", "Hit%", "Goblin accuracy", "Knight
/// defense", "Wizard defense", "Rogue accuracy"). Every row asserts through
/// the *cached* values the pipeline produced — no hand-computed bypass.
#[test]
fn accuracy_defense_hit_chance_chains_reproduce_exactly() {
    let (world, _tiles, broker, bridge) = load_real_content(Path::new(MODS));
    let (mut sim, _player, goblin) = demo_sim(&world, broker, bridge);

    // The shipped goblin (acceptance fixture: L2, A13, Bonuses.ac = 2).
    let g = derived_map(&sim, goblin);
    assert_eq!(g["accuracy"], 8, "goblin accuracy: 2 + 2*(13-10) + 0");
    assert_eq!(g["dodge"], 8, "goblin dodge: 2 + 2*(13-10)");
    assert_eq!(g["defense"], 12, "goblin defense: 8 + min(2*2, 30)");
    assert_eq!(g["max_hp"], 34, "goblin HP: 10 + 2*9 + 1*(3 + 9//3)");

    // Knight: L3, A9, Bonuses.accuracy = 3, Bonuses.ac = 10.
    let knight = fixture(
        &mut sim,
        &world,
        (4, 2),
        9,
        13,
        3,
        Bonuses {
            accuracy: 3,
            ac: 10,
            ..Bonuses::default()
        },
    );
    let k = derived_map(&sim, knight);
    assert_eq!(k["accuracy"], 4, "Knight accuracy: 3 + 2*(9-10) + 3");
    assert_eq!(k["dodge"], 1, "Knight dodge: 3 + 2*(9-10)");
    assert_eq!(k["defense"], 21, "Knight defense: 1 + min(10*2, 30)");

    // Wizard: L3, A11 (AC 0 → defense = dodge).
    let wizard = fixture(&mut sim, &world, (5, 2), 11, 10, 3, Bonuses::default());
    let w = derived_map(&sim, wizard);
    assert_eq!(w["dodge"], 5, "Wizard dodge: 3 + 2*(11-10)");
    assert_eq!(w["defense"], 5, "Wizard defense: 5 + min(0*2, 30)");

    // Rogue: L3, A15, Bonuses.accuracy = 3.
    let rogue = fixture(
        &mut sim,
        &world,
        (6, 2),
        15,
        9,
        3,
        Bonuses {
            accuracy: 3,
            ..Bonuses::default()
        },
    );
    assert_eq!(
        derived_map(&sim, rogue)["accuracy"],
        16,
        "Rogue accuracy: 3 + 2*(15-10) + 3"
    );

    // The four hit% chains, through cached accuracy/defense + live tuning.
    assert_eq!(
        hit_chance(&world, k["accuracy"], g["defense"]),
        42,
        "Knight → goblin"
    );
    assert_eq!(
        hit_chance(&world, g["accuracy"], k["defense"]),
        37,
        "goblin → Knight"
    );
    assert_eq!(
        hit_chance(&world, g["accuracy"], w["defense"]),
        53,
        "goblin → Wizard"
    );
    assert_eq!(
        hit_chance(&world, derived_map(&sim, rogue)["accuracy"], g["defense"]),
        54,
        "Rogue → goblin"
    );
}

/// The speed table: Agility 8/10/15/20/25 → 96/100/110/120/130 (slope 2,
/// clamp 50–140), plus the "Speed + status" row (Agility 20 base 120 with
/// `speed_pct = 90` → 108) and the post-status floor 20 (`max(sp, 20)` from
/// `prototype_class_sim.py`'s `speed()`).
#[test]
fn speed_table_and_status_modified_speed_reproduce_exactly() {
    let (world, _tiles, broker, bridge) = load_real_content(Path::new(MODS));
    let (mut sim, _player, _goblin) = demo_sim(&world, broker, bridge);

    for (agility, expected_speed) in [(8i64, 96i64), (10, 100), (15, 110), (20, 120), (25, 130)] {
        let e = fixture(&mut sim, &world, (4, 2), agility, 10, 1, Bonuses::default());
        assert_eq!(
            derived_map(&sim, e)["speed"],
            expected_speed,
            "100 + 2*({agility} - 10), clamped to [50, 140]"
        );
    }

    // Agility 20 → base 120; a Chilled stack (`speed_pct = 90`) → 108.
    let e = fixture(&mut sim, &world, (4, 2), 20, 10, 1, Bonuses::default());
    assert_eq!(derived_map(&sim, e)["speed"], 120, "base before status");
    write_status(&sim, e, mods_pct(90));
    sim.recompute_live();
    assert_eq!(derived_map(&sim, e)["speed"], 108, "120 * 90 // 100");

    // The prototype's post-status floor: a degenerate stack cannot stall the
    // scheduler — speed never falls below 20.
    write_status(&sim, e, mods_pct(5));
    sim.recompute_live();
    assert_eq!(derived_map(&sim, e)["speed"], 20, "invariant floor 20");
}

/// Same seed ⇒ byte-identical `Derived`: two independent content loads and
/// sim runs (same `SIM_SEED`, same script of mutations) produce the exact
/// same `Derived` payloads, serialized byte-for-byte, for every stat-ful
/// entity — the fixture's determinism guarantee (acceptance #2).
#[test]
fn same_seed_produces_byte_identical_derived() {
    let run = || {
        let (world, _tiles, broker, bridge) = load_real_content(Path::new(MODS));
        let (mut sim, player, _goblin) = demo_sim(&world, broker, bridge);
        // A deterministic mutation script: an attribute write, a status
        // application, an HP overshoot (exercises the clamp), and ordinary
        // turns. Recomputation consumes no RNG; the cache is a pure function
        // of cached inputs.
        sim.set_part_console(
            player,
            PART_ATTRIBUTES,
            &BTreeMap::from([("agility".to_string(), 20)]),
        )
        .expect("setpart agility");
        write_status(&sim, player, mods_pct(90));
        sim.set_entity_hp(player, 40).expect("sethp above the cap");
        sim.recompute_live();
        sim.submit_action(PlayerAction::Wait).expect("wait");

        let broker = sim.broker();
        let b = broker.read().unwrap_or_else(|p| p.into_inner());
        let mut out: Vec<(u64, String)> = b
            .entities_with_part(PART_ATTRIBUTES)
            .into_iter()
            .map(|e| {
                let values = b
                    .typed_part::<tessera_core::effects::Derived>(e, &PartTypeId::new("Derived"))
                    .expect("stat-ful entity must carry Derived")
                    .values
                    .clone();
                (e.to_bits().get(), format!("{values:?}"))
            })
            .collect();
        out.sort_by_key(|(id, _)| *id);
        out
    };

    let run_a = run();
    let run_b = run();
    assert_eq!(
        run_a, run_b,
        "same seed must produce byte-identical Derived for every stat-ful entity"
    );
    assert!(
        !run_a.is_empty(),
        "the determinism comparison must cover live entities"
    );

    // Sanity that the script actually exercised the status pipeline: the
    // player's cached speed reflects the 90% step (120 -> 108) in both runs.
    let player_speed: Vec<&str> = run_a
        .iter()
        .map(|(_, s)| s.as_str())
        .filter(|s| s.contains("\"speed\": 108"))
        .collect();
    assert_eq!(player_speed.len(), 1, "{run_a:#?}");
}

/// The shipped base Goblin reproduces the acceptance-table goblin fixture by
/// construction: its authored `Attributes`/`Level`/`Bonuses` (blueprint
/// override of the Actor root — the per-blueprint override decision) land the exact four chains'
/// numbers. This is the provenance anchor for the fixture entities above:
/// the values come from base data, not test-only plumbing.
#[test]
fn shipped_goblin_blueprint_reproduces_the_acceptance_fixture() {
    let (world, _tiles, broker, bridge) = load_real_content(Path::new(MODS));
    let (sim, _player, goblin) = demo_sim(&world, broker, bridge);
    let d = derived_map(&sim, goblin);
    assert_eq!(
        d,
        BTreeMap::from([
            ("ac".to_string(), 2),
            ("accuracy".to_string(), 8),
            ("defense".to_string(), 12),
            ("dodge".to_string(), 8),
            ("max_hp".to_string(), 34),
            ("speed".to_string(), 106), // 100 + 2*(13 - 10) — Agility, not a flat 100
        ]),
        "the shipped goblin is the acceptance goblin"
    );
}
