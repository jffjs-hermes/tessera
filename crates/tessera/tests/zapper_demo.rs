//! Integration smoke test for the script integration + Zapper demo.
//!
//! Loads the real `data/base` pack (the Zapper blueprint + `zapper.luau`),
//! builds the shared broker + script bridge, spawns the player and a Zapper
//! adjacent to it, and drives two Zapper attacks (plus player turns). Asserts:
//!   - the player's `Health` falls by 4 over the two attacks
//!     (2 × [1 native base hit + 1 scripted zap]);
//!   - the Zapper's scripted `Charge` ends at 1 (3 − 2);
//!   - both attacks produce script-handler evidence (the zapper zap), proving
//!     script handlers share the native dispatch path;
//!   - the headless run is deterministic (two identical runs agree).

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use tessera::scripting::ScriptBridge;
use tessera::sim::{PlayerAction, Sim};
use tessera_core::ecs::EntityBroker;
use tessera_core::events::Point;
use tessera_core::vfs::{DirVfs, Vfs};
use tessera_core::world::RenderTile;
use tessera_core::world::World;
use tessera_core::zone::Zone;

const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/base");
const PLAYER_BLUEPRINT: &str = "Player";
const ZAPPER_BLUEPRINT: &str = "Zapper";

/// Build the world exactly as the binary does: register engine parts, load the
/// Zapper script (registering its `Charge` schema), sync schemas into the world
/// registry, then load the blueprints.
fn build_world(vfs: &DirVfs, bridge: &ScriptBridge) -> World {
    let mut world = World::new();
    world
        .register_component::<RenderTile>("Render")
        .expect("register Render");
    world
        .register_part::<tessera_core::events::Position>(tessera_core::events::PART_POSITION)
        .expect("register Position");
    world
        .register_part::<tessera_core::turns::ActorEnergy>(tessera_core::turns::PART_ENERGY)
        .expect("register ActorEnergy");
    world
        .register_part::<tessera_core::events::Health>(tessera_core::events::PART_HEALTH)
        .expect("register Health");
    world
        .register_part::<tessera_core::events::Stats>(tessera_core::events::PART_STATS)
        .expect("register Stats");

    for path in vfs.list("scripts").expect("list scripts") {
        if path.ends_with(".luau") {
            let src = vfs.read_to_string(&path).expect("read script");
            bridge.load(&path, &path, &src).expect("load zapper script");
        }
    }
    bridge.sync_schemas(&mut world).expect("sync schemas");
    world.load(vfs).expect("load blueprints");
    world
}

/// Build the demo sim: player at (2,2), Zapper at (3,2), both scheduled.
fn build_demo() -> Sim {
    let vfs = DirVfs::new(PathBuf::from(DATA));
    let broker: Arc<RwLock<EntityBroker>> = Arc::new(RwLock::new(EntityBroker::new()));
    let bridge = ScriptBridge::new(broker.clone());
    let world = build_world(&vfs, &bridge);
    let world = Arc::new(world);
    bridge.set_world(world.clone());

    let mut b = broker.write().unwrap();
    let player = world
        .spawn_blueprint(PLAYER_BLUEPRINT, &mut b)
        .expect("spawn player");
    b.insert_part(
        player,
        tessera_core::events::PART_POSITION,
        Box::new(tessera_core::events::Position { x: 2, y: 2 }),
    )
    .unwrap();
    let zapper = world
        .spawn_blueprint(ZAPPER_BLUEPRINT, &mut b)
        .expect("spawn zapper");
    b.insert_part(
        zapper,
        tessera_core::events::PART_POSITION,
        Box::new(tessera_core::events::Position { x: 3, y: 2 }),
    )
    .unwrap();
    drop(b);

    let map = demo_zone(40, 20);
    Sim::from_shared(broker, Some(bridge), map, player, Some(zapper))
}

/// The demo room for this integration fixture: bordered all-floor grid with a
/// pillar at `x % 8 == 4 && y % 6 == 3` (the layout the player/Zapper positions
/// below were authored against); player at (2,2), Zapper at (3,2).
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
    Zone::from_cells("test", width, height, Point::new(2, 2), cells)
}

/// Drive the schedule until the Zapper has landed exactly two Attacks on the
/// player (interleaving the player's own turns deterministically).
fn drive_two_zapper_attacks(sim: &mut Sim) {
    let mut attacks = 0;
    let mut guards = 0;
    while attacks < 2 {
        sim.submit_action(PlayerAction::Wait).expect("wait turn");
        guards += 1;
        let count = sim
            .log()
            .iter()
            .filter(|l| l.contains("SCRIPT Attack"))
            .count();
        attacks = attacks.max(count);
        assert!(
            guards < 200,
            "zapper never attacked twice: {} waits, log={:?}",
            guards,
            sim.log()
        );
    }
}

#[test]
fn zapper_demo_health_minus_four_and_charge_one() {
    let mut sim = build_demo();
    let hp_before = sim.player_health();
    let charge_before = sim.enemy_charge();
    assert_eq!(hp_before, 20, "player baseline Health");
    assert_eq!(charge_before, Some(3), "Zapper baseline Charge");

    drive_two_zapper_attacks(&mut sim);

    assert_eq!(
        sim.player_health(),
        16,
        "Health falls by 4 after two attacks"
    );
    assert_eq!(sim.enemy_charge(), Some(1), "Charge ends at 1");
    assert!(
        sim.log().iter().filter(|l| l.contains("zap")).count() >= 2,
        "two zap log entries, got {:?}",
        sim.log()
    );
}

#[test]
fn headless_demo_run_is_deterministic() {
    let run = || {
        let mut sim = build_demo();
        let tiles = tessera_core::tiles::TileRegistry::new();
        let _map = sim.run_headless(&tiles, 8);
        (sim.log().to_vec(), sim.player_health(), sim.enemy_charge())
    };
    let (a_log, a_hp, a_charge) = run();
    let (b_log, b_hp, b_charge) = run();
    assert_eq!(a_log, b_log, "event log deterministic");
    assert_eq!(a_hp, b_hp, "health deterministic");
    assert_eq!(a_charge, b_charge, "charge deterministic");
    assert!(
        a_log.iter().any(|l| l.contains("SCRIPT Attack")),
        "headless run must include script handler evidence, got {a_log:?}"
    );
}

#[test]
fn player_zap_dispatches_attack_toward_last_faced_direction() {
    // The player's scripted `Zap` (E) maps to an `Attack` on the entity in the
    // last faced cell (demo action mapping). Player at (2,2) starts facing
    // east (1,0); the Zapper at (3,2) is straight ahead.
    let mut sim = build_demo();
    let hp0 = sim.enemy_health();
    assert_eq!(hp0, 20, "Zapper baseline Health");

    // Default facing is east (the Zapper is straight ahead), so a plain Zap
    // hits the faced entity.
    sim.submit_action(PlayerAction::Zap)
        .expect("zap toward zapper");
    assert_eq!(
        sim.enemy_health(),
        hp0 - 1,
        "zap lands base damage on the faced entity"
    );

    // Face north (empty cell ahead) and zap: no live target, nothing damaged.
    sim.submit_action(PlayerAction::Move { dx: 0, dy: -1 })
        .expect("move up");
    let before = sim.enemy_health();
    sim.submit_action(PlayerAction::Zap)
        .expect("zap empty cell");
    assert_eq!(
        sim.enemy_health(),
        before,
        "zapping an empty cell hits nothing"
    );
    // The player's attack flows through the shared script+dispatch path, so
    // the event log carries script-handler evidence for it.
    assert!(
        sim.log().iter().any(|l| l.contains("SCRIPT Attack")),
        "player zap produces script-handler evidence, got {:?}",
        sim.log()
    );
}
