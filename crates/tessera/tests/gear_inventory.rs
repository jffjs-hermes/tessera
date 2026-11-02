//! Equipment and inventory, end to end on the shipped content.
//!
//! Drives the real content path (base + campaign mod, `rules/stats.luau`, the
//! base `Tuning_Combat` table and the `Item_*` blueprints) through the public
//! Sim/console/inspector seams, proving:
//!
//!   1. `give`/`equip`/`unequip` move an item between the pack and a slot and
//!      dispatch `RecomputeDerived` (the gear dirtying event), so the cached
//!      AC/speed settle before the next action;
//!   2. the armor-load speed penalty lands on the speed base (the Knight mail
//!      kit: 33 worn vs a Might-14 threshold of 28 → +5% → 98 → 93), and
//!      gear AC adds to the authored `Bonuses` without double-counting;
//!   3. gear invariants fail closed with named errors (unknown item, not
//!      owned, slot occupied, hands busy, over capacity);
//!   4. the inspector row and the console `stats`/`inventory` surfaces show
//!      the wielded gear and the load;
//!   5. a save/load round-trip preserves `Equipment`/`Inventory` exactly and a
//!      v2 save (pre-gear format) is rejected as `UnsupportedVersion`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use tessera::devtools::console::{self, ConsoleContext, ConsoleOutcome};
use tessera::devtools::inspector::snapshot;
use tessera::mods::{Overlay, apply_mod_patches, discover_sources};
use tessera::scripting::ScriptBridge;
use tessera::sim::Sim;
use tessera_core::ecs::{EntityBroker, PartTypeId};
use tessera_core::events::{PART_HEALTH, PART_POSITION, Point, Position};
use tessera_core::items::{Equipment, Inventory, PART_EQUIPMENT, PART_INVENTORY, worn_weight};
use tessera_core::save::{SAVE_FORMAT_VERSION, load_from_bytes, save_to_bytes};
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

/// Load the real shipped content exactly as the binary does.
fn load_real_content(mods_root: &Path) -> (Arc<World>, Arc<RwLock<EntityBroker>>, ScriptBridge) {
    let sources = discover_sources(mods_root).expect("discover mods");
    let registry = tessera_mods::ModRegistry::build(sources).expect("mod registry");
    let base_vfs = DirVfs::new(PathBuf::from(BASE));
    let overlay = Overlay::new(Box::new(base_vfs), &registry);

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
    (world, broker, bridge)
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

fn tmp_save_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tessera-gear-test-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp save dir");
    dir
}

/// Spawn a Player and wrap the shared broker in a Sim (the demo setup).
fn setup() -> (Arc<World>, Sim) {
    let (world, broker, bridge) = load_real_content(Path::new(MODS));
    let player = {
        let mut b = broker.write().unwrap_or_else(|p| p.into_inner());
        let e = world
            .spawn_blueprint("Player", &mut b)
            .expect("spawn Player");
        b.insert_part(
            e,
            PART_POSITION,
            Box::new(Position::from_point(Point::new(2, 2))),
        )
        .expect("set position");
        e
    };
    // The demo Sim owns its own broker copy; build it over the same shared one
    // so this test can keep the entity handle.
    let sim = Sim::from_shared(broker.clone(), Some(bridge), demo_zone(), player, None);
    (world, sim)
}

fn derived(sim: &Sim, e: hecs::Entity) -> std::collections::BTreeMap<String, i64> {
    let broker = sim.broker().read().unwrap_or_else(|p| p.into_inner());
    let names: Vec<String> = broker
        .part_map(e)
        .map(|m| m.names().map(|n| n.to_string()).collect())
        .unwrap_or_default();
    broker
        .typed_part::<tessera_core::effects::Derived>(e, &PartTypeId::new("Derived"))
        .unwrap_or_else(|| {
            panic!(
                "entity carries a Derived cache; parts={names:?} log={:?}",
                sim.log()
            )
        })
        .values
        .clone()
}

fn equipment(sim: &Sim, e: hecs::Entity) -> Equipment {
    let broker = sim.broker().read().unwrap_or_else(|p| p.into_inner());
    broker
        .typed_part::<Equipment>(e, &PartTypeId::new(PART_EQUIPMENT))
        .cloned()
        .expect("entity carries Equipment")
}

fn inventory(sim: &Sim, e: hecs::Entity) -> Inventory {
    let broker = sim.broker().read().unwrap_or_else(|p| p.into_inner());
    broker
        .typed_part::<Inventory>(e, &PartTypeId::new(PART_INVENTORY))
        .cloned()
        .expect("entity carries Inventory")
}

fn ctx<'a>(
    world: &'a World,
    hooks: &'a tessera_core::save::SaveHooks,
    save_dir: &'a Path,
) -> ConsoleContext<'a> {
    ConsoleContext {
        world,
        save_hooks: hooks,
        save_dir,
    }
}

#[test]
fn give_equip_unequip_drive_the_cache_and_the_armor_load_penalty() {
    let (world, mut sim) = setup();
    let hooks = world.save_hooks();
    let save_dir = tmp_save_dir("equip");
    let c = ctx(&world, &hooks, &save_dir);
    let player = sim.player_entity();

    // A bare actor: speed 98 (Agility 9), no gear AC, no worn weight.
    let before = derived(&sim, player);
    assert_eq!(before["speed"], 98, "unarmored speed base");
    assert_eq!(before["ac"], 0);

    // The Knight mail kit through the console: give then equip each piece.
    for item in [
        "Item_Longsword",
        "Item_RoundShield",
        "Item_MailHauberk",
        "Item_HalfHelm",
        "Item_MailSleeves",
        "Item_MailBoots",
    ] {
        let got = console::execute(
            &mut sim,
            &c,
            console::parse(&format!("give {item}")).unwrap(),
        );
        assert!(matches!(got, ConsoleOutcome::Ok(_)), "give {item}: {got:?}");
        let equipped = console::execute(
            &mut sim,
            &c,
            console::parse(&format!("equip {item}")).unwrap(),
        );
        assert!(
            matches!(equipped, ConsoleOutcome::Ok(_)),
            "equip {item}: {equipped:?}"
        );
    }

    let eq = equipment(&sim, player);
    assert_eq!(eq.main_hand, "Item_Longsword");
    assert_eq!(eq.off_hand, "Item_RoundShield");
    assert_eq!(eq.body, "Item_MailHauberk");
    let inv = inventory(&sim, player);
    assert!(inv.is_empty(), "every given item was equipped: {inv:?}");

    // Gear AC 12 (2 shield + 4 hauberk + 2 helm + 2 sleeves + 2 boots) on top
    // of the authored Bonuses.ac (0 for the base Player) — no double count.
    assert_eq!(
        worn_weight(&eq, &world),
        33,
        "the armor-load fixture: 33 worn"
    );
    let after = derived(&sim, player);
    assert_eq!(
        after["ac"], 12,
        "gear AC sums into the cached ac: {after:?}"
    );
    // 98 * (100 - 10) // 100 = 88 (threshold 2 × Might 14 = 28, over 5).
    assert_eq!(
        after["speed"], 88,
        "the armor-load penalty lands on the speed base: {after:?}"
    );

    // Unequip drops the shield: AC 10, worn 27 → no penalty → speed back to 98.
    let out = console::execute(&mut sim, &c, console::parse("unequip off_hand").unwrap());
    assert!(matches!(out, ConsoleOutcome::Ok(_)), "{out:?}");
    let after = derived(&sim, player);
    assert_eq!(after["ac"], 10);
    assert_eq!(after["speed"], 98, "under threshold: no penalty: {after:?}");
    assert_eq!(inventory(&sim, player).items, vec!["Item_RoundShield"]);

    // The console `inventory` read-out shows the pack and the load line.
    let listing = console::execute(&mut sim, &c, console::parse("inventory").unwrap());
    let ConsoleOutcome::Ok(text) = listing else {
        panic!("inventory: {listing:?}");
    };
    assert!(text.contains("wield: Longsword"), "{text}");
    assert!(text.contains("pack: Item_RoundShield"), "{text}");

    // Deleting the last slot empties the equipment part but keeps the pack.
    console::execute(&mut sim, &c, console::parse("unequip main_hand").unwrap());
    let eq = equipment(&sim, player);
    assert_eq!(eq.main_hand, "");
    assert_eq!(
        inventory(&sim, player).items,
        vec!["Item_RoundShield", "Item_Longsword"]
    );

    std::fs::remove_dir_all(&save_dir).ok();
}

#[test]
fn gear_invariants_fail_closed_with_named_errors() {
    let (world, mut sim) = setup();
    let hooks = world.save_hooks();
    let save_dir = tmp_save_dir("errors");
    let c = ctx(&world, &hooks, &save_dir);

    let err = |sim: &mut Sim, line: &str| -> String {
        match console::execute(sim, &c, console::parse(line).unwrap()) {
            ConsoleOutcome::Err(e) => e,
            other => panic!("{line}: expected an error, got {other:?}"),
        }
    };

    // Unknown blueprint id.
    assert!(err(&mut sim, "give Item_Nonexistent").contains("unknown item"));
    // Not owned.
    assert!(err(&mut sim, "equip Item_Longsword").contains("not in the inventory"));
    // Unknown slot name for the item's authored slot.
    console::execute(
        &mut sim,
        &c,
        console::parse("give Item_MailHauberk").unwrap(),
    );
    assert!(
        err(&mut sim, "equip Item_MailHauberk head").contains("belongs in slot 'body'"),
        "wrong slot is named"
    );
    // Slot occupied: two longswords, one slot.
    console::execute(&mut sim, &c, console::parse("give Item_Longsword").unwrap());
    console::execute(
        &mut sim,
        &c,
        console::parse("equip Item_Longsword").unwrap(),
    );
    console::execute(&mut sim, &c, console::parse("give Item_Longsword").unwrap());
    assert!(err(&mut sim, "equip Item_Longsword").contains("is occupied"));
    // Hands busy: a two-handed main hand leaves no off hand.
    console::execute(&mut sim, &c, console::parse("unequip main_hand").unwrap());
    console::execute(
        &mut sim,
        &c,
        console::parse("give Item_RoundShield").unwrap(),
    );
    console::execute(
        &mut sim,
        &c,
        console::parse("equip Item_RoundShield").unwrap(),
    );
    console::execute(
        &mut sim,
        &c,
        console::parse("give Item_Greatsword").unwrap(),
    );
    assert!(err(&mut sim, "equip Item_Greatsword").contains("hands busy"));
    // Over capacity: Might 14 → capacity 140; keep giving 8-weight greatswords
    // until the pack refuses, and prove the refusal is the capacity rule
    // (one more item would not fit) rather than an arbitrary cut-off.
    let mut given = 0;
    let refusal = loop {
        match console::execute(
            &mut sim,
            &c,
            console::parse("give Item_Greatsword").unwrap(),
        ) {
            ConsoleOutcome::Ok(_) => given += 1,
            ConsoleOutcome::Err(e) => break e,
            other => panic!("unexpected give outcome: {other:?}"),
        }
        assert!(given < 40, "the pack never refused an over-capacity give");
    };
    assert!(refusal.contains("over capacity"), "{refusal}");
    let nums: Vec<i64> = refusal
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect();
    let (load, capacity) = match nums.as_slice() {
        [.., load, capacity] => (*load, *capacity),
        _ => panic!("unparseable capacity error: {refusal}"),
    };
    assert_eq!(capacity, 140, "10 × Might 14: {refusal}");
    assert!(
        load > capacity && load <= capacity + 8,
        "refused exactly when one more item would not fit: {refusal}"
    );
    assert!(given > 0, "the loop must have accepted items first");

    std::fs::remove_dir_all(&save_dir).ok();
}

#[test]
fn inspector_row_and_stats_show_the_wielded_gear() {
    let (world, mut sim) = setup();
    let hooks = world.save_hooks();
    let save_dir = tmp_save_dir("inspector");
    let c = ctx(&world, &hooks, &save_dir);
    let player = sim.player_entity();

    for item in ["Item_Longsword", "Item_RoundShield", "Item_MailHauberk"] {
        console::execute(
            &mut sim,
            &c,
            console::parse(&format!("give {item}")).unwrap(),
        );
        console::execute(
            &mut sim,
            &c,
            console::parse(&format!("equip {item}")).unwrap(),
        );
    }

    let snap = snapshot(&sim, &world, &hooks);
    let row = snap
        .entities
        .iter()
        .find(|r| r.id == player.to_bits().get())
        .expect("player row");
    assert!(row.summary.contains("wield: Longsword"), "{}", row.summary);
    assert!(row.summary.contains("off: Round shield"), "{}", row.summary);
    assert!(row.summary.contains("load 20/28 (+0%)"), "{}", row.summary);

    let text = match console::execute(&mut sim, &c, console::parse("stats player").unwrap()) {
        ConsoleOutcome::Ok(t) => t,
        other => panic!("stats: {other:?}"),
    };
    assert!(text.contains("gear: wield: Longsword"), "{text}");
    assert!(text.contains("load 20/28"), "{text}");
    assert!(
        text.contains("ac = 6"),
        "gear AC 2+4 on top of Bonuses 0: {text}"
    );

    std::fs::remove_dir_all(&save_dir).ok();
}

#[test]
fn save_round_trip_preserves_gear_and_rejects_v2() {
    let (world, mut sim) = setup();
    let hooks = world.save_hooks();
    let save_dir = tmp_save_dir("save");
    let c = ctx(&world, &hooks, &save_dir);
    let player = sim.player_entity();

    for item in [
        "Item_MailHauberk",
        "Item_Longsword",
        "Item_HalfHelm",
        "Item_MailBoots",
    ] {
        console::execute(
            &mut sim,
            &c,
            console::parse(&format!("give {item}")).unwrap(),
        );
    }
    console::execute(
        &mut sim,
        &c,
        console::parse("equip Item_MailHauberk").unwrap(),
    );
    console::execute(
        &mut sim,
        &c,
        console::parse("equip Item_Longsword").unwrap(),
    );
    console::execute(&mut sim, &c, console::parse("equip Item_HalfHelm").unwrap());

    let saved_equipment = equipment(&sim, player);
    let saved_inventory = inventory(&sim, player);
    assert_eq!(saved_inventory.items, vec!["Item_MailBoots"]);

    assert_eq!(SAVE_FORMAT_VERSION, 3, "the gear bump");
    let data = sim.save(&hooks).expect("save");
    let bytes = save_to_bytes(&data).expect("encode");

    // The snapshot carries both gear parts as whitelisted state.
    let saved = load_from_bytes(&bytes).expect("decode");
    let player_bits = player.to_bits().get();
    let parts = saved
        .entities
        .iter()
        .find(|e| e.bits == player_bits)
        .expect("player in the snapshot")
        .parts
        .clone();
    assert!(parts.iter().any(|(n, _)| n == PART_EQUIPMENT), "{parts:?}");
    assert!(parts.iter().any(|(n, _)| n == PART_INVENTORY), "{parts:?}");

    sim.restore_from_save(saved, &hooks).expect("restore");
    let player = sim.player_entity();
    assert_eq!(equipment(&sim, player), saved_equipment);
    assert_eq!(inventory(&sim, player), saved_inventory);
    // Worn armor only: body 4 + head 2 (no shield, no off-hand).
    assert_eq!(derived(&sim, player)["ac"], 6, "AC 4 + 2 re-derived");

    // A v2 save (pre-gear format) is rejected, not guessed at.
    let mut v2 = bytes.clone();
    v2[4..8].copy_from_slice(&2u32.to_le_bytes());
    match load_from_bytes(&v2) {
        Err(tessera_core::save::SaveError::UnsupportedVersion { found, supported }) => {
            assert_eq!((found, supported), (2, 3));
        }
        other => panic!("expected UnsupportedVersion for a v2 save, got {other:?}"),
    }

    std::fs::remove_dir_all(&save_dir).ok();
}
