//! Headless deterministic tests for the Caves-of-Qud bump combat rule: moving
//! into a live enemy attacks it instead of overlapping, and no two actors ever
//! share a tile. Driven through the real sim + script bridge against the
//! shipped `data/base` pack.
//!
//! Covers:
//!   - a player bump on an adjacent goblin lands the base hit (Health - 1) and
//!     does NOT move the player into the goblin's tile;
//!   - a seeded run of wander and chase turns never leaves two live creatures
//!     on the same tile, asserted over every tick, and is byte-identical across
//!     two runs.

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

/// Build the world as the binary does: register engine parts, load every
/// shipped script (registering `GoblinBrain`/`RespawnSystem` schemas), sync
/// schemas into the world registry, then load the blueprints.
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
    world
        .register_part::<tessera_core::events::Stats>(tessera_core::events::PART_STATS)
        .expect("register Stats");

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

/// A bordered all-floor demo room with a pillar at `x % 8 == 4 && y % 6 == 3`.
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

/// Spawn a positioned, scheduled actor from a blueprint.
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

/// Build the sim: a player at (1,1) and a goblin at `gbx, gby`, with every
/// shipped script loaded and a world-state singleton installed. Returns the
/// sim and the shared broker.
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

/// The live enemy entity with a `Goblin` marker (any creature whose
/// `RenderTile` is the goblin glyph), if one is currently alive.
fn live_goblin(broker: &RwLock<EntityBroker>) -> Option<Entity> {
    let b = broker.read().unwrap();
    b.entities_with_part(PART_POSITION)
        .iter()
        .copied()
        .find(|e| {
            b.world()
                .get::<&RenderTile>(*e)
                .map(|t| t.tile == "creature.goblin")
                .unwrap_or(false)
        })
}

/// The player's grid position, read from the broker's live `Position` parts
/// (the only positioned entity whose tile is not the goblin's).
fn player_pos(broker: &RwLock<EntityBroker>) -> (i32, i32) {
    let b = broker.read().unwrap();
    for e in b.entities_with_part(PART_POSITION) {
        if let Some(p) = b.typed_part::<Position>(e, &PART_POSITION.into()) {
            let is_goblin = b
                .world()
                .get::<&RenderTile>(e)
                .map(|t| t.tile == "creature.goblin")
                .unwrap_or(false);
            if !is_goblin {
                return (p.x, p.y);
            }
        }
    }
    unreachable!("player always positioned")
}

/// Every live creature's grid position (entities carrying both a `Position`
/// and a `Health` part).
fn creature_positions(broker: &RwLock<EntityBroker>) -> Vec<(i32, i32)> {
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
        .filter_map(|e| b.typed_part::<Position>(*e, &PART_POSITION.into()))
        .map(|p| (p.x, p.y))
        .collect()
}

/// The goblin's current `Health.hp`, if it is alive.
fn goblin_hp(broker: &RwLock<EntityBroker>) -> Option<i32> {
    let g = live_goblin(broker)?;
    let b = broker.read().unwrap();
    b.typed_part::<tessera_core::events::Health>(
        g,
        &PartTypeId::new(tessera_core::events::PART_HEALTH),
    )
    .map(|h| h.hp)
}

/// A player bump into an adjacent goblin deals the base hit (Health - 1) and
/// does NOT move the player onto the goblin's tile (no overlap).
#[test]
fn player_bump_deals_base_hit_without_overlapping() {
    let (mut sim, broker) = build_sim(2, 1); // goblin adjacent right of player
    let hp0 = goblin_hp(&broker).expect("goblin alive");
    assert_eq!(hp0, 3, "goblin baseline Health");

    let p_before = player_pos(&broker);
    // Moving east hits the goblin: an Attack (+1 base, no move).
    sim.submit_action(PlayerAction::Move { dx: 1, dy: 0 })
        .expect("bump turn");
    assert_eq!(
        goblin_hp(&broker),
        Some(hp0 - 1),
        "the bump must land the base hit"
    );
    assert_eq!(
        player_pos(&broker),
        p_before,
        "the bump must NOT move the player into the goblin's tile"
    );

    // The event log records the Attack (base hit) but no Move onto the tile.
    let log = sim.log().to_vec();
    assert!(
        log.iter().any(|l| l.contains("EVENT Attack")),
        "expected an Attack, got {log:?}"
    );
    assert!(
        !log.iter()
            .any(|l| l.contains("Move") && l.contains("handled")),
        "the bump must not be logged as a handled Move, got {log:?}"
    );
}

/// A seeded run of wander and chase turns never leaves two live creatures on
/// the same tile, asserted over every tick, and the run is byte-identical
/// across two identical seeds.
#[test]
fn never_share_a_tile_over_seeded_wander_and_chase() {
    let run = || {
        // Goblin far enough (dist2 = 121) to wander at first, then chase as the
        // player approaches.
        let (mut sim, broker) = build_sim(12, 1);
        let mut trace: Vec<Vec<(i32, i32)>> = Vec::new();
        let mut goblin_alive_hits = 0usize;
        for i in 0..120 {
            // Deterministic driver: walk toward the goblin (east) most turns,
            // mixing in waits so the goblin both wanders and chases.
            let action = if i % 3 == 2 {
                PlayerAction::Wait
            } else {
                PlayerAction::Move { dx: 1, dy: 0 }
            };
            sim.submit_action(action).expect("submit turn");
            let mut cells = creature_positions(&broker);
            cells.sort_unstable();
            // No two live creatures share a tile at any observed state.
            for pair in cells.windows(2) {
                assert_ne!(pair[0], pair[1], "two creatures overlap at {pair:?}");
            }
            if sim.player_health() > 0 && live_goblin(&broker).is_some() {
                goblin_alive_hits += 1;
            }
            trace.push(cells);
        }
        assert!(
            goblin_alive_hits > 0,
            "goblin should be alive at least some of the run"
        );
        (trace, sim.player_health(), sim.player_turns_public())
    };

    let (a, ahp, aturn) = run();
    let (b, bhp, bturn) = run();
    assert_eq!(a, b, "position trace must be byte-identical across seeds");
    assert_eq!(ahp, bhp, "health identity");
    assert_eq!(aturn, bturn, "turn clock identity");
}
