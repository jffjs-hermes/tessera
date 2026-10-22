//! Headless deterministic test: every live entity with a `RenderTile` (the
//! `Render` part) is drawn on top of the terrain at its position, not just the
//! player. Builds the sim with the demo zone and a goblin, then calls the same
//! `build_cells` the renderer uses and asserts the cell at the goblin's
//! position carries the goblin's glyph (and that it is not the terrain glyph).

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use hecs::Entity;
use tessera::scripting::ScriptBridge;
use tessera::sim::{RenderTarget, Sim, build_cells};
use tessera_core::ecs::{EntityBroker, Part};
use tessera_core::events::{PART_POSITION, Point, Position};
use tessera_core::tiles::TileVisual;
use tessera_core::vfs::{DirVfs, Vfs};
use tessera_core::world::{RenderTile, World};
use tessera_core::zone::Zone;
use tessera_render::atlas::Atlas;

const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/base");
const PLAYER_BP: &str = "Player";
const GOBLIN_BP: &str = "Goblin";

/// Build the world exactly as the binary does: register engine parts, load
/// every shipped script, sync schemas, then load the blueprints.
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

/// Spawn a positioned actor from a blueprint into `broker`.
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

/// Build the sim: a player at (1,1) and a goblin at `gx,gy`, with the shipped
/// scripts loaded and a world-state singleton installed. Returns the sim and
/// the shared broker.
fn build_sim(gx: i32, gy: i32) -> (Sim, Arc<RwLock<EntityBroker>>) {
    let vfs = DirVfs::new(PathBuf::from(DATA));
    let broker: Arc<RwLock<EntityBroker>> = Arc::new(RwLock::new(EntityBroker::new()));
    let bridge = ScriptBridge::new(broker.clone());
    let world = Arc::new(build_world(&vfs, &bridge));
    bridge.set_world(world.clone());

    let mut b = broker.write().unwrap();
    let player = spawn_actor(&world, &mut b, PLAYER_BP, 1, 1);
    let goblin = spawn_actor(&world, &mut b, GOBLIN_BP, gx, gy);
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

/// The tile registry and atlas for the demo pack, built the same way the
/// binary (`load_content`) builds them.
fn tiles_and_atlas() -> (tessera_core::tiles::TileRegistry, Atlas) {
    let vfs = DirVfs::new(PathBuf::from(DATA));
    let tiles = tessera_core::tiles::TileRegistry::load(&vfs).expect("load tiles");
    let mut chars = tiles.glyph_chars();
    chars.push('?');
    let atlas = Atlas::build(&chars);
    (tiles, atlas)
}

/// The goblin's glyph char and the terrain glyph under a given cell.
fn glyph_at(tiles: &tessera_core::tiles::TileRegistry, map: &Zone, x: i32, y: i32) -> char {
    let TileVisual::Glyph { ch } = tiles.get(map.tile_id(x as usize, y as usize)).visual;
    ch
}

#[test]
fn every_renderable_entity_is_drawn_not_just_the_player() {
    // Build without submitting any actions, so the goblin stays parked and the
    // cell builder sees a stable post-setup world.
    let (sim, _broker) = build_sim(3, 1); // goblin at (3,1), which is floor.
    let (tiles, atlas) = tiles_and_atlas();

    let targets = sim.renderables();
    // Both the player and the goblin carry a RenderTile, so both are targets.
    let tiles_sorted: Vec<String> = {
        let mut v: Vec<String> = targets.iter().map(|t| t.tile.clone()).collect();
        v.sort();
        v
    };
    assert!(
        tiles_sorted.contains(&"creature.goblin".to_string()),
        "renderables must include the goblin, got {tiles_sorted:?}"
    );
    assert!(
        targets.contains(&RenderTarget {
            x: 3,
            y: 1,
            tile: "creature.goblin".to_string(),
        }),
        "the goblin target must be at its position, got {:?}",
        targets
    );

    let map = sim.map();
    let cells = build_cells(map, &tiles, &atlas, &targets);

    // The cell at the goblin's position must carry the goblin's glyph...
    let goblin_idx = map.width + 3;
    let goblin_cell = &cells[goblin_idx];
    let TileVisual::Glyph { ch: goblin_ch } = tiles.get("creature.goblin").visual;
    assert_eq!(
        goblin_cell.glyph,
        atlas.slot(goblin_ch),
        "goblin cell must use the goblin glyph"
    );

    // ...and must NOT be the terrain glyph underneath it (which is floor).
    let terrain_ch = glyph_at(&tiles, map, 3, 1);
    assert_ne!(
        tiles.get("creature.goblin").visual,
        TileVisual::Glyph { ch: terrain_ch },
        "the goblin glyph must differ from the terrain glyph under it"
    );
    assert_ne!(
        goblin_cell.glyph,
        atlas.slot(terrain_ch),
        "goblin cell must not be the terrain glyph"
    );

    // The player's cell still draws its own glyph, on floor.
    let player_idx = map.width + 1;
    let TileVisual::Glyph { ch: player_ch } = tiles.get("creature.player").visual;
    assert_eq!(cells[player_idx].glyph, atlas.slot(player_ch));
}

#[test]
fn terrain_cells_without_entities_keep_the_terrain_glyph() {
    let (sim, _broker) = build_sim(3, 1);
    let (tiles, atlas) = tiles_and_atlas();
    let map = sim.map();
    let cells = build_cells(map, &tiles, &atlas, &sim.renderables());

    // A floor cell far from the player and goblin stays floor.
    let floor_idx = 15 * map.width + 30;
    let TileVisual::Glyph { ch: floor_ch } = tiles.get("terrain.floor").visual;
    assert_eq!(cells[floor_idx].glyph, atlas.slot(floor_ch));
}
