//! Headless acceptance tests for the shipped sample mod (`data/mods/sample-mod`)
//! loaded through the real binary content path.
//!
//! Covers the demo mapping:
//!   - the sample mod adds a new "Bogling" blueprint (inherits base Goblin,
//!     carries a mod-defined `Charge` part) — spawns and carries Charge;
//!   - the sample mod patches the base Goblin: tile becomes
//!     `creature.goblin.angry`;
//!   - the mod's script handler fires on Attack and its shot appears in the
//!     deterministic event log (proof a mod script shares the engine dispatch);
//!   - all of the above is identical whether the same mod loads from a directory
//!     or from a zip built from that directory (folder / zip parity);
//!   - base blueprint files on disk are unchanged after a modded load (patches
//!     apply to in-memory copies).

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use tessera::mods::{Overlay, apply_mod_patches, discover_sources};
use tessera::scripting::ScriptBridge;
use tessera::sim::{PlayerAction, Sim};
use tessera_core::ecs::{EntityBroker, Part, PartTypeId};
use tessera_core::events::{PART_POSITION, Point, Position};
use tessera_core::tiles::TileRegistry;
use tessera_core::vfs::{DirVfs, Vfs};
use tessera_core::world::{RenderTile, World};
use tessera_core::zone::Zone;

const BASE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/base");
const MODS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/mods");

/// Register the engine part set exactly as the binary does: the event-facing
/// parts the binary registers by hand (the native `Attributes`/`Level`/
/// `Bonuses` parts are registered centrally by `World::new`).
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

/// Load content exactly as the binary does: base under one overlay with every
/// discovered mod, scripts + blueprints through it, then per-mod patches. The
/// caller supplies the mods root (either the on-disk folder, or a temp dir
/// holding a zip of the same sample) so folder/zip parity can be compared.
fn load_content_world(
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
    // Scripts: every shipped base script + the sample mod's `bogling-tweak.luau`
    // resolve through the overlay, registering its Attack handler.
    for path in overlay.list("scripts").unwrap_or_default() {
        if path.ends_with(".luau") {
            let src = overlay.read_to_string(&path).expect("read script");
            bridge.load(&path, &path, &src).expect("load script");
        }
    }
    bridge.sync_schemas(&mut world).expect("sync schemas");
    world.load(&overlay).expect("load blueprints");
    // Apply the sample mod's Goblin patch (tile) in load order.
    apply_mod_patches(&world, &registry).expect("apply mod patches");

    let world = Arc::new(world);
    bridge.set_world(world.clone());
    (world, tiles, broker, bridge)
}

/// A bordered all-floor room: a tiny deterministic arena for the demo.
fn demo_zone() -> Zone {
    const W: usize = 20;
    const H: usize = 10;
    let mut cells = vec!["terrain.floor".to_string(); W * H];
    for y in 0..H {
        for x in 0..W {
            if x == 0 || y == 0 || x == W - 1 || y == H - 1 {
                cells[y * W + x] = "terrain.wall".to_string();
            }
        }
    }
    Zone::from_cells("demo", W, H, Point::new(1, 1), cells)
}

/// Spawn a positioned actor from a blueprint (same seam the binary uses).
fn spawn_at(world: &World, broker: &mut EntityBroker, bp: &str, x: i32, y: i32) -> hecs::Entity {
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

/// Read the resolved Goblin blueprint's `Attributes.might` as a raw integer
/// (the value every creature inherits from the base `Actor` root unless the
/// blueprint overrides it — the stats-slice Goblin carries its own build
/// (Might 9); patches operate on raw payloads). Attributes/Level/Bonuses
/// replace the legacy `Stats` part, which must be gone from the resolved
/// blueprint entirely.
fn goblin_might(world: &World) -> i64 {
    let lib = world.library().read().unwrap_or_else(|p| p.into_inner());
    let goblin = lib.get("Goblin").expect("Goblin blueprint");
    goblin
        .part("Attributes")
        .expect("Goblin inherits the base Attributes part")
        .get("might")
        .expect("Attributes.might key")
        .as_integer()
        .expect("might is an integer")
}

/// Read a spawned GoBogli/Charge part's integer field.
fn part_int(broker: &RwLock<EntityBroker>, entity: hecs::Entity, part: &str, field: &str) -> i64 {
    let b = broker.read().unwrap();
    let part = b
        .get_part(entity, &PartTypeId::new(part))
        .expect("part present");
    let sc = part
        .as_any()
        .downcast_ref::<tessera_core::script_part::ScriptPart>()
        .expect("script part");
    match sc.0.get(field) {
        Some(tessera_core::script_part::PartValue::Int(n)) => *n,
        other => panic!("{field} is an int, got {other:?}"),
    }
}

/// The tile id the spawned Goblin's Render part resolves to (post-patch).
fn goblin_tile(broker: &RwLock<EntityBroker>, entity: hecs::Entity) -> String {
    let b = broker.read().unwrap();
    b.world()
        .get::<&RenderTile>(entity)
        .map(|r| r.tile.clone())
        .unwrap_or_else(|_| panic!("spawned Goblin has a Render component"))
}

/// Drive the demo: player at (1,1), patched Goblin at (2,1), the mod's Bogling
/// at (3,1). Submit a Zip toward the goblin (Zap = Attack), which fires the
/// mod's Attack handler, then return the full event log.
fn run_demo(
    world: &Arc<World>,
    broker: &Arc<RwLock<EntityBroker>>,
    bridge: ScriptBridge,
) -> (Vec<String>, i64, String) {
    let mut b = broker.write().unwrap();
    let player = spawn_at(world, &mut b, "Player", 1, 1);
    let goblin = spawn_at(world, &mut b, "Goblin", 2, 1);
    let bogling = spawn_at(world, &mut b, "Bogling", 3, 1);
    drop(b);

    let goblin_tile = goblin_tile(broker, goblin);
    let charge = part_int(broker, bogling, "Charge", "charges");

    // Sim harness with the goblin as the reference enemy; the player Zaps it.
    let mut sim = Sim::from_shared(
        broker.clone(),
        Some(bridge),
        demo_zone(),
        player,
        Some(goblin),
    );
    // Zap fires Attack at the last-faced tile in front of the player -> goblin.
    sim.submit_action(PlayerAction::Zap).expect("zap");
    let log = sim.log().to_vec();
    (log, charge, goblin_tile)
}

#[test]
fn sample_mod_adds_bogling_charge_patches_goblin_and_logs_handler() {
    let (world, _tiles, broker, bridge) = load_content_world(Path::new(MODS));

    // Bogling blueprint exists only because the mod is loaded.
    assert!(
        world.library().read().unwrap().contains("Bogling"),
        "sample mod must add a Bogling blueprint"
    );
    // The legacy `Stats` part is gone from the resolved blueprint; every
    // creature carries `Attributes`, and the stats-slice Goblin restates its
    // own build (Might 9) as a blueprint override of the Actor root defaults.
    assert_eq!(goblin_might(&world), 9);
    {
        let lib = world.library().read().unwrap();
        let goblin = lib.get("Goblin").expect("Goblin blueprint");
        assert!(
            goblin.part("Stats").is_none(),
            "legacy Stats part must be removed from the resolved blueprint"
        );
    }
    // Its resolved tile is the mod's recolor.
    let lib = world.library().read().unwrap();
    let goblin = lib.get("Goblin").unwrap();
    let tile = goblin
        .part("Render")
        .unwrap()
        .get("tile")
        .unwrap()
        .as_str()
        .unwrap();
    assert_eq!(tile, "creature.goblin.angry");

    let (log, charge, goblin_tile) = run_demo(&world, &broker, bridge);
    // Bogling carries the mod-defined Charge part (charges = 5).
    assert_eq!(charge, 5, "Bogling Charge part from the sample mod");
    // The spawned Goblin uses the mod's patched tile.
    assert_eq!(goblin_tile, "creature.goblin.angry");
    // The mod's script handler fired: its engine.log line is in the event log.
    assert!(
        log.iter().any(|l| l.contains("bogling tweak")),
        "mod script handler must fire on Attack; log={log:?}"
    );
}

#[test]
fn base_blueprint_files_patch_in_memory_only() {
    // Snapshot base blueprint bytes, load with mods, confirm disk files unchanged.
    let creatures = std::fs::read(format!("{BASE}/blueprints/creatures.toml")).unwrap();
    let (world, _t, _b, _bridge) = load_content_world(Path::new(MODS));
    // The patch is applied in memory: the goblin's resolved tile is the mod's
    // recolor (the sample patch's only effect now that Stats is gone).
    {
        let lib = world.library().read().unwrap();
        let goblin = lib.get("Goblin").unwrap();
        let tile = goblin
            .part("Render")
            .unwrap()
            .get("tile")
            .unwrap()
            .as_str()
            .unwrap();
        assert_eq!(tile, "creature.goblin.angry", "patched in memory");
    }
    let after = std::fs::read(format!("{BASE}/blueprints/creatures.toml")).unwrap();
    assert_eq!(
        creatures, after,
        "base blueprint files are not mutated on disk"
    );
}

/// Folder/zip parity: the same sample-mod loaded from a directory and from a
/// zip of that directory must produce identical headless output (the demo
/// acceptance for "zip-able folders via the VFS").
#[test]
fn folder_and_zip_sample_mod_produce_identical_output() {
    // Build a zip mirroring the folder layout (root = sample-mod/ ...).
    let zip_dir = std::env::temp_dir().join("tessera-mod-demo-zip");
    let _ = std::fs::remove_dir_all(&zip_dir);
    std::fs::create_dir_all(&zip_dir).unwrap();
    let zip_path = zip_dir.join("sample-mod.zip");
    write_sample_zip(&zip_path);

    let folder_out = run_full_headless(Path::new(MODS));
    let zip_out = run_full_headless(&zip_dir);
    assert_eq!(
        folder_out, zip_out,
        "folder and zip sample mods must produce identical headless output"
    );
    let _ = std::fs::remove_dir_all(&zip_dir);
}

/// Run the full headless demo invocation against a mods root and return its
/// stdout (map + event log + status), mirroring `--headless`.
fn run_full_headless(mods_root: &Path) -> String {
    let (world, _tiles, broker, bridge) = load_content_world(mods_root);
    let (log, _charge, _tile) = run_demo(&world, &broker, bridge);
    let mut out = String::new();
    for line in log {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Write a zip of `data/mods/sample-mod` into `target`, with the mod folder as
/// its single top-level dir (the "download from GitHub" archive shape ZipVfs
/// auto-detects), for the folder/zip parity test.
fn write_sample_zip(target: &Path) {
    use std::io::Write;
    let file = std::fs::File::create(target).unwrap();
    let mut writer = zip::ZipWriter::new(file);
    let sample_root = Path::new(MODS).join("sample-mod");
    let options: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for entry in walkdir(&sample_root) {
        let rel = entry
            .strip_prefix(&sample_root)
            .unwrap()
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        let name = format!("sample-mod/{rel}");
        if entry.is_dir() {
            writer.add_directory(&name, options).unwrap();
        } else {
            writer.start_file(&name, options).unwrap();
            writer.write_all(&std::fs::read(&entry).unwrap()).unwrap();
        }
    }
    writer.finish().unwrap();
}

/// Recursively collect files + dirs under `root` in sorted order (no itertools).
fn walkdir(root: &Path) -> Vec<PathBuf> {
    fn walk(cur: &Path, out: &mut Vec<PathBuf>) {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(cur)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort_unstable();
        for p in entries {
            out.push(p.clone());
            if p.is_dir() {
                walk(&p, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out
}
