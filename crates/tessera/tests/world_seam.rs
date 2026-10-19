//! Integration tests for the world-slice script seam, driven through the real
//! sim + script bridge against the base data pack:
//!   - the native `TakeDamage` applicator emits the reserved `Death` event
//!     exactly once when hp clamps to zero (a second hit on a dead entity does
//!     not re-emit), observable by a scripted `engine.on("Death")` handler;
//!   - `ActorTurnStart` is delivered to scripts each actor turn;
//!   - `engine.spawn_at` registers a blueprint-spawned, `ActorEnergy`-carrying
//!     actor with the scheduler (so it becomes scheduled);
//!   - `player_turns` increments once per submitted player action including
//!     cancelled (wall-blocked) ones;
//!   - the Player blueprint's own `player_respawn.luau` script performs the
//!     player's death/respawn (fresh `Player` at the start tile) — not the
//!     goblin's script.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use tessera::scripting::ScriptBridge;
use tessera::sim::{PlayerAction, Sim};
use tessera_core::ecs::EntityBroker;
use tessera_core::events::Point;
use tessera_core::vfs::DirVfs;
use tessera_core::world::{RenderTile, World};
use tessera_core::zone::Zone;

const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/base");
const PLAYER_BLUEPRINT: &str = "Player";
const GOBLIN_BLUEPRINT: &str = "Goblin";

/// Build the world exactly as the binary does (engine parts registered, the
/// given scripts loaded, schemas synced, then blueprints loaded).
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

    // Load the shipped `zapper.luau`? No — tests load their own scripts on the
    // bridge; only the part schemas are mirrored here.
    bridge.sync_schemas(&mut world).expect("sync schemas");
    world.load(vfs).expect("load blueprints");
    world
}

fn demo_zone(width: usize, height: usize) -> Zone {
    let mut cells = vec!["terrain.floor".to_string(); width * height];
    for y in 0..height {
        for x in 0..width {
            if x == 0 || y == 0 || x == width - 1 || y == height - 1 {
                cells[y * width + x] = "terrain.wall".to_string();
            }
        }
    }
    Zone::from_cells("test", width, height, Point::new(1, 1), cells)
}

/// Build the shared broker + bridge, spawn the player and a goblin (both
/// scheduled, the goblin adjacent to the player so combat is immediate), and
/// return the sim. `script_source`, when given, is loaded on the bridge
/// before the sim is built so its handlers are live for the run.
fn base_sim(script_source: Option<&str>, enemy: bool) -> (Sim, Arc<RwLock<EntityBroker>>) {
    let vfs = DirVfs::new(PathBuf::from(DATA));
    let broker: Arc<RwLock<EntityBroker>> = Arc::new(RwLock::new(EntityBroker::new()));
    let bridge = ScriptBridge::new(broker.clone());
    // Register the `Charge` script schema the shipped zapper blueprint needs
    // (mirrors the binary loading `zapper.luau` before blueprints). Tests add
    // their own handlers on top.
    bridge
        .load(
            "charge-schema",
            "Bp",
            r#"-- tessera-api: 1
                engine.define_part("Charge", {
                  fields = { charges = { kind = "int", default = 1, min = 0, max = 99 } },
                })
                engine.define_part("GoblinBrain", {
                  fields = { detect = { kind = "int", default = 10, min = 1, max = 100 } },
                })
                engine.define_part("RespawnSystem", {
                  fields = {
                    due = { kind = "int", default = -1, min = -1, max = 1000000000 },
                    blueprint = { kind = "string", default = "" },
                  },
                })"#,
        )
        .expect("register charge + goblin schemas");
    if let Some(src) = script_source {
        bridge.load("worldtest", "Bp", src).expect("load script");
    }
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
        Box::new(tessera_core::events::Position { x: 1, y: 1 }),
    )
    .unwrap();
    let goblin = if enemy {
        let goblin = world
            .spawn_blueprint(GOBLIN_BLUEPRINT, &mut b)
            .expect("spawn goblin");
        b.insert_part(
            goblin,
            tessera_core::events::PART_POSITION,
            Box::new(tessera_core::events::Position { x: 2, y: 1 }),
        )
        .unwrap();
        Some(goblin)
    } else {
        None
    };
    drop(b);

    let sim = Sim::from_shared(
        broker.clone(),
        Some(bridge),
        demo_zone(20, 10),
        player,
        goblin,
    );
    (sim, broker)
}

/// The native `TakeDamage` applicator emits `Death` exactly once when the
/// goblin's attack clamps the player's hp to zero, and a scripted
/// `engine.on("Death")` handler observes exactly one such event across many
/// more (non-reducing) hits on the already-dead player.
#[test]
fn death_emitted_once_and_delivered_to_scripts() {
    let (mut sim, _broker) = base_sim(
        Some(
            r#"-- tessera-api: 1
            engine.on("Death", engine.NORMAL, function(ev)
              engine.log(("died %d"):format(ev.target))
              return true
            end)"#,
        ),
        true,
    );

    // Drive up to 120 player Waits; the adjacent goblin attacks the player on
    // its scheduled turns (submit_action resolves enemy turns first). The
    // player's hp (20) reaches zero and the Death handler fires once; further
    // bites on the hp-0 player do not re-emit.
    for _ in 0..120 {
        sim.submit_action(PlayerAction::Wait).expect("wait");
        if sim.player_health() == 0
            && sim.log().iter().filter(|l| l.starts_with("died ")).count() >= 1
        {
            break;
        }
    }
    assert_eq!(
        sim.player_health(),
        0,
        "player must be dead after goblin bites"
    );

    let died: Vec<&String> = sim
        .log()
        .iter()
        .filter(|l| l.starts_with("died "))
        .collect();
    assert_eq!(
        died.len(),
        1,
        "Death must be emitted exactly once, got {:?} (log={:?})",
        died,
        sim.log()
    );

    // A handful more bites on the dead player must not re-emit Death.
    for _ in 0..10 {
        sim.submit_action(PlayerAction::Wait).expect("wait");
    }
    let died_after: Vec<&String> = sim
        .log()
        .iter()
        .filter(|l| l.starts_with("died "))
        .collect();
    assert_eq!(
        died_after.len(),
        1,
        "no Death re-emission on a dead entity, got {:?}",
        died_after
    );
}

/// The Player blueprint's own `player_respawn.luau` script performs the
/// player's death/respawn. With it loaded on the bridge (and no goblin script),
/// the adjacent goblin's native fallback attack still kills the player; the
/// script then respawns a fresh `Player` at the start tile and hp climbs back
/// to the full baseline. This pins that the player policy lives in the
/// Player-owned script, not `goblin_ai.luau`.
#[test]
fn player_respawn_script_respawns_player_on_death() {
    let src = std::fs::read_to_string(format!("{DATA}/scripts/player_respawn.luau"))
        .expect("read player_respawn.luau");
    let (mut sim, _broker) = base_sim(Some(&src), true);

    let mut saw_damage = false;
    let mut saw_respawn = false;
    for _ in 0..200 {
        sim.submit_action(PlayerAction::Wait).unwrap();
        let hp = sim.player_health();
        if hp < 20 {
            saw_damage = true;
        }
        // A jump back up to the full 20 baseline after damage = the fresh
        // Player respawned at full health via the player's script.
        if saw_damage && hp == 20 {
            saw_respawn = true;
            break;
        }
    }
    assert!(saw_damage, "the adjacent goblin must damage the player");
    assert!(
        saw_respawn,
        "player_respawn.luau must respawn the player to full hp (final hp {})",
        sim.player_health()
    );
    assert!(
        sim.log().iter().any(|l| l.contains("player respawned at")),
        "player_respawn.luau must log the respawn, got {:?}",
        sim.log()
    );
}

/// `ActorTurnStart` is delivered to a scripted handler each actor turn, with
/// the acting entity id as `ev.actor`.
#[test]
fn actor_turn_start_is_delivered_to_scripts() {
    let (mut sim, _broker) = base_sim(
        Some(
            r#"-- tessera-api: 1
            engine.on("ActorTurnStart", engine.NORMAL, function(ev)
              engine.log(("turn_start %d"):format(ev.actor))
              return true
            end)"#,
        ),
        false,
    );

    sim.submit_action(PlayerAction::Wait).expect("wait");
    sim.submit_action(PlayerAction::Wait).expect("wait");

    let turns: Vec<&String> = sim
        .log()
        .iter()
        .filter(|l| l.starts_with("turn_start "))
        .collect();
    assert!(
        turns.len() >= 2,
        "ActorTurnStart must be delivered every actor turn, got {:?} (log={:?})",
        turns,
        sim.log()
    );
}

/// `engine.spawn_at("Goblin", x, y)` reserves an entity at the script-facing
/// id, stamps the blueprint, places `Position`, and — because `Goblin` carries
/// `ActorEnergy` — registers it with the scheduler.
#[test]
fn spawn_at_registers_blueprint_position_and_scheduler() {
    use tessera_core::turns::PART_ENERGY;
    let (mut sim, broker) = base_sim(
        Some(
            r#"-- tessera-api: 1
            local spawned = false
            engine.on("ActorTurnStart", engine.NORMAL, function(ev)
              if not spawned then
                spawned = true
                local id = engine.spawn_at("Goblin", 5, 5)
                engine.log(("spawned_at %d"):format(id))
              end
              return true
            end)"#,
        ),
        false,
    );

    // First player turn triggers an ActorTurnStart -> one spawn_at.
    sim.submit_action(PlayerAction::Wait).expect("wait");

    let spawned_lines: Vec<&String> = sim
        .log()
        .iter()
        .filter(|l| l.starts_with("spawned_at "))
        .collect();
    assert_eq!(
        spawned_lines.len(),
        1,
        "one spawn_at per turn: {:?}",
        sim.log()
    );

    // The spawned entity: live under the script-facing id, with a Position part
    // at (5,5) and ActorEnergy in the scheduler.
    let id: u64 = spawned_lines[0]["spawned_at ".len()..]
        .trim()
        .parse()
        .unwrap();
    let entity = sim
        .resolve_script_entity(id)
        .expect("script id must resolve to a live entity");
    let b = broker.read().unwrap();
    assert!(b.is_alive(entity), "spawn_at entity must be live");
    let pos = b
        .typed_part::<tessera_core::events::Position>(
            entity,
            &tessera_core::ecs::PartTypeId::new(tessera_core::events::PART_POSITION),
        )
        .expect("spawned entity has Position");
    assert_eq!((pos.x, pos.y), (5, 5));
    assert!(
        b.typed_part::<tessera_core::turns::ActorEnergy>(
            entity,
            &tessera_core::ecs::PartTypeId::new(PART_ENERGY),
        )
        .is_some(),
        "spawned Goblin carries ActorEnergy"
    );
    drop(b);

    // Scheduler registration: the sim's shared scheduler now contains the new
    // actor (it becomes scheduled).
    assert!(
        sim.has_scheduled(entity),
        "spawn_at actor must be registered with the scheduler"
    );
}

/// `player_turns` counts once per submitted player action — including a
/// cancelled (wall-blocked) move — and the count is readable by scripts.
#[test]
fn player_turns_increments_even_on_cancelled_actions() {
    let (mut sim, _broker) = base_sim(
        Some(
            r#"-- tessera-api: 1
            engine.on("ActorTurnStart", engine.NORMAL, function(ev)
              engine.log(("ptur %d"):format(engine.player_turns()))
              return true
            end)"#,
        ),
        false,
    );

    // The player starts at (1,1); moving left hits the wall (cancelled).
    sim.submit_action(PlayerAction::Move { dx: -1, dy: 0 })
        .expect("cancelled move");
    // A normal move right (handled).
    sim.submit_action(PlayerAction::Move { dx: 1, dy: 0 })
        .expect("move");
    // A wait.
    sim.submit_action(PlayerAction::Wait).expect("wait");

    // The ActorTurnStart handler observed the clock on each *player* turn:
    // exactly three submit_action calls => player_turns reached 3, and the
    // cancelled left-move still contributed (its handler saw a positive clock).
    let seen: Vec<i64> = sim
        .log()
        .iter()
        .filter_map(|l| l.strip_prefix("ptur ").map(|s| s.trim().parse().unwrap()))
        .collect();
    assert_eq!(
        seen,
        vec![1, 2, 3],
        "player_turns must count every submitted player action, got {seen:?} (log={:?})",
        sim.log()
    );
}
