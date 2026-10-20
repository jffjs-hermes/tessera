//! Headless save/load smoke: snapshot hooks + `Sim::save`/`Sim::load`.
//!
//! Builds a populated world (player + scheduled goblin carrying a `Charge`
//! script part), plays a deterministic prefix, saves it (both through the
//! in-memory bytes API and the atomic file layer), reloads into a fresh
//! [`Sim`], and continues BOTH worlds with the same action script to show the
//! loaded world produces an *identical* continued event log.
//!
//! Run with:
//! ```sh
//! cargo run -p tessera --example snapshot_smoke
//! ```

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use hecs::Entity;
use tessera::sim::{PlayerAction, Sim};
use tessera_core::ecs::{EntityBroker, Part, PartRegistry};
use tessera_core::events::{Health, PART_HEALTH, PART_POSITION, Point, Position};
use tessera_core::save::{SaveHooks, build_save_hooks, load, save_to_bytes, write_atomic};
use tessera_core::script_part::{FieldDef, PartValue, ScriptPart, ScriptPartType};
use tessera_core::turns::{ActorEnergy, PART_ENERGY};
use tessera_core::zone::Zone;

/// A demo room: bordered all-floor grid (the pillar arrangement doesn't matter
/// for this smoke — the border walls guarantee a wall-blocked move).
fn demo_zone(width: usize, height: usize) -> Zone {
    let mut cells = vec!["terrain.floor".to_string(); width * height];
    for y in 0..height {
        for x in 0..width {
            let edge = x == 0 || y == 0 || x == width - 1 || y == height - 1;
            if edge {
                cells[y * width + x] = "terrain.wall".to_string();
            }
        }
    }
    Zone::from_cells("start", width, height, Point::new(1, 1), cells)
}

/// The whitelist hooks mirroring the shipped demo content. `build_save_hooks`
/// only uses the registry to identify script-part names (the native
/// Position / Health / ActorEnergy / Stats hooks are registered
/// unconditionally), so this registers just the `Charge` script part.
fn hooks_and_registry() -> (SaveHooks, PartRegistry) {
    let mut registry = PartRegistry::new();
    let mut schema = BTreeMap::new();
    schema.insert(
        "charges".to_string(),
        FieldDef::int().default(PartValue::Int(3)),
    );
    registry
        .register_script_part(ScriptPartType::new("Charge", schema))
        .unwrap();
    let hooks = build_save_hooks(&registry).0;
    (hooks, registry)
}

/// Build a populated world: player + a scheduled goblin carrying a `Charge`
/// script part. The goblin acts (native deterministic default: attacks the
/// player) on its own scheduled turns.
fn build_populated() -> (Sim, Entity, Entity) {
    let mut broker = EntityBroker::new();
    let player = broker.spawn();
    broker
        .insert_part(player, PART_ENERGY, Box::new(ActorEnergy::new(100)))
        .unwrap();
    broker
        .insert_part(
            player,
            PART_POSITION,
            Box::new(Position { x: 2, y: 2 }) as Box<dyn Part>,
        )
        .unwrap();
    broker
        .insert_part(
            player,
            PART_HEALTH,
            Box::new(Health { hp: 20 }) as Box<dyn Part>,
        )
        .unwrap();
    let goblin = broker.spawn();
    broker
        .insert_part(goblin, PART_ENERGY, Box::new(ActorEnergy::new(100)))
        .unwrap();
    broker
        .insert_part(
            goblin,
            PART_POSITION,
            Box::new(Position { x: 4, y: 4 }) as Box<dyn Part>,
        )
        .unwrap();
    broker
        .insert_part(
            goblin,
            PART_HEALTH,
            Box::new(Health { hp: 3 }) as Box<dyn Part>,
        )
        .unwrap();
    let mut charge = ScriptPart::default();
    charge.insert("charges".into(), PartValue::Int(3));
    broker
        .insert_part(goblin, "Charge", Box::new(charge))
        .unwrap();

    let broker = Arc::new(RwLock::new(broker));
    let sim = Sim::from_shared(broker, None, demo_zone(20, 10), player, Some(goblin));
    (sim, player, goblin)
}

/// A deterministic action script (a mix of handled moves, waits, and a
/// wall-blocked move) so the smoke exercises ordinary behavior AND the
/// cancellation path.
fn script(i: usize) -> PlayerAction {
    match i % 5 {
        0 => PlayerAction::Move { dx: 1, dy: 0 },
        1 => PlayerAction::Wait,
        2 => PlayerAction::Move { dx: 0, dy: 1 },
        3 => PlayerAction::Wait,
        _ => PlayerAction::Move { dx: -1, dy: 0 }, // wall-blocked
    }
}

/// A unique temp directory under the system temp dir (std only — no extra
/// dev-dependency needed by the demo).
fn temp_save_dir(tag: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir();
    let uniq = format!(
        "tessera-snapshot-smoke-{tag}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos()
    );
    let dir = base.join(uniq);
    std::fs::create_dir_all(&dir).expect("create temp save dir");
    dir
}

fn main() {
    // 1. Build a populated world and play a deterministic prefix.
    let (mut sim, _player, _goblin) = build_populated();
    for i in 0..12 {
        sim.submit_action(script(i)).expect("turn resolves");
    }
    println!(
        "== saved world: played 12 actions, log has {} lines ==",
        sim.log().len()
    );

    // 2. Save: via the in-memory bytes API and the atomic file layer.
    let (hooks, _registry) = hooks_and_registry();
    let save_data = sim.save(&hooks).expect("snapshot world to SaveData");
    let bytes = save_to_bytes(&save_data).expect("encode bytes");
    println!(
        "== SaveData: zone={} entities={} clock={} actors={} size={} bytes ==",
        save_data.zone_id,
        save_data.entities.len(),
        save_data.clock,
        save_data.actors.len(),
        bytes.len()
    );

    let dir = temp_save_dir("main");
    let save_path = dir.join("quicksave");
    write_atomic(&save_path, &save_data).expect("write save to disk");
    let reloaded = load(&save_path).expect("reload save from disk");
    assert_eq!(reloaded, save_data, "file round-trip equals in-memory save");
    println!(
        "== wrote + reloaded {} (atomic file layer round-trip equal) ==",
        save_path.display()
    );

    // 3. Reload into a fresh sim and continue BOTH worlds with the same script.
    let mut reference = {
        let (mut r, _p, _g) = build_populated();
        for i in 0..12 {
            r.submit_action(script(i)).expect("turn resolves");
        }
        r
    };
    let mut loaded =
        Sim::load(save_data, demo_zone(20, 10), &hooks, None).expect("reload sim from SaveData");
    assert_eq!(
        loaded.position(),
        reference.position(),
        "loaded player position equals the saved world's"
    );
    assert_eq!(
        loaded.player_turns_public(),
        reference.player_turns_public(),
        "loaded player-turn clock equals the saved world's"
    );

    let continued_at = reference.log().len();
    for i in 0..10 {
        // Both worlds are at the save point, so they continue with the same
        // logical action sequence (the loaded world just starts from an empty
        // log while the reference carries its pre-save history).
        reference.submit_action(script(12 + i)).unwrap();
        loaded.submit_action(script(12 + i)).unwrap();
    }

    let continued = &reference.log()[continued_at..];
    let from_loaded = loaded.log();
    println!("\n== continuation: 10 more actions in each world ==");
    for (a, b) in continued.iter().zip(from_loaded.iter()) {
        assert_eq!(a, b, "identical continued event line");
        println!("  {a}");
    }
    assert_eq!(
        continued, from_loaded,
        "loaded world continues with an identical event log"
    );
    println!(
        "\nOK: reloaded world continued with {} identical event-log lines.",
        from_loaded.len()
    );
    println!("Snapshot demo passed (SaveData eq, atomic file round-trip, identical continuation).");
}
