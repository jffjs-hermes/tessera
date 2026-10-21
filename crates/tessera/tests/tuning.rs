//! End-to-end tuning coverage through the real stack: a fixture blueprint
//! loads a `Tuning_Combat` table, a Luau script reads `base_hit` through
//! `engine.tuning`, a valid override lands natively, and the override is
//! proven isolated from the authored content. Negative paths — unknown
//! table/key and wrong-typed values — fail closed before any simulation.
//!
//! Scope guard: this is the tuning/override layer only; no combat resolution
//! or sweep-harness behavior is exercised here.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use tessera::scripting::ScriptBridge;
use tessera_core::ecs::EntityBroker;
use tessera_core::vfs::{DirVfs, Vfs};
use tessera_core::world::{RenderTile, World};
use tessera_script::HandlerOutcome;
use tessera_script::ScriptError;

fn tmp_fixture(toml: &str) -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("blueprints")).expect("mkdir blueprints");
    std::fs::write(dir.path().join("blueprints/tuning.toml"), toml).expect("write fixture");
    dir
}

fn register_parts(world: &mut World) {
    world
        .register_component::<RenderTile>("Render")
        .expect("register Render");
}

/// Build a world over the fixture (the six content kinds, including `Tuning`,
/// are registered automatically by `World::new`), load it, and install a
/// script bridge sharing the same broker so the host can read the tuning
/// store through `SimWorldAccess::tuning_table`.
fn fixture_world(
    vfs: &dyn Vfs,
    script: &str,
) -> (Arc<World>, Arc<RwLock<EntityBroker>>, ScriptBridge) {
    let broker = Arc::new(RwLock::new(EntityBroker::new()));
    let bridge = ScriptBridge::new(broker.clone());
    let mut world = World::new();
    register_parts(&mut world);
    bridge
        .load("@fixture", "@fixture", script)
        .expect("load script");
    world.load(vfs).expect("load blueprints");
    let world = Arc::new(world);
    bridge.set_world(world.clone());
    (world, broker, bridge)
}

const COMBAT_FIXTURE: &str = r##"
[Tuning_Combat]
parts.Tuning = { base_hit = 50, ac_factor = 2, crit_percent = { pct = 150 },
                 speed_table = [50, 90, 140] }
"##;

/// A fixture loads `Tuning_Combat`; Luau reads `base_hit`; a valid override
/// is applied natively; a script read proves the override is visible; the
/// authored blueprint payload is untouched (override isolation).
#[test]
fn fixture_reads_tuning_then_override_is_isolated_from_authored() {
    let dir = tmp_fixture(COMBAT_FIXTURE);
    let vfs = DirVfs::new(PathBuf::from(dir.path()));

    let (world, _broker, bridge) = fixture_world(
        &vfs,
        r##"-- tessera-api: 1
           engine.on("Move", engine.NORMAL, function(ev)
             local t = engine.tuning("Tuning_Combat")
             engine.log(("base=%d pct=%d list=%d"):format(t.base_hit, t.crit_percent.pct, t.speed_table[1]))
             return true
           end)"##,
    );

    // First dispatch: authored values reach the script.
    {
        let mut host = bridge.host().lock().expect("host lock");
        let res = host.dispatch("Move", 1, &[]);
        assert_eq!(res.error_count(), 0, "{res:?}");
        let log = host.drain_log();
        assert!(
            log.contains(&"base=50 pct=150 list=50".to_string()),
            "{log:?}"
        );
    }

    // Apply a valid override natively: integer, percent and list each change.
    world
        .apply_tuning_override("Tuning_Combat", "base_hit", &toml_int(62))
        .expect("override int");
    world
        .apply_tuning_override("Tuning_Combat", "crit_percent", &toml_pct(175))
        .expect("override pct");
    world
        .apply_tuning_override("Tuning_Combat", "speed_table", &toml_list("[1, 2, 3]"))
        .expect("override list");

    // Second dispatch: the script now reads the overridden values.
    {
        let mut host = bridge.host().lock().expect("host lock");
        let res = host.dispatch("Move", 1, &[]);
        assert_eq!(res.error_count(), 0, "{res:?}");
        let log = host.drain_log();
        assert!(
            log.contains(&"base=62 pct=175 list=1".to_string()),
            "{log:?}"
        );
    }

    // The overrides are recorded in application order (reproducible run
    // output) and the authored blueprint payload is untouched.
    let applied = world.tuning_applied();
    assert_eq!(applied.len(), 3);
    assert_eq!(applied[0].key, "base_hit");
    assert_eq!(applied[2].key, "speed_table");

    let lib = world.library().read().expect("library lock");
    let bp = lib.get("Tuning_Combat").expect("tuning blueprint");
    let authored_hit = bp
        .part("Tuning")
        .expect("Tuning part")
        .get("base_hit")
        .expect("base_hit key")
        .as_integer();
    assert_eq!(
        authored_hit,
        Some(50),
        "authored payload must not be mutated"
    );
}

/// Negative paths fail closed *before* any simulation: an unknown table,
/// unknown key and wrong-typed value are rejected by the override layer, and
/// a script reading a missing table/key raises a script error.
#[test]
fn override_errors_and_script_misses_fail_closed() {
    let dir = tmp_fixture(COMBAT_FIXTURE);
    let vfs = DirVfs::new(PathBuf::from(dir.path()));
    let (world, _broker, bridge) = fixture_world(
        &vfs,
        r##"-- tessera-api: 1
           engine.on("Move", engine.NORMAL, function(ev)
             local t = engine.tuning("Tuning_Combat")
             local _ = t.missing_key
             return true
           end)"##,
    );

    // Native override layer: unknown table, unknown key, wrong types.
    let e = world
        .apply_tuning_override("Tuning_Nope", "base_hit", &toml_int(1))
        .unwrap_err();
    assert!(
        matches!(&e, tessera_core::tuning::TuningError::UnknownTable(t) if t == "Tuning_Nope"),
        "{e:?}"
    );
    let e = world
        .apply_tuning_override("Tuning_Combat", "bogus", &toml_int(1))
        .unwrap_err();
    assert!(
        matches!(
            &e,
            tessera_core::tuning::TuningError::UnknownKey { table, key, .. }
                if table == "Tuning_Combat" && key == "bogus"
        ),
        "{e:?}"
    );
    let e = world
        .apply_tuning_override("Tuning_Combat", "base_hit", &toml_int_list("[1]"))
        .unwrap_err();
    assert!(
        matches!(&e, tessera_core::tuning::TuningError::WrongType { authored, .. } if *authored == "integer"),
        "{e:?}"
    );
    // Nothing was recorded by the failed overrides.
    assert!(world.tuning_applied().is_empty());

    // Script side: unknown key inside the handler raises a script error.
    {
        let mut host = bridge.host().lock().expect("host lock");
        let res = host.dispatch("Move", 1, &[]);
        assert_eq!(res.error_count(), 1, "{res:?}");
        let key_err = res.outcomes.iter().any(|o| {
            matches!(
                o,
                HandlerOutcome::Errored {
                    error: ScriptError::Handler { msg, .. },
                } if msg.contains("missing_key") && msg.contains("Tuning_Combat")
            )
        });
        assert!(key_err, "{res:?}");
    }

    // An authored payload with a wrong-typed value fails *at load*, before
    // any script or simulation can see it.
    let bad = tmp_fixture("[Tuning_Combat]\nparts.Tuning = { base_hit = 50.5 }\n");
    let bad_vfs = DirVfs::new(PathBuf::from(bad.path()));
    let broker = Arc::new(RwLock::new(EntityBroker::new()));
    let bridge2 = ScriptBridge::new(broker.clone());
    let mut world2 = World::new();
    register_parts(&mut world2);
    bridge2.load("@bad", "@bad", "-- tessera-api: 1\n").ok();
    let load_err = world2.load(&bad_vfs).unwrap_err();
    assert!(load_err.to_string().contains("Tuning_Combat"), "{load_err}");
}

fn toml_int(n: i64) -> toml::Value {
    toml::Value::Integer(n)
}
fn toml_pct(n: i64) -> toml::Value {
    let text = format!("payload = {{ pct = {n} }}");
    let doc: toml::Value = toml::from_str(&text).expect("pct toml");
    doc.get("payload").expect("pct key").clone()
}
fn toml_list(text: &str) -> toml::Value {
    let anchored = format!("payload = {text}");
    let doc: toml::Value = toml::from_str(&anchored).expect("list toml");
    doc.get("payload").expect("list key").clone()
}
fn toml_int_list(text: &str) -> toml::Value {
    toml_list(text)
}
