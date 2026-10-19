//! The shipped `data/base/zones/start.toml` loads as a generated cave and its
//! spawns land on walkable tiles.
//!
//! Covers the acceptance "shipped zone uses caves": the zone file must parse
//! through the loader's `caves` generator branch, produce the deterministic
//! 80x50 grid, and the demo's spawn logic (player at `zone.start`, goblin at a
//! zone-id-seeded `random_walkable_empty` tile) must land both on walkable
//! terrain. The seed derivation here mirrors the demo binary so the goblin's
//! position is reproducible.

use tessera_core::events::Point;
use tessera_core::rng::SimRng;
use tessera_core::vfs::DirVfs;
use tessera_core::zone::load_zones;

const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/base");

/// Deterministic per-zone RNG stream used to pick random spawn tiles (the
/// `"zone-spawn"` stream), identical to the demo binary's derivation so the
/// goblin lands on the same tile here as it does in `--headless`.
fn zone_spawn_rng(zone_id: &str) -> SimRng {
    const ZONE_SPAWN: u64 = 0x7a6f_6e65_5f73_7061; // "zone_spa"
    let mut acc: u64 = ZONE_SPAWN;
    for b in zone_id.bytes() {
        acc = acc
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(b as u64);
    }
    SimRng::from_seed(acc)
}

#[test]
fn shipped_start_toml_loads_as_caves_with_walkable_spawns() {
    let vfs = DirVfs::new(DATA);
    let tiles = tessera_core::tiles::TileRegistry::load(&vfs).expect("load tile registry");
    let world = load_zones(&vfs, &tiles).expect("load shipped start.toml");
    let zone = world
        .get("start")
        .expect("shipped pack has the 'start' zone");

    // It's a cave, not the ascii room: 80x50 and holding both floor and wall.
    assert_eq!((zone.width, zone.height), (80, 50));
    let has_wall =
        (0..zone.height).any(|y| (0..zone.width).any(|x| zone.tile_id(x, y) == "terrain.wall"));
    assert!(has_wall, "generated cave contains wall");

    // Spawn table survives: Player at start, Goblin in the random list.
    assert_eq!(zone.spawns.start.as_deref(), Some("Player"));
    assert!(zone.spawns.random.iter().any(|b| b == "Goblin"));

    // Player start is walkable.
    assert!(
        zone.is_walkable(zone.start.x, zone.start.y),
        "player start ({},{}) must be walkable",
        zone.start.x,
        zone.start.y
    );

    // The goblin spawns at a zone-id-seeded walkable-empty tile (same selection
    // as the demo binary), which the loader's spawn code guarantees is
    // walkable and unoccupied by the player.
    let occupied = |x: i32, y: i32| x == zone.start.x && y == zone.start.y;
    let mut rng = zone_spawn_rng(&zone.id);
    let goblin = tessera_core::path::random_walkable_empty(zone, &occupied, &mut rng)
        .expect("cave has a walkable-empty goblin spawn");
    assert!(
        zone.is_walkable(goblin.x, goblin.y),
        "goblin spawn is walkable"
    );
    assert_ne!(
        Point::new(goblin.x, goblin.y),
        zone.start,
        "goblin does not share the player's start tile"
    );
}
