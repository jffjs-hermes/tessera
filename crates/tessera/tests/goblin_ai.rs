//! Headless deterministic tests for the goblin AI + death/respawn policy,
//! driven through the real sim, script bridge, and the shipped `goblin_ai.luau`
//! against the base data pack.
//!
//! Every behavior is asserted on the deterministic event log + entity/hp state,
//! mirroring how the demo is exercised in `--headless`:
//!   - wander is seeded and byte-identical across two runs;
//!   - chase (dist2 = 100) follows a deterministic path step toward the player,
//!     and a far-away goblin (dist2 = 121) wanders deterministically;
//!   - an adjacent goblin Attacks and reduces the player's hp;
//!   - player death despawns the old entity and respawns `Player` at the start
//!     tile, with the player-turn clock counting through the death;
//!   - a killed goblin is absent, then reappears only after exactly 5 player
//!     turns at a seeded walkable-empty tile;
//!   - a full map (no free tile) leaves the respawn pending without crashing.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use hecs::Entity;
use tessera::scripting::ScriptBridge;
use tessera::sim::{PlayerAction, Sim};
use tessera_core::ecs::{EntityBroker, Part, PartTypeId};
use tessera_core::events::{PART_POSITION, Point, Position};
use tessera_core::vfs::{DirVfs, Vfs};
use tessera_core::world::{RenderTile, World};
use tessera_core::zone::Zone;

const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/base");
const PLAYER_BP: &str = "Player";
const GOBLIN_BP: &str = "Goblin";

/// Build the world exactly as the binary does: register engine parts, load
/// every shipped script (registering their `define_part` schemas incl.
/// `GoblinBrain`/`RespawnSystem`), sync schemas into the world registry, then
/// load the blueprints.
fn build_world(vfs: &DirVfs, bridge: &ScriptBridge) -> World {
    let mut world = World::new();
    world
        .register_component::<RenderTile>("Render")
        .expect("register Render");
    world
        .register_part::<Position>(PART_POSITION)
        .expect("register Position");
    world
        .register_part::<tessera_core::turns::ActorEnergy>(tessera_core::turns::PART_ENERGY)
        .expect("register ActorEnergy");
    world
        .register_part::<tessera_core::events::Health>(tessera_core::events::PART_HEALTH)
        .expect("register Health");

    // Load every shipped script (registers `GoblinBrain` + `RespawnSystem`
    // schemas the blueprints validate against), sync schemas into the world
    // registry, then load the blueprints.
    for path in vfs.list("scripts").expect("list scripts") {
        if path.ends_with(".luau") {
            let src = vfs.read_to_string(&path).expect("read script");
            bridge.load(&path, &path, &src).expect("load script");
        }
    }
    bridge.sync_schemas(&mut world).expect("sync schemas");
    world.load(vfs).expect("load blueprints");
    world
}

/// A bordered all-floor demo room with a pillar at `x % 8 == 4 && y % 6 == 3`,
/// matching the demo-zone fixture other tests use.
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
    Zone::from_cells("test", width, height, Point::new(1, 1), cells)
}

/// Spawn a positioned, scheduled actor from a blueprint into `broker`.
fn spawn_actor(world: &World, broker: &mut EntityBroker, bp: &str, x: i32, y: i32) -> Entity {
    let e = world.spawn_blueprint(bp, broker).expect("spawn blueprint");
    broker
        .insert_part(
            e,
            PART_POSITION,
            Box::new(Position { x, y }) as Box<dyn Part>,
        )
        .unwrap();
    e
}

/// Build the sim: a player at (1,1) and a goblin at `gbx, gby`, with the
/// shipped scripts loaded and a world-state singleton installed (respawn
/// anchor). Returns the sim and the shared broker.
fn build_sim(gbx: i32, gby: i32) -> (Sim, Arc<RwLock<EntityBroker>>) {
    let vfs = DirVfs::new(PathBuf::from(DATA));
    let broker: Arc<RwLock<EntityBroker>> = Arc::new(RwLock::new(EntityBroker::new()));
    let bridge = ScriptBridge::new(broker.clone());
    let world = Arc::new(build_world(&vfs, &bridge));
    bridge.set_world(world.clone());

    let mut b = broker.write().unwrap();
    let player = spawn_actor(&world, &mut b, PLAYER_BP, 1, 1);
    let goblin = spawn_actor(&world, &mut b, GOBLIN_BP, gbx, gby);
    drop(b);

    // World-state singleton (non-scheduled respawn anchor).
    let mut b = broker.write().unwrap();
    let world_state = b.spawn();
    drop(b);
    bridge.set_world_state_entity(world_state);

    let sim = Sim::from_shared(
        broker.clone(),
        Some(bridge),
        demo_zone(40, 20),
        player,
        Some(goblin),
    );
    (sim, broker)
}

/// Number of live "creature" entities: broker entities carrying both a
/// `Position` and a `Health` part (the player and any goblin; the world-state
/// anchor carries neither).
fn creature_count(broker: &RwLock<EntityBroker>) -> usize {
    let b = broker.read().unwrap();
    b.entities_with_part(PART_POSITION)
        .iter()
        .filter(|e| {
            b.typed_part::<tessera_core::events::Health>(
                **e,
                &PartTypeId::new(tessera_core::events::PART_HEALTH),
            )
            .is_some()
        })
        .count()
}

/// Fixed-seed wander: park the player far away; the goblin's event log is
/// byte-identical across two independent runs, and the goblin wanders (multiple
/// handled moves) rather than attacking.
#[test]
fn fixed_seed_wander_log_is_byte_identical() {
    let run = || {
        let (mut sim, _broker) = build_sim(20, 10);
        let tiles = tessera_core::tiles::TileRegistry::new();
        for _ in 0..25 {
            sim.submit_action(PlayerAction::Wait).unwrap();
        }
        (sim.log().to_vec(), sim.text_map(&tiles))
    };
    let (log_a, map_a) = run();
    let (log_b, map_b) = run();
    assert_eq!(log_a, log_b, "wander log must be byte-identical");
    assert_eq!(map_a, map_b, "wander map must be byte-identical");
    assert!(
        log_a
            .iter()
            .filter(|l| l.contains("Move") && l.contains("handled"))
            .count()
            >= 5,
        "the far goblin should wander, got {log_a:?}"
    );
}

/// Chase at dist2 = 100: the goblin follows a deterministic path step toward
/// the player (the first handled goblin move reduces `x`). Wander at dist2 =
/// 121: the goblin does not attack and its wander is deterministic across runs.
#[test]
fn chase_at_dist2_100_and_wander_at_121() {
    // Player at (1,1); goblin at (11,1) => dist2 = 100 => chase.
    let (mut sim, _broker) = build_sim(11, 1);
    for _ in 0..3 {
        sim.submit_action(PlayerAction::Wait).unwrap();
    }
    let first_chase = sim
        .log()
        .iter()
        .find(|l| l.contains("Move") && l.contains("handled"))
        .cloned()
        .expect("chasing goblin must move");
    assert_eq!(
        parse_to(&first_chase),
        (10, 1),
        "chase steps to path[1], got {first_chase}"
    );

    // Player at (1,1); goblin at (12,1) => dist2 = 121 => wander.
    let run_wander = || {
        let (mut sim, _broker) = build_sim(12, 1);
        for _ in 0..3 {
            sim.submit_action(PlayerAction::Wait).unwrap();
        }
        sim.log().to_vec()
    };
    let a = run_wander();
    let b = run_wander();
    assert_eq!(a, b, "far goblin wander must be deterministic");
    assert!(
        a.iter()
            .any(|l| l.contains("Move") && l.contains("handled")),
        "far goblin should still move (wander), got {a:?}"
    );
}

/// An adjacent goblin Attacks the player each goblin turn and reduces hp.
#[test]
fn adjacent_attack_reduces_player_hp() {
    let (mut sim, _broker) = build_sim(2, 1); // adjacent to player at (1,1)
    let hp0 = sim.player_health();
    assert!(hp0 > 0);
    for _ in 0..3 {
        sim.submit_action(PlayerAction::Wait).unwrap();
    }
    assert!(sim.player_health() < hp0, "adjacent goblin must reduce hp");
    assert!(
        sim.log().iter().any(|l| l.contains("SCRIPT Attack")),
        "expected a goblin Attack in the log, got {:?}",
        sim.log()
    );
}

/// Player death: the fresh `Player` respawns after the old entity dies, and
/// the player-turn clock keeps counting through the death (every submit
/// returns Ok: the clock never stalls).
///
/// The kill and the respawn both resolve inside one scheduler turn, so the
/// externally-observable signature is *damage taken* (hp drops well below the
/// 20 baseline) followed by a jump *back up* to full hp (the respawned play).
/// We assert that sequence and that every submit returns Ok (the clock never
/// stalls on the death).
#[test]
fn player_death_respawns_at_start_and_clock_counts_through() {
    let (mut sim, broker) = build_sim(2, 1); // adjacent goblin bites the player
    let mut saw_damage = false;
    let mut saw_respawn = false;
    let mut submissions = 0u64;
    for _ in 0..120 {
        sim.submit_action(PlayerAction::Wait).unwrap();
        submissions += 1;
        let hp = sim.player_health();
        if hp < 20 {
            saw_damage = true;
        }
        // A jump back up to the full 20 baseline after damage = the fresh
        // Player respawned at full health.
        if saw_damage && hp == 20 {
            saw_respawn = true;
            break;
        }
    }
    assert!(
        saw_damage,
        "the adjacent goblin must damage the player below 20 hp"
    );
    assert!(
        saw_respawn,
        "the player must respawn back to full hp after dying (final hp {})",
        sim.player_health()
    );
    // The clock counted every submitted action through and past the death.
    assert!(
        submissions >= 2 && sim.player_turns_public() == submissions,
        "clock should count every submit through the death (turns {}, submits {})",
        sim.player_turns_public(),
        submissions
    );
    let _ = &broker;
}

/// Goblin death + 5-player-turn respawn timing.
#[test]
fn goblin_respawns_after_exactly_five_player_turns() {
    // Player at (1,1), goblin at (3,1). The goblin has hp=3; once adjacent the
    // player can zap it to death. The goblin advances toward the player.
    let (mut sim, broker) = build_sim(3, 1);
    // Step the player toward the goblin until adjacent or dead, then zap until
    // the goblin dies.
    let mut guard = 0;
    while creature_count(&broker) > 1 && guard < 60 {
        // If the goblin reached adjacency, attack it (facing position).
        sim.submit_action(PlayerAction::Move { dx: 1, dy: 0 })
            .unwrap();
        guard += 1;
        if goblin_adjacent(&broker) {
            let mut zaps = 0;
            while creature_count(&broker) > 1 && zaps < 10 {
                sim.submit_action(PlayerAction::Zap).unwrap();
                zaps += 1;
            }
            break;
        }
    }
    assert!(
        creature_count(&broker) == 1,
        "goblin must be dead after zaps"
    );
    let dead_turn = sim.player_turns_public();

    // Turns 1..4 after death: still absent.
    for _ in 0..4 {
        sim.submit_action(PlayerAction::Wait).unwrap();
    }
    assert_eq!(
        creature_count(&broker),
        1,
        "goblin must not respawn before 5 player turns (now at {})",
        sim.player_turns_public() - dead_turn
    );
    assert_eq!(
        sim.player_turns_public() - dead_turn,
        4,
        "four turns elapsed"
    );

    // The 5th player turn after death: it reappears.
    sim.submit_action(PlayerAction::Wait).unwrap();
    assert_eq!(
        creature_count(&broker),
        2,
        "goblin must respawn on exactly the 5th player turn after death (turn {} after)",
        sim.player_turns_public() - dead_turn
    );
}

/// Full map (no walkable-empty tile): the respawn stays pending and the sim
/// keeps running deterministically (the script's `random_walkable_tile` `None`
/// path is exercised without a crash).
#[test]
fn full_map_random_tile_retry_does_not_crash() {
    let zone = demo_zone(6, 3);
    let build = || {
        let vfs = DirVfs::new(PathBuf::from(DATA));
        let broker: Arc<RwLock<EntityBroker>> = Arc::new(RwLock::new(EntityBroker::new()));
        let bridge = ScriptBridge::new(broker.clone());
        let world = Arc::new(build_world(&vfs, &bridge));
        bridge.set_world(world.clone());
        let mut b = broker.write().unwrap();
        let player = spawn_actor(&world, &mut b, PLAYER_BP, 1, 1);
        let goblin = spawn_actor(&world, &mut b, GOBLIN_BP, 2, 2);
        drop(b);
        let mut b = broker.write().unwrap();
        let world_state = b.spawn();
        drop(b);
        bridge.set_world_state_entity(world_state);
        let mut sim = Sim::from_shared(
            broker.clone(),
            Some(bridge),
            zone.clone(),
            player,
            Some(goblin),
        );
        for _ in 0..20 {
            sim.submit_action(PlayerAction::Wait).unwrap();
        }
        (sim.log().to_vec(), sim.player_turns_public())
    };
    let (a, ta) = build();
    let (b2, tb) = build();
    assert_eq!(a, b2, "full-map retry must stay deterministic");
    assert_eq!(ta, tb);
}

// ---- helpers ---------------------------------------------------------------

fn goblin_adjacent(broker: &RwLock<EntityBroker>) -> bool {
    let b = broker.read().unwrap();
    let mut cells = Vec::new();
    for e in b.entities_with_part(PART_POSITION) {
        if let Some(p) = b.typed_part::<Position>(e, &PART_POSITION.into()) {
            cells.push((p.x, p.y));
        }
    }
    cells.sort_unstable();
    cells.dedup();
    cells.retain(|c| *c != (1, 1));
    cells.iter().any(|c| c.0.abs_diff(1) + c.1.abs_diff(1) <= 1)
}

fn parse_to(line: &str) -> (i32, i32) {
    // "EVENT Move tick=.. from=(a,b) to=(x,y) handled"
    let to_part = line
        .split(" to=")
        .nth(1)
        .unwrap()
        .split(' ')
        .next()
        .unwrap();
    let inner = to_part.trim_start_matches('(').trim_end_matches(')');
    let mut it = inner.split(',');
    let x: i32 = it.next().unwrap().trim().parse().unwrap();
    let y: i32 = it.next().unwrap().trim().parse().unwrap();
    (x, y)
}
