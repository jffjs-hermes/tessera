//! Shield Bash vertical slice, end to end.
//!
//! Proves the four merged-schema conditions through the real content path
//! (`data/mods/tessera-campaign` loads as a mod), a seeded headless sim, and
//! the Luau assertion script:
//!
//!   1. the goblin's HP drops into the declared blunt range (3..6) with
//!      `TakeDamage(blunt)`;
//!   2. `Status_Staggered` is present on the target;
//!   3. the attacker's native charge pool spent exactly one (3 -> 2);
//!   4. the stagger skip is observed through the scheduler seam (the
//!      staggered goblin's next scheduled action is consumed, not executed);
//!
//! plus the named `--headless` event-log mapping (UseAbility ->
//! TakeDamage(blunt) -> ApplyStatus(Status_Staggered) in order) and the
//! focused hot-reload extension test (only *new* status applications read
//! the reloaded duration; in-flight instances keep their snapshot).
//!
//! Scope guard: no combat-sim harness, scenario/bot formats, band policy,
//! or sweep-driver behavior is exercised here.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use tessera::mods::{Overlay, apply_mod_patches, discover_sources};
use tessera::scripting::ScriptBridge;
use tessera::sim::{PlayerAction, Sim};
use tessera_core::ecs::{EntityBroker, Part, PartTypeId};
use tessera_core::effects::{Charges, PART_CHARGES, PART_STATUS_EFFECTS, StatusEffects};
use tessera_core::events::{ApplyStatus, PART_POSITION, Point, Position, SimContext};
use tessera_core::rng::SimRng;
use tessera_core::tiles::TileRegistry;
use tessera_core::vfs::{DirVfs, MemVfs, Vfs};
use tessera_core::world::{RenderTile, World};
use tessera_core::zone::Zone;

const BASE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/base");
const MODS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/mods");

/// Register the engine part set exactly as the binary does: the event-facing
/// parts the binary registers by hand (the native `Attributes`/`Level`/
/// `Bonuses` parts are registered centrally by `World::new`, same as the
/// content-kind parts).
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

/// Load the real shipped content exactly as the binary does: base under one
/// overlay with every discovered mod (`data/mods/sample-mod` and
/// `data/mods/tessera-campaign`), scripts + blueprints through it, then the
/// per-mod patches. `world.load` also builds the tuning store, so
/// `engine.tuning("Tuning_Combat")` is live.
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
    for path in overlay.list("scripts").unwrap_or_default() {
        if path.ends_with(".luau") {
            let src = overlay.read_to_string(&path).expect("read script");
            bridge.load(&path, &path, &src).expect("load script");
        }
    }
    bridge.sync_schemas(&mut world).expect("sync schemas");
    world.load(&overlay).expect("load blueprints");
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

fn broker_read(broker: &RwLock<EntityBroker>) -> std::sync::RwLockReadGuard<'_, EntityBroker> {
    broker.read().unwrap_or_else(|p| p.into_inner())
}

fn charges(broker: &RwLock<EntityBroker>, entity: hecs::Entity) -> i64 {
    broker_read(broker)
        .typed_part::<Charges>(entity, &PartTypeId::new(PART_CHARGES))
        .expect("charge pool")
        .charges
}

fn health_hp(broker: &RwLock<EntityBroker>, entity: hecs::Entity) -> i32 {
    broker_read(broker)
        .typed_part::<tessera_core::events::Health>(
            entity,
            &PartTypeId::new(tessera_core::events::PART_HEALTH),
        )
        .expect("health part")
        .hp
}

fn stagger_skip(broker: &RwLock<EntityBroker>, entity: hecs::Entity) -> i64 {
    broker_read(broker)
        .typed_part::<StatusEffects>(entity, &PartTypeId::new(PART_STATUS_EFFECTS))
        .expect("status store")
        .skip_turns("Status_Staggered")
}

fn position_of(broker: &RwLock<EntityBroker>, entity: hecs::Entity) -> (i32, i32) {
    let b = broker_read(broker);
    let p = b
        .typed_part::<Position>(entity, &PartTypeId::new(PART_POSITION))
        .expect("position");
    (p.x, p.y)
}

/// The baseline Goblin HP once the campaign patch raises it (also read by the
/// assertion script from `Tuning_Combat.goblin_hp`).
const GOBLIN_HP: i32 = 20;
const BASH_LO: i32 = 3;
const BASH_HI: i32 = 6;

/// The event-log mapping's three lines, in causal order: the `UseAbility`
/// turn line (EVENT ... handled), then the nested `TakeDamage(blunt)` and
/// `ApplyStatus(Status_Staggered)` observer lines.
fn assert_bash_event_order(log: &[String]) -> i32 {
    let use_idx = log
        .iter()
        .position(|l| l.starts_with("EVENT UseAbility "))
        .expect("log must contain the UseAbility turn line");
    let dmg_idx = log
        .iter()
        .position(|l| l.starts_with("EVENT TakeDamage ") && l.contains("dtype=blunt"))
        .expect("log must contain TakeDamage(blunt)");
    let st_idx = log
        .iter()
        .position(|l| l.starts_with("EVENT ApplyStatus ") && l.contains("status=Status_Staggered"))
        .expect("log must contain ApplyStatus(Status_Staggered)");
    assert!(
        use_idx < dmg_idx && dmg_idx < st_idx,
        "event order must be UseAbility -> TakeDamage(blunt) -> ApplyStatus(Status_Staggered); \
         got indices {use_idx} < {dmg_idx} < {st_idx} in {log:?}"
    );
    // The damage amount on the blunt line (the seeded roll in lo..=hi).
    let amount = log[dmg_idx]
        .split("amount=")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse::<i32>().ok())
        .expect("blunt line carries an integer amount");
    assert!(
        (BASH_LO..=BASH_HI).contains(&amount),
        "roll {amount} outside declared 3..6"
    );
    amount
}

// ---------------------------------------------------------------------------
// 1. The seeded end-to-end path: the four merged-schema conditions
// ---------------------------------------------------------------------------

/// The full observable outcome of one bash scenario (used for the seeded
/// assertions and the determinism comparison).
struct BashScenario {
    log: Vec<String>,
    goblin_hp: i32,
    player_charges: i64,
    skip_after: i64,
    goblin_pos: (i32, i32),
    player_hp: i32,
    script_lines: Vec<String>,
}

/// Build a fresh content world, spawn the player (campaign-granted ability)
/// and a goblin, bash, then let the goblin's next turn resolve. Returns the
/// full observable outcome for the determinism comparison.
fn run_bash_scenario() -> BashScenario {
    let (world, _tiles, broker, bridge) = load_content_world(Path::new(MODS));
    let mut b = broker.write().unwrap();
    let player = spawn_at(&world, &mut b, "Player", 2, 2);
    let goblin = spawn_at(&world, &mut b, "Goblin", 3, 2);
    drop(b);

    let mut sim = Sim::from_shared(
        broker.clone(),
        Some(bridge),
        demo_zone(),
        player,
        Some(goblin),
    );

    // The Bash turn: use the player's first granted ability (Shield Bash) on
    // the reference enemy — declarative damage/status/push, native charges.
    sim.submit_action(PlayerAction::Bash).expect("bash");

    // The goblin's next scheduled turn: it stands adjacent and would attack,
    // but the stagger consumes its action (the scheduler seam).
    sim.submit_action(PlayerAction::Wait).expect("wait");

    let log = sim.log().to_vec();
    let goblin_hp = health_hp(&broker, goblin);
    let goblin_pos = position_of(&broker, goblin);
    let player_hp = health_hp(&broker, player);
    let script_lines: Vec<String> = log
        .iter()
        .filter(|l| l.contains("shield bash check:"))
        .cloned()
        .collect();
    BashScenario {
        log,
        goblin_hp,
        player_charges: charges(&broker, player),
        skip_after: stagger_skip(&broker, goblin),
        goblin_pos,
        player_hp,
        script_lines,
    }
}

#[test]
fn campaign_mod_loads_the_vertical_slice_content() {
    let (world, _tiles, _broker, _bridge) = load_content_world(Path::new(MODS));
    let lib = world.library().read().expect("library lock");
    // The content conventions load as a real mod: the ability, the status and
    // the tuning table all resolve from `data/mods/tessera-campaign`.
    for id in ["Ability_ShieldBash", "Status_Staggered", "Tuning_Combat"] {
        assert!(lib.contains(id), "campaign mod must define '{id}'");
    }
    // The Player patch granted the ability (its spawn stamps the pool) and
    // the Goblin patch raised HP to the tuning-declared baseline.
    let player = lib.get("Player").expect("Player blueprint");
    let abilities = player
        .part("Abilities")
        .expect("Player patched with parts.Abilities")
        .get("list")
        .and_then(|v| v.as_array())
        .expect("Abilities.list is an array");
    assert_eq!(
        abilities
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>(),
        vec!["Ability_ShieldBash"]
    );
    let goblin = lib.get("Goblin").expect("Goblin blueprint");
    assert_eq!(
        goblin
            .part("Health")
            .expect("Health part")
            .get("hp")
            .and_then(|v| v.as_integer()),
        Some(GOBLIN_HP as i64)
    );
}

#[test]
fn shield_bash_seeded_end_to_end_proves_the_four_conditions() {
    let s = run_bash_scenario();

    // Condition 1: the goblin's HP dropped by the declared 3..6 with blunt
    // type, observed through the event log (the amount matches the roll).
    let rolled = assert_bash_event_order(&s.log);
    let dropped = GOBLIN_HP - s.goblin_hp;
    assert_eq!(dropped, rolled, "logged blunt roll must equal the HP drop");
    assert!(
        (BASH_LO..=BASH_HI).contains(&dropped),
        "HP drop {dropped} outside declared 3..6"
    );

    // Condition 3: attacker charges spent natively 3 -> 2 (verified inside the
    // scenario closure state: the final charge count is 2).
    assert_eq!(
        s.player_charges, 2,
        "one use spends exactly one native charge"
    );

    // Declarative push seam: the goblin was pushed one tile away from the
    // player (3,2) -> (4,2).
    assert_eq!(
        s.goblin_pos,
        (4, 2),
        "Effect.push moves the target one tile"
    );

    // The goblin's next scheduled turn was consumed by the stagger (it stood
    // adjacent and would attack): no damage landed on the player (base player
    // baseline is 20), and the skip credit is spent (the scheduler seam).
    assert_eq!(
        s.player_hp, PLAYER_BASELINE_HP,
        "staggered goblin must not attack on its next turn; player hp {}",
        s.player_hp
    );
    assert_eq!(s.skip_after, 0, "the scheduler consumed the skip credit");

    // The Luau assertion script ran on the goblin's turns and PASSed its
    // checks (it reads Charges/StatusEffects/Health/tuning through the
    // whitelisted API).
    assert!(
        s.script_lines
            .iter()
            .any(|l| l.contains("hp=") && l.contains("PASS")),
        "script HP check must pass; got {:?}",
        s.script_lines
    );
    assert!(
        s.script_lines
            .iter()
            .any(|l| l.contains("status=Status_Staggered") && l.contains("PASS")),
        "script status check must pass; got {:?}",
        s.script_lines
    );
    assert!(
        s.script_lines
            .iter()
            .any(|l| l.contains("charges=2") && l.contains("PASS")),
        "script charges check must pass; got {:?}",
        s.script_lines
    );
    assert!(
        s.log
            .iter()
            .any(|l| l.contains("stagger owed") && l.contains("PASS")),
        "script must observe the owed stagger; got {:?}",
        s.log
    );
    assert!(
        s.log.iter().any(|l| l.contains("hold")),
        "script must log its summary line; got {:?}",
        s.log
    );
}

/// The player HP after the bash scenario — the goblin's staggered turn must
/// never land, so the player keeps the base 20 baseline.
const PLAYER_BASELINE_HP: i32 = 20;

#[test]
fn shield_bash_output_is_deterministic_per_seed() {
    let a = run_bash_scenario();
    let b = run_bash_scenario();
    assert_eq!(a.log, b.log, "event log deterministic for the same seed");
    assert_eq!(a.goblin_hp, b.goblin_hp, "goblin HP deterministic");
    assert_eq!(a.player_charges, b.player_charges, "charges deterministic");
    assert_eq!(a.skip_after, b.skip_after, "skip consumption deterministic");
    assert_eq!(
        a.script_lines, b.script_lines,
        "script assertions deterministic"
    );
}

// ---------------------------------------------------------------------------
// 2. The named --headless event-log mapping
// ---------------------------------------------------------------------------

#[test]
fn headless_run_logs_use_ability_then_damage_then_status_in_order() {
    let (world, tiles, broker, bridge) = load_content_world(Path::new(MODS));
    let mut b = broker.write().unwrap();
    let player = spawn_at(&world, &mut b, "Player", 2, 2);
    let goblin = spawn_at(&world, &mut b, "Goblin", 7, 5);
    drop(b);

    let mut sim = Sim::from_shared(
        broker.clone(),
        Some(bridge),
        demo_zone(),
        player,
        Some(goblin),
    );
    // The headless script: right, right, down, down, wait, bash — the Bash
    // step exercises the ability, so the log carries the event mapping.
    let _map = sim.run_headless(&tiles, 6);
    let log = sim.log().to_vec();
    assert_bash_event_order(&log);
    // The bash itself resolved natively: the goblin is staggered.
    assert!(
        log.iter().any(|l| {
            l.starts_with("EVENT ApplyStatus ") && l.contains("status=Status_Staggered")
        }),
        "{log:?}"
    );
}

// ---------------------------------------------------------------------------
// 3. Hot reload: only new applications use the edited status duration
// ---------------------------------------------------------------------------

const STAGGER_100: &str = r##"
[Status]
parts.Status = { name = "Status", duration = 1, stacking = "refresh" }

[Status_Staggered]
inherits = "Status"
parts.Status    = { name = "Staggered", duration = 100, stacking = "refresh" }
parts.Modifiers = { skip_turns = 1 }
"##;

const STAGGER_250: &str = r##"
[Status]
parts.Status = { name = "Status", duration = 1, stacking = "refresh" }

[Status_Staggered]
inherits = "Status"
parts.Status    = { name = "Staggered", duration = 250, stacking = "refresh" }
parts.Modifiers = { skip_turns = 1 }
"##;

fn spawn_ctx_entity(ctx: &mut SimContext, x: i32, y: i32) -> hecs::Entity {
    let e = ctx
        .broker
        .write()
        .unwrap_or_else(|p| p.into_inner())
        .spawn();
    ctx.set_position(e, crate_position(x, y));
    e
}

fn crate_position(x: i32, y: i32) -> Position {
    Position { x, y }
}

#[test]
fn hot_reload_only_new_applications_use_the_edited_status_duration() {
    // A world + effect runtime over the v1 fixture (duration 100).
    let world = World::new();
    let v1 = MemVfs::new().with("blueprints/statuses.toml", STAGGER_100);
    world.load(&v1).expect("v1 load");
    let mut ctx = SimContext::new(EntityBroker::new(), demo_zone(), SimRng::from_seed(7));
    tessera_core::effects::install_native_effects(&mut ctx, world.library().clone());

    let a = spawn_ctx_entity(&mut ctx, 5, 5);
    let b = spawn_ctx_entity(&mut ctx, 6, 5);

    // Applying before the reload snapshots the authored duration.
    ctx.dispatch(ApplyStatus {
        target: a,
        status: "Status_Staggered".into(),
    })
    .expect("apply v1");
    assert_eq!(stagger_duration_ctx(&ctx, a), 100, "v1 application");
    assert_eq!(stagger_skip_ctx(&ctx, a), 1, "v1 carries the skip credit");

    // Reload: the engine's in-place library swap (the same Arc the handlers
    // captured), now carrying the edited duration 250.
    let v2 = MemVfs::new().with("blueprints/statuses.toml", STAGGER_250);
    world.load(&v2).expect("v2 reload");

    // A *new* application reads the reloaded definition...
    ctx.dispatch(ApplyStatus {
        target: b,
        status: "Status_Staggered".into(),
    })
    .expect("apply v2");
    assert_eq!(
        stagger_duration_ctx(&ctx, b),
        250,
        "new application uses the edited duration"
    );

    // ...while the in-flight instance keeps its snapshot (only new
    // applications see the reload).
    assert_eq!(
        stagger_duration_ctx(&ctx, a),
        100,
        "in-flight instances keep their applied duration"
    );

    // A reapplication of the reloaded status on the old entity refreshes to
    // the new duration — the refresh is itself a new application.
    ctx.dispatch(ApplyStatus {
        target: a,
        status: "Status_Staggered".into(),
    })
    .expect("refresh with v2");
    assert_eq!(
        stagger_duration_ctx(&ctx, a),
        250,
        "refreshed application uses the reloaded definition"
    );
    assert_eq!(stagger_skip_ctx(&ctx, a), 1);
}

fn stagger_duration_ctx(ctx: &SimContext, entity: hecs::Entity) -> i64 {
    ctx.broker
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .typed_part::<StatusEffects>(entity, &PartTypeId::new(PART_STATUS_EFFECTS))
        .expect("status store")
        .instances("Status_Staggered")[0]
        .duration_remaining
}

fn stagger_skip_ctx(ctx: &SimContext, entity: hecs::Entity) -> i64 {
    ctx.broker
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .typed_part::<StatusEffects>(entity, &PartTypeId::new(PART_STATUS_EFFECTS))
        .expect("status store")
        .skip_turns("Status_Staggered")
}
