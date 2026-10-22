//! Devtools tests for the stats slice (inspector and console visibility): the
//! inspector summary and the read-only console `stats <id>` command, both
//! showing **live** derived values after a dirtying event, plus the headless
//! demo's visible entity/draw mapping.
//!
//! - the inspector snapshot's row summary carries the live `Derived` values
//!   after a status application and an attribute write (not a stale copy),
//!   and the snapshot builder is read-only;
//! - the console `stats <id|player>` command prints the five values plus the
//!   contributing terms (attribute, level, bonuses, each live status's
//!   modifier contribution) after a status application, and there is **no
//!   write path for Derived**: executing `stats` leaves every cache
//!   byte-identical, and no `setderived` command exists;
//! - the `--headless` demo path (event log + text map + entity readout)
//!   shows spawn and `Status_Staggered` recomputes and a goblin whose speed
//!   reflects Agility — visible on the map, not just in the log.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use tessera::devtools::console::{self, ConsoleContext, ConsoleOutcome};
use tessera::devtools::inspector::snapshot;
use tessera::mods::{Overlay, apply_mod_patches, discover_sources};
use tessera::scripting::ScriptBridge;
use tessera::sim::{PlayerAction, Sim};
use tessera_core::ecs::{EntityBroker, PartTypeId};
use tessera_core::effects::{Derived, Modifiers, PART_DERIVED, PART_STATUS_EFFECTS, StatusEffects};
use tessera_core::events::{PART_POSITION, Point, Position};
use tessera_core::stats::PART_ATTRIBUTES;
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
    Zone::from_cells("s1", 12, 12, Point::new(1, 1), cells)
}

fn demo_sim(
    world: &World,
    broker: Arc<RwLock<EntityBroker>>,
    bridge: ScriptBridge,
) -> (Sim, hecs::Entity, hecs::Entity) {
    let mut b = broker.write().unwrap_or_else(|p| p.into_inner());
    let player = spawn_at(world, &mut b, "Player", 2, 2);
    let goblin = spawn_at(world, &mut b, "Goblin", 3, 2);
    drop(b);
    let sim = Sim::from_shared(broker, Some(bridge), demo_zone(), player, Some(goblin));
    (sim, player, goblin)
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

/// Every live `Derived` payload, serialized — the byte-level fingerprint
/// used to prove the devtools surfaces never write the cache.
fn derived_fingerprint(sim: &Sim) -> Vec<(u64, String)> {
    let b = sim.broker().read().unwrap_or_else(|p| p.into_inner());
    let mut out: Vec<(u64, String)> = b
        .entities()
        .into_iter()
        .filter_map(|e| {
            b.typed_part::<Derived>(e, &PartTypeId::new(PART_DERIVED))
                .map(|d| (e.to_bits().get(), format!("{:?}", d.values)))
        })
        .collect();
    out.sort_by_key(|(id, _)| *id);
    out
}

fn console_ctx<'a>(
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

fn tmp_save_dir(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("tessera-s1-devtools-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp save dir");
    dir
}

/// The inspector row summary shows the **live** derived values after a
/// dirtying event, and the snapshot builder never mutates the broker.
#[test]
fn inspector_row_summary_shows_live_derived_after_dirtying_events() {
    let (world, _tiles, broker, bridge) = load_real_content(Path::new(MODS));
    let (mut sim, player, _goblin) = demo_sim(&world, broker, bridge);

    // Baseline: the shipped goblin's acceptance summary is live from the cache.
    let snap_before = snapshot(&sim, &world, &world.save_hooks());
    let goblin_row = snap_before
        .entities
        .iter()
        .find(|r| r.blueprint == "Goblin")
        .expect("goblin row");
    assert!(
        goblin_row.summary.contains("HP 20/34"),
        "summary carries live HP cur/max: {}",
        goblin_row.summary
    );
    assert!(
        goblin_row.summary.contains("Acc 8"),
        "goblin accuracy live: {}",
        goblin_row.summary
    );
    assert!(
        goblin_row.summary.contains("Spd 106"),
        "goblin speed lives in the summary (Agility 13 -> 106): {}",
        goblin_row.summary
    );
    let player_row = snap_before
        .entities
        .iter()
        .find(|r| r.blueprint == "Player")
        .expect("player row");
    assert!(player_row.summary.contains("Might 14"));
    assert!(player_row.summary.contains("Lv 1"));
    assert!(player_row.summary.contains("Spd 98"), "player A9 -> 98");

    // Dirtying event 1: a live status application (speed_pct = 90) →
    // the summary must reflect 108 immediately (live cache, not a stale
    // snapshot).
    write_status(&sim, player, mods_pct(90));
    sim.recompute_live();
    let snap_mid = snapshot(&sim, &world, &world.save_hooks());
    let player_row = snap_mid
        .entities
        .iter()
        .find(|r| r.blueprint == "Player")
        .expect("player row");
    assert!(
        player_row.summary.contains("Spd 108"),
        "inspector summary shows the post-status speed: {}",
        player_row.summary
    );

    // Dirtying event 2: an attribute write (Agility 20) → base 120.
    sim.set_part_console(
        player,
        PART_ATTRIBUTES,
        &BTreeMap::from([("agility".to_string(), 20)]),
    )
    .expect("setpart agility");
    let snap_after = snapshot(&sim, &world, &world.save_hooks());
    let player_row = snap_after
        .entities
        .iter()
        .find(|r| r.blueprint == "Player")
        .expect("player row");
    assert!(
        player_row.summary.contains("Agility 20"),
        "summary reads live Attributes: {}",
        player_row.summary
    );
    assert!(
        player_row.summary.contains("Spd 108"),
        "120 * 90 // 100 stays live after the attribute dirtying: {}",
        player_row.summary
    );

    // Read-only: two consecutive snapshots are identical, and the broker's
    // Derived payloads are byte-identical before and after the snapshot.
    assert_eq!(snap_after, snapshot(&sim, &world, &world.save_hooks()));
    let fp_before = derived_fingerprint(&sim);
    let _ = snapshot(&sim, &world, &world.save_hooks());
    assert_eq!(
        fp_before,
        derived_fingerprint(&sim),
        "snapshot never writes"
    );
}

/// The console `stats <id|player>` command is read-only and prints live
/// values plus the contributing terms after a status application; there is
/// no `setderived` write path.
#[test]
fn console_stats_is_read_only_and_shows_live_values_after_status() {
    let (world, _tiles, broker, bridge) = load_real_content(Path::new(MODS));
    let (mut sim, player, _goblin) = demo_sim(&world, broker, bridge);
    let save_dir = tmp_save_dir("console");
    let hooks = world.save_hooks();

    // Baseline via the numeric id and the `player` alias.
    let goblin_id = {
        let b = sim.broker().read().unwrap_or_else(|p| p.into_inner());
        b.entities_with_part(PART_POSITION)
            .into_iter()
            .find(|e| *e != player)
            .map(|e| e.to_bits().get())
            .expect("goblin id")
    };

    // Dirtying event: a live `speed_pct = 90` status; stats must show 108.
    write_status(&sim, player, mods_pct(90));
    sim.recompute_live();
    let fp_before = derived_fingerprint(&sim);

    let text_player = match console::execute(
        &mut sim,
        &console_ctx(&world, &hooks, &save_dir),
        console::parse("stats player").expect("parse"),
    ) {
        ConsoleOutcome::Ok(t) => t,
        other => panic!("stats player failed: {other:?}"),
    };
    // The five live values, the acceptance style, post-status.
    assert!(text_player.contains("Spd 108"), "{text_player}");
    assert!(text_player.contains("HP 20/36"), "{text_player}");
    // The contributing terms: attributes, level, bonuses, status modifiers.
    assert!(
        text_player.contains("attributes: might=14 agility=9"),
        "{text_player}"
    );
    assert!(text_player.contains("level=1"), "{text_player}");
    assert!(
        text_player.contains("bonuses: accuracy=0 dodge=0 ac=0"),
        "{text_player}"
    );
    assert!(
        text_player.contains("status Status_Chilled x1: speed pct=90"),
        "{text_player}"
    );
    assert!(text_player.contains("speed = 108"), "{text_player}");

    // The goblin row: acceptance numbers through the numeric-id form.
    let text_goblin = match console::execute(
        &mut sim,
        &console_ctx(&world, &hooks, &save_dir),
        console::parse(&format!("stats {goblin_id}")).expect("parse"),
    ) {
        ConsoleOutcome::Ok(t) => t,
        other => panic!("stats {goblin_id} failed: {other:?}"),
    };
    assert!(text_goblin.contains("Spd 106"), "{text_goblin}");
    assert!(text_goblin.contains("Def 12"), "{text_goblin}");
    assert!(
        text_goblin.contains("attributes: might=9 agility=13"),
        "{text_goblin}"
    );
    assert!(text_goblin.contains("level=2"), "{text_goblin}");

    // Read-only: the caches are byte-identical after both stats calls, and a
    // bogus id fails cleanly.
    assert_eq!(
        fp_before,
        derived_fingerprint(&sim),
        "stats never writes Derived"
    );
    let err = match console::execute(
        &mut sim,
        &console_ctx(&world, &hooks, &save_dir),
        console::parse("stats 999999999").expect("parse"),
    ) {
        ConsoleOutcome::Err(e) => e,
        other => panic!("expected Err for a bogus id, got {other:?}"),
    };
    assert!(err.contains("no live entity"), "{err}");

    // No write path for Derived: `setderived` is not a command, and the
    // help text documents `stats` but nothing that writes the cache.
    assert!(
        console::parse("setderived 3 speed 99").is_err(),
        "no setderived command exists"
    );
    assert!(
        console::parse("stats player speed 99").is_err(),
        "stats takes exactly one argument"
    );
    let help = console::execute(
        &mut sim,
        &console_ctx(&world, &hooks, &save_dir),
        console::parse("help").expect("parse"),
    );
    let help_text = match help {
        ConsoleOutcome::Ok(t) => t,
        other => panic!("help failed: {other:?}"),
    };
    assert!(help_text.contains("stats <id|player>"), "{help_text}");
    assert!(!help_text.contains("setderived"), "{help_text}");
    std::fs::remove_dir_all(&save_dir).ok();
}

/// The headless demo wiring: the event log shows spawn and
/// `Status_Staggered` recomputes in order, the text map visibly draws the
/// player and the goblin at their cells, and the entity readout names the
/// goblin's Agility-derived speed — the draw mapping is the visibility
/// proof, not the log alone.
#[test]
fn headless_demo_shows_spawn_and_stagger_recomputes_and_goblin_speed() {
    let (world, tiles, broker, bridge) = load_real_content(Path::new(MODS));
    let (mut sim, player, goblin) = demo_sim(&world, broker, bridge);

    // The Bash dirtying event: Status_Staggered lands on the goblin and the
    // recompute follows it in the log (the spawn recomputes are already
    // logged at sim construction — two stat-ful entities).
    sim.submit_action(PlayerAction::Bash).expect("bash");

    let log = sim.log().to_vec();
    let recomputes: Vec<usize> = log
        .iter()
        .enumerate()
        .filter(|(_, l)| l.starts_with("EVENT RecomputeDerived"))
        .map(|(i, _)| i)
        .collect();
    assert!(
        recomputes.len() >= 3,
        "spawn recompute per stat-ful entity + the status recompute: {log:?}"
    );
    // The first two lines are the spawn recomputes (player, then goblin —
    // entity-id order); the Status_Staggered recompute follows its
    // ApplyStatus line.
    let status_idx = log
        .iter()
        .position(|l| l.starts_with("EVENT ApplyStatus") && l.contains("Status_Staggered"))
        .expect("Bash must apply Status_Staggered");
    assert!(
        log[status_idx + 1..]
            .iter()
            .any(|l| l.starts_with("EVENT RecomputeDerived")),
        "recompute fires after the stagger: {log:?}"
    );

    // Draw mapping: the goblin glyph is on the map at the goblin's cell,
    // and the player's '@' too.
    let map = sim.text_map(&tiles);
    let (gx, gy) = {
        let b = sim.broker().read().unwrap_or_else(|p| p.into_inner());
        let pos = b
            .typed_part::<Position>(goblin, &PART_POSITION.into())
            .expect("goblin position");
        (pos.x, pos.y)
    };
    let (px, py) = {
        let b = sim.broker().read().unwrap_or_else(|p| p.into_inner());
        let pos = b
            .typed_part::<Position>(player, &PART_POSITION.into())
            .expect("player position");
        (pos.x, pos.y)
    };
    let glyph_at = |x: i32, y: i32| -> char {
        map.lines()
            .nth(y as usize)
            .and_then(|l| l.chars().nth(x as usize))
            .expect("cell in map")
    };
    assert_eq!(glyph_at(px, py), '@', "player drawn from its tile glyph");
    assert_eq!(glyph_at(gx, gy), 'g', "goblin drawn from its tile glyph");

    // The entity readout names the map's glyphs with live derived values.
    let readout = sim.demo_entity_lines(&tiles);
    assert!(readout.contains("creature.player"), "{readout}");
    assert!(readout.contains("Spd 98"), "player A9: {readout}");
    assert!(readout.contains("creature.goblin"), "{readout}");
    assert!(
        readout.contains("Spd 106"),
        "goblin speed reflects Agility 13 (no longer a flat 100): {readout}"
    );
    assert!(readout.contains("HP 20/34"), "{readout}");
}
