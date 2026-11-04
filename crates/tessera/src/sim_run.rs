//! Headless batch runner: scenario + build TOMLs, seeded runs, JSON output.
//!
//! The runner is plumbing only. It loads content, spawns a scenario's actors,
//! drives the energy clock with each actor's own policy script, and reports the
//! phase-1 metrics. Every rule that decides what an actor *does* lives in a
//! Luau policy script (`sim/policies/`), and every combat number lives in the
//! rules script and the content tuning — nothing here computes a formula.
//!
//! Determinism: a fresh broker + sim is built per run, seeded from the run's
//! own named stream derived from the master seed, and all metrics are integers.
//! The output of a run is therefore a pure function of (seed, scenario, build,
//! content).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use hecs::Entity;
use serde::Deserialize;
use tessera_core::ecs::EntityBroker;
use tessera_core::events::{Health, PART_HEALTH, PART_POSITION, Position};
use tessera_core::rng::SimRng;
use tessera_core::tiles::TileRegistry;
use tessera_core::vfs::{DirVfs, Vfs};
use tessera_core::world::World;
use tessera_core::zone::{WorldMap, load_zones};

use crate::Sim;
use crate::mods::{Overlay, apply_mod_patches, discover_sources};
use crate::scripting::ScriptBridge;

/// The prototype's stalemate cap: 120 game turns = 12 000 game-time units.
pub const MAX_TURNS: i64 = 120;
/// Game-time units per turn (the scheduler clock's units).
pub const TURN_UNITS: i64 = 100;

/// Content loaded once and reused by every run in a batch: the composed
/// filesystem overlay, its blueprint world, the script sources, and the zones.
pub struct SimPack {
    data: PathBuf,
    registry: tessera_mods::ModRegistry,
    world: Arc<World>,
    /// Script sources in deterministic load order (`path`, `source`).
    scripts: Vec<(String, String)>,
    zones: WorldMap,
}

impl SimPack {
    /// Load base data + mods from `data` / `mods` (the same overlay the demo
    /// binary builds, minus the render-only atlas).
    pub fn load(data: &Path, mods: &Path) -> anyhow::Result<SimPack> {
        let base = DirVfs::new(data);
        let sources = if mods.is_dir() {
            discover_sources(mods)?
        } else {
            Vec::new()
        };
        let registry = match tessera_mods::ModRegistry::build(sources) {
            Ok(reg) => reg,
            Err(errors) => {
                let joined = errors.iter().map(|e| e.to_string()).collect::<Vec<_>>();
                anyhow::bail!("mod loading errors: {}", joined.join("; "));
            }
        };
        let overlay = Overlay::new(Box::new(base), &registry);
        let tiles = TileRegistry::load(&overlay)?;
        let broker = Arc::new(RwLock::new(EntityBroker::new()));
        let bridge = ScriptBridge::new(broker.clone());
        let world = build_world(&overlay, &bridge)?;
        apply_mod_patches(&world, &registry)
            .map_err(|e| anyhow::anyhow!("applying mod patches: {e}"))?;
        let zones = load_zones(&overlay, &tiles)?;
        let mut scripts = Vec::new();
        for path in overlay.list("scripts").unwrap_or_default() {
            if path.ends_with(".luau") {
                let source = overlay.read_to_string(&path)?;
                scripts.push((path, source));
            }
        }
        drop(overlay);
        Ok(SimPack {
            data: data.to_path_buf(),
            registry,
            world: Arc::new(world),
            scripts,
            zones,
        })
    }

    /// The loaded zones, by id.
    pub fn zones(&self) -> &WorldMap {
        &self.zones
    }

    /// The blueprint world (content inspection for tests and reporting).
    pub fn world(&self) -> Arc<World> {
        self.world.clone()
    }

    /// Read any composed content file (scenario, build, policy).
    pub fn read_text(&self, path: &str) -> anyhow::Result<String> {
        let base = DirVfs::new(&self.data);
        let overlay = Overlay::new(Box::new(base), &self.registry);
        Ok(overlay.read_to_string(path)?)
    }

    /// Parse a scenario file out of the composed content tree.
    pub fn scenario(&self, path: &str) -> anyhow::Result<Scenario> {
        let text = self.read_text(path)?;
        Ok(ScenarioSpec::from_toml(&text)?.resolve())
    }

    /// Parse a build file out of the composed content tree.
    pub fn build(&self, path: &str) -> anyhow::Result<BuildSpec> {
        let text = self.read_text(path)?;
        BuildSpec::from_toml(&text)
    }
}

fn build_world(vfs: &dyn Vfs, bridge: &ScriptBridge) -> anyhow::Result<World> {
    use tessera_core::world::RenderTile;
    let mut world = World::new();
    world.register_component::<RenderTile>("Render")?;
    world.register_part::<Position>(PART_POSITION)?;
    world.register_part::<tessera_core::turns::ActorEnergy>(tessera_core::turns::PART_ENERGY)?;
    world.register_part::<Health>(PART_HEALTH)?;
    for path in vfs.list("scripts").unwrap_or_default() {
        if !path.ends_with(".luau") {
            continue;
        }
        let source = vfs.read_to_string(&path)?;
        bridge.load(&path, &path, &source)?;
    }
    bridge.sync_schemas(&mut world)?;
    world.load(vfs)?;
    Ok(world)
}

/// The scenario file's shape.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioSpec {
    pub scenario: ScenarioHead,
    #[serde(default)]
    pub enemies: Vec<EnemyGroup>,
    #[serde(default)]
    pub band: BTreeMap<String, Vec<i64>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioHead {
    pub id: String,
    pub map: String,
    #[serde(default = "default_turn_cap")]
    pub turn_cap: i64,
    pub player_spawn: [i32; 2],
    #[serde(default)]
    pub start_alertness: Option<String>,
}

fn default_turn_cap() -> i64 {
    MAX_TURNS * TURN_UNITS
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnemyGroup {
    pub blueprint: String,
    pub spawn_cells: Vec<[i32; 2]>,
}

/// A resolved scenario: the file's head plus its band table.
#[derive(Debug, Clone)]
pub struct Scenario {
    pub id: String,
    pub map: String,
    pub turn_cap: i64,
    pub player_spawn: (i32, i32),
    pub enemies: Vec<EnemyGroup>,
    pub band: BTreeMap<String, Vec<i64>>,
}

impl ScenarioSpec {
    pub fn from_toml(text: &str) -> anyhow::Result<ScenarioSpec> {
        Ok(toml::from_str(text)?)
    }

    pub fn resolve(self) -> Scenario {
        Scenario {
            id: self.scenario.id,
            map: self.scenario.map,
            turn_cap: self.scenario.turn_cap,
            player_spawn: (self.scenario.player_spawn[0], self.scenario.player_spawn[1]),
            enemies: self.enemies,
            band: self.band,
        }
    }
}

/// The build file's shape.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildSpec {
    pub build: BuildHead,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildHead {
    pub id: String,
    pub blueprint: String,
    /// VFS path of the policy script this build runs (e.g.
    /// `sim/policies/knight_expert.luau`).
    pub policy: String,
}

impl BuildSpec {
    pub fn from_toml(text: &str) -> anyhow::Result<BuildSpec> {
        Ok(toml::from_str(text)?)
    }
}

/// One run's metric row (the phase-1 subset). Integer-only by construction:
/// no float ever enters an outcome, so the JSON is byte-stable across
/// platforms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunResult {
    pub run: u32,
    pub win: bool,
    pub turns: i64,
    pub hp_left_pct: i64,
    pub damage_dealt: i64,
    pub damage_taken: i64,
    pub kills: i64,
    pub stalemate: bool,
}

impl RunResult {
    /// One JSON object (no JSON dependency: the payload is flat and
    /// integer/bool only, so a hand-written line cannot drift from the shape).
    pub fn to_json(&self) -> String {
        format!(
            "{{\"run\":{},\"win\":{},\"turns\":{},\"hp_left_pct\":{},\
             \"damage_dealt\":{},\"damage_taken\":{},\"kills\":{},\"stalemate\":{}}}",
            self.run,
            self.win,
            self.turns,
            self.hp_left_pct,
            self.damage_dealt,
            self.damage_taken,
            self.kills,
            self.stalemate
        )
    }
}

/// The run seed for `run`: the master seed forked along the run's own named
/// stream label, so a run is reproducible alone and adding runs never changes
/// an existing run's sequence.
pub fn run_seed(master: u64, scenario: &str, build: &str, run: u32) -> u64 {
    let mut stream =
        SimRng::from_seed(master).fork(label_hash(&format!("sim:{scenario}:{build}:{run}")));
    (u64::from(stream.next_u32()) << 32) | u64::from(stream.next_u32())
}

/// The same stable string hash the engine uses for named streams.
fn label_hash(label: &str) -> u64 {
    let mut acc: u64 = 0xcbf2_9ce4_8422_2325;
    for b in label.bytes() {
        acc ^= b as u64;
        acc = acc.wrapping_mul(0x0000_0100_0000_01b3);
    }
    acc
}

/// Play one run of `scenario` under `build` with the given seed.
pub fn run_one(
    pack: &SimPack,
    scenario: &Scenario,
    build: &BuildSpec,
    run: u32,
    seed: u64,
) -> anyhow::Result<RunResult> {
    let zone =
        pack.zones.get(&scenario.map).cloned().ok_or_else(|| {
            anyhow::anyhow!("scenario {}: unknown map {}", scenario.id, scenario.map)
        })?;
    // The build's policy script is selected explicitly (it is not part of the
    // auto-loaded `scripts/` tree, so two policies never both act in one run).
    let policy = pack.read_text(&build.build.policy).map_err(|e| {
        anyhow::anyhow!(
            "build {}: reading policy {}: {e}",
            build.build.id,
            build.build.policy
        )
    })?;

    let broker = Arc::new(RwLock::new(EntityBroker::new()));
    let bridge = ScriptBridge::new(broker.clone());
    // The world-state singleton is the policy anchor scripts read through
    // `engine.world_state()`; it exists before any spawn so its id is stable.
    let world_state = {
        let mut b = broker.write().unwrap_or_else(|p| p.into_inner());
        b.spawn()
    };
    let world = pack.world.clone();
    bridge.set_world(world.clone());
    bridge.set_world_state_entity(world_state);
    for (path, source) in &pack.scripts {
        bridge
            .load(path, path, source)
            .map_err(|e| anyhow::anyhow!("loading script {path}: {e}"))?;
    }
    bridge
        .load(&build.build.policy, &build.build.policy, &policy)
        .map_err(|e| anyhow::anyhow!("loading policy {}: {e}", build.build.policy))?;

    let spawn = |blueprint: &str, at: (i32, i32)| -> anyhow::Result<Entity> {
        let mut b = broker.write().unwrap_or_else(|p| p.into_inner());
        let e = world
            .spawn_blueprint(blueprint, &mut b)
            .map_err(|e| anyhow::anyhow!("spawning {blueprint:?}: {e}"))?;
        b.insert_part(
            e,
            PART_POSITION,
            Box::new(Position::from_point(tessera_core::events::Point::new(
                at.0, at.1,
            ))) as Box<dyn tessera_core::ecs::Part>,
        )?;
        Ok(e)
    };
    let player = spawn(&build.build.blueprint, scenario.player_spawn)?;
    let mut enemies: Vec<Entity> = Vec::new();
    for group in &scenario.enemies {
        for cell in &group.spawn_cells {
            enemies.push(spawn(&group.blueprint, (cell[0], cell[1]))?);
        }
    }

    let mut sim = Sim::from_shared(
        broker.clone(),
        Some(bridge.clone()),
        zone.clone(),
        player,
        None,
    );
    sim.reseed(seed);
    for e in &enemies {
        sim.schedule_actor(*e).map_err(|e| anyhow::anyhow!("{e}"))?;
    }

    let (player_max, enemy_max) = {
        let b = broker.read().unwrap_or_else(|p| p.into_inner());
        let pm = hp_of(&b, player).unwrap_or(0);
        let em: i64 = enemies.iter().filter_map(|e| hp_of(&b, *e)).sum();
        (pm, em)
    };

    // Play until a scenario end condition or the stalemate cap. The cap is the
    // prototype's `max_turns`, which binds before the scenario's headroom cap.
    let cap_ticks = MAX_TURNS * TURN_UNITS;
    let mut stalemate = false;
    let mut win = false;
    loop {
        let b = broker.read().unwrap_or_else(|p| p.into_inner());
        let player_dead = hp_of(&b, player).map(|h| h <= 0).unwrap_or(true);
        // A raider that died leaves the map; "alive" is therefore "still in
        // the world with hp > 0", and a cleared room is the win condition.
        let alive = enemies
            .iter()
            .filter(|e| hp_of(&b, **e).map(|h| h > 0).unwrap_or(false))
            .count();
        drop(b);
        if alive == 0 {
            win = true;
            break;
        }
        if player_dead {
            break;
        }
        if sim.ticks() as i64 >= cap_ticks {
            stalemate = true;
            break;
        }
        sim.policy_turn()?;
    }

    let b = broker.read().unwrap_or_else(|p| p.into_inner());
    let hp_now = hp_of(&b, player).unwrap_or(0);
    let enemy_hp_now: i64 = enemies.iter().filter_map(|e| hp_of(&b, *e)).sum();
    let kills = enemies
        .iter()
        .filter(|e| hp_of(&b, **e).map(|h| h <= 0).unwrap_or(true))
        .count() as i64;
    let ticks = sim.ticks() as i64;
    Ok(RunResult {
        run,
        win,
        turns: ticks / TURN_UNITS,
        hp_left_pct: if player_max > 0 {
            (hp_now.max(0) * 100) / player_max
        } else {
            0
        },
        damage_dealt: (enemy_max - enemy_hp_now).max(0),
        damage_taken: (player_max - hp_now).max(0),
        kills,
        stalemate,
    })
}

fn hp_of(broker: &EntityBroker, entity: Entity) -> Option<i64> {
    broker
        .typed_part::<Health>(entity, &PART_HEALTH.into())
        .map(|h| h.hp as i64)
}

/// Run `scenario` under `build` for `runs` runs at `master_seed`.
pub fn run_batch(
    pack: &SimPack,
    scenario: &Scenario,
    build: &BuildSpec,
    runs: u32,
    master_seed: u64,
) -> anyhow::Result<Vec<RunResult>> {
    let mut out = Vec::with_capacity(runs as usize);
    for run in 0..runs {
        let seed = run_seed(master_seed, &scenario.id, &build.build.id, run);
        out.push(run_one(pack, scenario, build, run, seed)?);
    }
    Ok(out)
}

/// The phase-1 report: one JSON line per run plus the summary line.
pub fn report_json(results: &[RunResult], summary: &SimSummary, policy: &str, seed: u64) -> String {
    let mut text = String::new();
    for r in results {
        text.push_str(&r.to_json());
        text.push('\n');
    }
    text.push_str(&summary.to_json());
    text.push_str(&format!(
        "\n{{\"policy\":\"{}\",\"seed\":{}}}",
        policy, seed
    ));
    text
}

/// Aggregate a batch: the summary appended after the per-run rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimSummary {
    pub runs: u32,
    pub wins: u32,
    pub win_rate_pct: i64,
    pub mean_turns: i64,
    pub median_turns: i64,
    pub mean_hp_left_pct: i64,
}

impl SimSummary {
    pub fn from_runs(results: &[RunResult]) -> SimSummary {
        let runs = results.len() as u32;
        let wins = results.iter().filter(|r| r.win).count() as u32;
        let mut turns: Vec<i64> = results.iter().map(|r| r.turns).collect();
        turns.sort_unstable();
        let median_turns = if turns.is_empty() {
            0
        } else {
            turns[turns.len() / 2]
        };
        let mean_turns = if turns.is_empty() {
            0
        } else {
            turns.iter().sum::<i64>() / turns.len() as i64
        };
        let wins_with_hp: Vec<i64> = results
            .iter()
            .filter(|r| r.win)
            .map(|r| r.hp_left_pct)
            .collect();
        let mean_hp_left_pct = if wins_with_hp.is_empty() {
            0
        } else {
            wins_with_hp.iter().sum::<i64>() / wins_with_hp.len() as i64
        };
        SimSummary {
            runs,
            wins,
            win_rate_pct: if runs == 0 {
                0
            } else {
                (wins as i64 * 100) / runs as i64
            },
            mean_turns,
            median_turns,
            mean_hp_left_pct,
        }
    }

    pub fn to_json(&self) -> String {
        format!(
            "{{\"summary\":true,\"runs\":{},\"wins\":{},\"win_rate\":{},\
             \"mean_turns\":{},\"median_turns\":{},\"mean_hp_left_pct\":{}}}",
            self.runs,
            self.wins,
            self.win_rate_pct,
            self.mean_turns,
            self.median_turns,
            self.mean_hp_left_pct
        )
    }
}
