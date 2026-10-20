//! Tessera demo: a data-driven `@` spawned from `[Player]` (a blueprint with
//! `parts.Render = { tile = "creature.player" }`), drawn from the tile registry.
//! Edit `data/base/blueprints/` while running and the blueprint library reloads
//! (new spawns use the new library; existing entities keep their parts).
//!
//! Turn-by-turn: input is routed through the
//! event/scheduler harness in [`sim::Sim`]. Arrow keys / WASD / `hjklyubn` and
//! the numpad move the `@`, `.` / Space / numpad 5 wait; a wall-blocked move is
//! cancelled by an EARLY map-collision
//! handler (no energy cost). `--headless <n>` runs `n` scripted actions with no
//! window and prints the final map + event log — the deterministic CI smoke
//! path. The renderer stays a pure reader: it never dispatches events or
//! mutates sim state; the redraw on hot reload is unchanged.

mod devtools;
mod scripting;
mod sim;

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use devtools::console::{self, ConsoleContext, ConsoleOutcome};
use notify::{RecursiveMode, Watcher};
use sim::{PlayerAction, Sim};
use tessera_core::ecs::{EntityBroker, Part};
use tessera_core::events::{Health, PART_HEALTH, PART_POSITION, Position};
use tessera_core::save::sweep_stale_tmp;
use tessera_core::tiles::TileRegistry;
use tessera_core::turns::ActorEnergy;
use tessera_core::vfs::{DirVfs, Vfs};
use tessera_core::world::{RenderTile, World};
use tessera_core::zone::{Zone, load_zones};
use tessera_render::atlas::Atlas;
use tessera_render::{Cell, Renderer};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, KeyCode, NamedKey, PhysicalKey};
use winit::window::{Window, WindowId};

use crate::scripting::ScriptBridge;

const GRID_W: u32 = 80;
const GRID_H: u32 = 50;
/// The `[Player]` blueprint id in `data/base/blueprints/creatures.toml`.
const PLAYER_BLUEPRINT: &str = "Player";

enum UserEvent {
    ContentChanged,
}

struct Args {
    data: PathBuf,
    /// Mods root (`--mods <dir>`), where local directory / zip mods are
    /// discovered. The shipped layout keeps the base pack at `data/base` and
    /// mods at `data/mods`, so the default matches when `--data` is unchanged.
    mods: PathBuf,
    /// Which zone to load (`--zone <id>`); the pack must have exactly one zone
    /// when omitted (fail-closed, no silent pick).
    zone: Option<String>,
    /// Exit after this many frames (windowed smoke tests).
    frames: Option<u32>,
    /// Run this many scripted actions headlessly, print final map + log, exit 0.
    headless: Option<u32>,
    /// Save directory for `save`/`load` console commands and `--load`
    /// (`--save-dir <dir>`, default `./saves`).
    save_dir: PathBuf,
    /// Load a named save at startup (`--load <name>`), replacing live state.
    load: Option<String>,
    /// Console command lines to run at startup before scripted actions
    /// (`--exec <line>`, repeatable).
    exec: Vec<String>,
}

fn parse_args() -> Result<Args> {
    let mut data = PathBuf::from("data/base");
    let mut mods = PathBuf::from("data/mods");
    let mut zone = None;
    let mut frames = None;
    let mut headless = None;
    let mut save_dir = PathBuf::from("saves");
    let mut load = None;
    let mut exec = Vec::new();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--data" => data = it.next().context("--data needs a path")?.into(),
            "--mods" => mods = it.next().context("--mods needs a path")?.into(),
            "--zone" => zone = Some(it.next().context("--zone needs an id")?.to_string()),
            "--frames" => frames = Some(it.next().context("--frames needs a number")?.parse()?),
            "--save-dir" => save_dir = it.next().context("--save-dir needs a path")?.into(),
            "--load" => load = Some(it.next().context("--load needs a name")?.to_string()),
            "--exec" => exec.push(
                it.next()
                    .context("--exec needs a command line")?
                    .to_string(),
            ),
            "--headless" => {
                headless = Some(it.next().context("--headless needs a number")?.parse()?)
            }
            "-h" | "--help" => {
                println!(
                    "tessera [--data <dir>] [--mods <dir>] [--zone <id>] [--frames <n>] [--headless <n>]\n\
                     \n\
                     Interactive: arrow keys / WASD / hjklyubn or numpad to move (all 8\n\
                     directions, numpad 5 = wait), `.` / Space = wait,\n\
                     E = Zap (Attack toward the last faced direction), Esc to quit.\n\
                     `--headless <n>`: run n scripted actions with no window, print final map + event log, exit 0.\n\
                     `--zone <id>`: load the named zone (defaults to the single zone in the pack).\n\
                     `--mods <dir>`: discover local directory / zip mods (default data/mods);\n\
                     base data loads first, then each mod in deterministic load order.\n\
                     `--save-dir <dir>`: save directory for save/load and --load (default ./saves).\n\
                     `--load <name>`: load the named save at startup, replacing live state.\n\
                     `--exec <line>`: run a console command at startup (repeatable), before scripted\n\
                     actions in --headless; results print to stdout.\n\
                     Console commands: help list entities spawn <bp> [x y] kill <id> | kill all <bp>\n\
                     print <id> sethp <id> <n> tp <id> <x> <y> save [name] load <name> clear"
                );
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument {other:?}"),
        }
    }
    Ok(Args {
        data,
        mods,
        zone,
        frames,
        headless,
        save_dir,
        load,
        exec,
    })
}

struct Content {
    tiles: TileRegistry,
    atlas: Atlas,
    /// Shared, hot-reloadable blueprint library + registries.
    world: Arc<World>,
    /// Shared broker the sim and the script host both operate on.
    broker: Arc<RwLock<EntityBroker>>,
    /// Script host bridge (loads + dispatches scripts against `broker`).
    bridge: ScriptBridge,
    /// Deterministic load order of the loaded mods (empty for a base-only
    /// pack). Kept so the headless demo can report what a mod run exercised.
    mod_ids: Vec<String>,
    /// Whitelist save hooks for this world (native + script parts), built once
    /// after content load. Used by the console `save`/`load` commands AND the
    /// `--load` startup flag, so both share the exact same whitelist registry.
    save_hooks: tessera_core::save::SaveHooks,
}

/// Build the world: register the engine's known part types and the dynamic
/// parts (Position / ActorEnergy / Health / Stats), load every script in the
/// content set (registering their `define_part` schemas via the host) and sync
/// those schemas into the world registry so blueprint `parts.<ScriptPart>` keys
/// validate — THEN load the blueprint set. On any error returns an anyhow
/// error carrying `file + id` diagnostics. `vfs` is the composed overlay (base
/// + mods), so scripts and blueprints resolve across all of them in one pass.
fn build_world(vfs: &dyn Vfs, bridge: &ScriptBridge) -> Result<World> {
    let mut world = World::new();
    // Engine component seam: `parts.Render` deserializes into a `RenderTile`
    // hecs component (tile id resolved against the atlas/tile registry at draw).
    world
        .register_component::<RenderTile>("Render")
        .map_err(|e| anyhow::anyhow!("registering engine part 'Render': {e}"))?;
    // Dynamic parts: Position is the event-facing
    // grid cell, ActorEnergy drives the scheduler, Health backs minimal combat,
    // Stats carries the mod-patched strength attributes.
    world
        .register_part::<Position>(PART_POSITION)
        .map_err(|e| anyhow::anyhow!("registering part 'Position': {e}"))?;
    world
        .register_part::<ActorEnergy>(tessera_core::turns::PART_ENERGY)
        .map_err(|e| anyhow::anyhow!("registering part 'ActorEnergy': {e}"))?;
    world
        .register_part::<Health>(PART_HEALTH)
        .map_err(|e| anyhow::anyhow!("registering part 'Health': {e}"))?;
    world
        .register_part::<tessera_core::events::Stats>(tessera_core::events::PART_STATS)
        .map_err(|e| anyhow::anyhow!("registering part 'Stats': {e}"))?;

    // Load scripts (deterministic, lexicographic) BEFORE the blueprint library
    // so their `define_part` schemas are in the world registry when
    // `parts.Charge` etc are validated. Scripts load before any spawn.
    for path in vfs.list("scripts").unwrap_or_default() {
        if !path.ends_with(".luau") {
            continue;
        }
        let source = vfs
            .read_to_string(&path)
            .with_context(|| format!("reading script {path}"))?;
        // Blueprint binding is informational (host dispatches per-event,
        // globally); use the script's own path as its record.
        bridge
            .load(&path, &path, &source)
            .with_context(|| format!("loading script {path}"))?;
    }
    // Mirror script-declared schemas into the world registry.
    bridge
        .sync_schemas(&mut world)
        .with_context(|| "syncing script schemas into world registry")?;

    world
        .load(vfs)
        .map_err(|e| anyhow::anyhow!("loading blueprints: {e}"))?;
    Ok(world)
}

/// Load the full content set: base data (tiles, scripts, blueprints, zones)
/// plus every discovered local directory / zip mod, layered in deterministic
/// load order. The composed overlay Vfs feeds the tile, script and blueprint
/// loaders, and each mod's `patches/*.toml` are applied to the blueprint library
/// afterwards. `mods` may be a missing or empty directory (base-only load).
fn load_content(data: &std::path::Path, mods: &std::path::Path) -> Result<Content> {
    let base_vfs = DirVfs::new(data);
    // Discover mods: directory + zip sources under the mods root. A missing /
    // empty mods directory is not an error — the demo loads base-only then.
    let sources = if mods.is_dir() {
        tessera::mods::discover_sources(mods)?
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
    // The overlay = base as the bottom layer, then each mod above it.
    let overlay = tessera::mods::Overlay::new(Box::new(base_vfs), &registry);
    let tiles = TileRegistry::load(&overlay)?;
    let mut chars = tiles.glyph_chars();
    chars.push('?');
    let atlas = Atlas::build(&chars);
    let broker = Arc::new(RwLock::new(EntityBroker::new()));
    let bridge = ScriptBridge::new(broker.clone());
    let world = build_world(&overlay, &bridge)?;
    // Apply each mod's blueprint patches in load order (sample mod patches the
    // base Goblin's tile + strength). Base content is untouched on disk.
    tessera::mods::apply_mod_patches(&world, &registry)
        .map_err(|e| anyhow::anyhow!("applying mod patches: {e}"))?;
    // Expose the blueprint world to the script host so `engine.spawn_at` can
    // resolve and stamp blueprints at runtime. The world is cheaply shared as
    // an `Arc` (its library is itself `Arc<RwLock<..>>`).
    let world = Arc::new(world);
    let save_hooks = world.save_hooks();
    bridge.set_world(world.clone());
    // Deterministic mod ids in load order (base excluded), for demo reporting.
    let mod_ids: Vec<String> = registry
        .order
        .iter()
        .skip(1)
        .map(|id| id.as_str().to_string())
        .collect();
    Ok(Content {
        tiles,
        atlas,
        world,
        broker,
        bridge,
        mod_ids,
        save_hooks,
    })
}

/// Spawn the demo world from the zone's `[spawns]` table: the `start`
/// blueprint at the zone start tile, each `random` blueprint at a seeded
/// random walkable-empty tile, and the world-state singleton (the respawn /
/// policy anchor). Builds the sim harness (with script bridge + enemy wiring)
/// around them. Falls back to a Player-only spawn when the zone declares no
/// spawns.
fn build_sim(content: &Content, zone: &Zone) -> Result<Sim> {
    let mut broker = content
        .broker
        .write()
        .map_err(|_| anyhow::anyhow!("broker lock poisoned"))?;

    // World-state singleton: a non-scheduled anchor entity scripts read and
    // write respawn/policy parts on via `engine.world_state()`. Created first
    // so its id is stable, then installed on the bridge.
    let world_state = broker.spawn();
    drop(broker);
    content.bridge.set_world_state_entity(world_state);

    // Player: from zone.spawns.start (default "Player"), placed at the zone
    // start tile.
    let start_bp = zone.spawns.start.as_deref().unwrap_or(PLAYER_BLUEPRINT);
    let mut broker = content
        .broker
        .write()
        .map_err(|_| anyhow::anyhow!("broker lock poisoned"))?;
    let player = content
        .world
        .spawn_blueprint(start_bp, &mut broker)
        .with_context(|| format!("spawning {start_bp:?}"))?;
    broker.insert_part(
        player,
        PART_POSITION,
        Box::new(Position::from_point(zone.start)) as Box<dyn Part>,
    )?;
    drop(broker);

    // Random spawns (e.g. the Goblin): one seeded random walkable-empty tile
    // each, in `[spawns].random` order. The first becomes the reference enemy
    // the sim schedules and resolves via the script policy.
    let mut enemy: Option<hecs::Entity> = None;
    let mut rng = zone_spawn_rng(&zone.id);
    for bp in &zone.spawns.random {
        let e = {
            let mut broker = content
                .broker
                .write()
                .map_err(|_| anyhow::anyhow!("broker lock poisoned"))?;
            content
                .world
                .spawn_blueprint(bp, &mut broker)
                .with_context(|| format!("spawning {bp:?}"))?
        };
        let to = {
            let b = content.broker.read().unwrap_or_else(|p| p.into_inner());
            let occupied = |cx: i32, cy: i32| {
                b.entities_with_part(PART_POSITION).iter().any(|e| {
                    b.typed_part::<Position>(*e, &PART_POSITION.into())
                        .map(|p| p.x == cx && p.y == cy)
                        .unwrap_or(false)
                })
            };
            tessera_core::path::random_walkable_empty(zone, &occupied, &mut rng)
                .ok_or_else(|| anyhow::anyhow!("no walkable-empty tile to spawn {bp:?}"))?
        };
        {
            let mut broker = content
                .broker
                .write()
                .map_err(|_| anyhow::anyhow!("broker lock poisoned"))?;
            broker.insert_part(e, PART_POSITION, Box::new(Position::from_point(to)))?;
        }
        if enemy.is_none() {
            enemy = Some(e);
        }
    }

    // Sample-mod demonstration: when a loaded mod adds a "Bogling" blueprint
    // (the shipped sample-mod does), spawn one on a seeded walkable-empty tile
    // so `--headless` shows a creature that only exists because a mod is
    // loaded, alongside the mod-patched Goblin. Base-only packs are unchanged
    // (this only fires when the blueprint is present).
    if content
        .world
        .library()
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .contains("Bogling")
    {
        const BOGLING_BP: &str = "Bogling";
        let e = {
            let mut broker = content
                .broker
                .write()
                .map_err(|_| anyhow::anyhow!("broker lock poisoned"))?;
            content
                .world
                .spawn_blueprint(BOGLING_BP, &mut broker)
                .with_context(|| format!("spawning {BOGLING_BP:?}"))?
        };
        let to = {
            let b = content.broker.read().unwrap_or_else(|p| p.into_inner());
            let occupied = |cx: i32, cy: i32| {
                b.entities_with_part(PART_POSITION).iter().any(|e| {
                    b.typed_part::<Position>(*e, &PART_POSITION.into())
                        .map(|p| p.x == cx && p.y == cy)
                        .unwrap_or(false)
                })
            };
            tessera_core::path::random_walkable_empty(zone, &occupied, &mut rng)
                .ok_or_else(|| anyhow::anyhow!("no walkable-empty tile to spawn {BOGLING_BP:?}"))?
        };
        {
            let mut broker = content
                .broker
                .write()
                .map_err(|_| anyhow::anyhow!("broker lock poisoned"))?;
            broker.insert_part(e, PART_POSITION, Box::new(Position::from_point(to)))?;
        }
    }

    Ok(Sim::from_shared(
        content.broker.clone(),
        Some(content.bridge.clone()),
        zone.clone(),
        player,
        enemy,
    ))
}

/// Deterministic per-zone RNG stream used to pick random spawn tiles (the
/// `"zone-spawn"` stream). Derived from the zone id so the goblin's spawn
/// position is a pure function of (seed, zone file), reproducible run to run.
fn zone_spawn_rng(zone_id: &str) -> tessera_core::rng::SimRng {
    const ZONE_SPAWN: u64 = 0x7a6f_6e65_5f73_7061; // "zone_spa"
    let mut acc: u64 = ZONE_SPAWN;
    for b in zone_id.bytes() {
        acc = acc
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(b as u64);
    }
    tessera_core::rng::SimRng::from_seed(acc)
}

/// Map a pressed key to the sim action it submits. The numpad is matched on
/// the *physical* key code (so movement works identically regardless of NumLock
/// state), all other bindings use the logical key. Returns `None` for keys that
/// map to nothing. A `Move` here is the same typed action the arrow/vi/wsad
/// bindings submit, so every direction — including diagonals — goes through the
/// sim's event/energy scheduler and the bump rule applies to all of them.
fn key_to_action(physical: PhysicalKey, logical: &Key) -> Option<PlayerAction> {
    match physical {
        PhysicalKey::Code(KeyCode::Numpad1) => return Some(PlayerAction::Move { dx: -1, dy: 1 }),
        PhysicalKey::Code(KeyCode::Numpad2) => return Some(PlayerAction::Move { dx: 0, dy: 1 }),
        PhysicalKey::Code(KeyCode::Numpad3) => return Some(PlayerAction::Move { dx: 1, dy: 1 }),
        PhysicalKey::Code(KeyCode::Numpad4) => return Some(PlayerAction::Move { dx: -1, dy: 0 }),
        PhysicalKey::Code(KeyCode::Numpad6) => return Some(PlayerAction::Move { dx: 1, dy: 0 }),
        PhysicalKey::Code(KeyCode::Numpad7) => return Some(PlayerAction::Move { dx: -1, dy: -1 }),
        PhysicalKey::Code(KeyCode::Numpad8) => return Some(PlayerAction::Move { dx: 0, dy: -1 }),
        PhysicalKey::Code(KeyCode::Numpad9) => return Some(PlayerAction::Move { dx: 1, dy: -1 }),
        PhysicalKey::Code(KeyCode::Numpad5) => return Some(PlayerAction::Wait),
        _ => {}
    }
    match logical {
        Key::Named(NamedKey::ArrowLeft) => Some(PlayerAction::Move { dx: -1, dy: 0 }),
        Key::Named(NamedKey::ArrowRight) => Some(PlayerAction::Move { dx: 1, dy: 0 }),
        Key::Named(NamedKey::ArrowUp) => Some(PlayerAction::Move { dx: 0, dy: -1 }),
        Key::Named(NamedKey::ArrowDown) => Some(PlayerAction::Move { dx: 0, dy: 1 }),
        Key::Character(c) => match c.as_str() {
            "h" | "a" => Some(PlayerAction::Move { dx: -1, dy: 0 }),
            "l" | "d" => Some(PlayerAction::Move { dx: 1, dy: 0 }),
            "k" | "w" => Some(PlayerAction::Move { dx: 0, dy: -1 }),
            "j" | "s" => Some(PlayerAction::Move { dx: 0, dy: 1 }),
            "y" => Some(PlayerAction::Move { dx: -1, dy: -1 }),
            "u" => Some(PlayerAction::Move { dx: 1, dy: -1 }),
            "b" => Some(PlayerAction::Move { dx: -1, dy: 1 }),
            "n" => Some(PlayerAction::Move { dx: 1, dy: 1 }),
            "e" => Some(PlayerAction::Zap),
            "." | " " => Some(PlayerAction::Wait),
            _ => None,
        },
        _ => None,
    }
}

struct App {
    args: Args,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    _watcher: Option<notify::RecommendedWatcher>,
    content: Content,
    sim: Sim,
    frames_drawn: u32,
    atlas_dirty: bool,
    /// Windowed devtools overlay (console panel + reserved inspector hook).
    /// Only compiled with the `devtools` feature; a default build has no egui in
    /// the dependency graph.
    #[cfg(feature = "devtools")]
    devtools: Option<devtools::Devtools>,
}

impl App {
    /// Build the draw cells by reading sim + map state only (pure reader). It
    /// never dispatches events and never mutates sim state.
    fn build_cells(&self) -> Vec<Cell> {
        let targets = self.sim.renderables();
        sim::build_cells(
            self.sim.map(),
            &self.content.tiles,
            &self.content.atlas,
            &targets,
        )
    }

    /// Hot reload (script + blueprint + tileset): rebuild the whole content
    /// set from disk and swap it in *atomically*. Watch the recursive file
    /// watcher: `data/*/scripts/*.luau`, `data/*/blueprints/*.toml` and the
    /// tile/tower definitions all flow through this one path.
    ///
    /// Semantics (whole-library swap, mirroring `World::load`'s atomic policy):
    /// - On any error the previous content is kept — a mid-edit save never
    ///   blanks or crashes the running app (fail-closed).
    /// - Live entities keep their parts: the running `sim` holds its own broker
    ///   and is not disturbed, so settled `ScriptPart` data and positions are
    ///   untouched. New spawns in a fresh session use the new content.
    /// - The newly built script host is a fresh sandbox whose `engine.on`
    ///   handlers replace the previous set deterministically; `SimRng` in the
    ///   sim is never touched.
    fn reload(&mut self) {
        match load_content(&self.args.data, &self.args.mods) {
            Ok(c) => {
                tracing::info!("content reloaded");
                self.content = c;
                self.atlas_dirty = true;
            }
            // Keep the old content: a typo mid-edit must not crash or blank the game.
            Err(e) => tracing::warn!("reload failed, keeping previous content: {e}"),
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("Tessera")
            .with_inner_size(PhysicalSize::new(GRID_W * 16, GRID_H * 16));
        let window = Arc::new(el.create_window(attrs).expect("create window"));
        let renderer =
            Renderer::new(el, window.clone(), (GRID_W, GRID_H), 2).expect("init renderer");
        self.atlas_dirty = true;
        // Windowed devtools overlay: shares the renderer's Device/Queue/surface
        // format (no eframe, no second surface).
        #[cfg(feature = "devtools")]
        {
            let devtools =
                devtools::Devtools::new(&window, renderer.device(), renderer.surface_format());
            self.devtools = Some(devtools);
        }
        window.request_redraw();
        self.window = Some(window);
        self.renderer = Some(renderer);
    }

    fn user_event(&mut self, _: &ActiveEventLoop, ev: UserEvent) {
        match ev {
            UserEvent::ContentChanged => {
                self.reload();
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, ev: WindowEvent) {
        // Devtools input hook: feed the event through egui State first. When
        // egui consumes the event (e.g. typing into the console TextEdit),
        // game keys are suppressed — so typing `s` in the console does not move
        // the player. The backtick/F3 panel toggles are also handled inside
        // on_window_event and never reach the game.
        #[cfg(feature = "devtools")]
        let egui_consumed =
            if let (Some(d), Some(w)) = (self.devtools.as_mut(), self.window.as_deref()) {
                d.on_window_event(w, &ev)
            } else {
                false
            };
        match ev {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::Resized(s) => {
                if let Some(r) = &mut self.renderer {
                    r.resize(s.width, s.height);
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                // If egui consumed this key, it belongs to the devtools UI —
                // do not route it to the game at all.
                #[cfg(feature = "devtools")]
                if egui_consumed {
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                    return;
                }
                // Arrows / WASD / hjklyubn and the numpad (all eight directions,
                // physical codes so NumLock state doesn't matter) map to a Move;
                // `.` / Space / numpad 5 map to Wait; `e` is Zap. The action is
                // submitted to the sim, which runs the scheduler until the
                // player's turn and dispatches the event.
                if event.logical_key == Key::Named(NamedKey::Escape) {
                    return el.exit();
                }
                let action = key_to_action(event.physical_key, &event.logical_key);
                if let Some(action) = action {
                    if let Err(e) = self.sim.submit_action(action) {
                        tracing::warn!("submit_action failed: {e:#}");
                    }
                    // One-line HP status readout (visible in the title bar).
                    if let Some(w) = &self.window {
                        w.set_title(&format!(
                            "Tessera  hp={}  turn={}",
                            self.sim.player_health(),
                            self.sim.player_turns_public()
                        ));
                        w.request_redraw();
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                let cells = self.build_cells();
                if let Some(r) = &mut self.renderer {
                    if self.atlas_dirty {
                        r.upload_atlas(&self.content.atlas);
                        self.atlas_dirty = false;
                    }
                    #[cfg(feature = "devtools")]
                    {
                        if let Some(d) = &mut self.devtools {
                            let window = self.window.clone().expect("window exists");
                            let cc = console::ConsoleContext {
                                world: &self.content.world,
                                save_hooks: &self.content.save_hooks,
                                save_dir: &self.args.save_dir,
                            };
                            r.render_with(&cells, |device, queue, view, enc| {
                                let mut scene =
                                    devtools::SceneCtx::new(&window, device, queue, view, enc);
                                d.paint(&mut scene, &mut self.sim, &cc)
                            });
                        } else {
                            r.render(&cells);
                        }
                    }
                    #[cfg(not(feature = "devtools"))]
                    r.render(&cells);
                }
                self.frames_drawn += 1;
                if let Some(limit) = self.args.frames {
                    if self.frames_drawn >= limit {
                        el.exit();
                    } else if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }
            _ => {}
        }
    }
}

/// Run the startup console hooks shared by the headless and windowed paths:
/// first `--load <name>` (replaces live state from a save), then each
/// `--exec <line>` in order. Results are printed to stdout (headless contract;
/// the windowed transcript is a separate later change). Also sweeps stale
/// `*.tmp` save fragments once per startup (saves are written atomically via a
/// same-directory temp file; see the save module docs).
fn startup_console(sim: &mut Sim, content: &Content, args: &Args) {
    if let Err(e) = sweep_stale_tmp(&args.save_dir) {
        tracing::warn!("save dir temp sweep failed: {e}");
    }
    let ctx = ConsoleContext {
        world: &content.world,
        save_hooks: &content.save_hooks,
        save_dir: &args.save_dir,
    };
    // "--load" first so it becomes the base state the --exec lines act on.
    if let Some(name) = &args.load {
        let path = args.save_dir.join(format!("{name}.tess"));
        match tessera_core::save::load(&path) {
            Ok(data) => match sim.restore_from_save(data, ctx.save_hooks) {
                Ok(()) => println!("[console] loaded {name} from {}", path.display()),
                Err(e) => println!("[console] error: load {name}: {e}"),
            },
            Err(e) => println!("[console] error: load {name}: {e}"),
        }
    }
    for line in &args.exec {
        if line.trim().is_empty() {
            continue;
        }
        match console::parse(line) {
            Ok(cmd) => match console::execute(sim, &ctx, cmd) {
                ConsoleOutcome::Ok(text) => println!("[console] {text}"),
                ConsoleOutcome::Err(e) => println!("[console] error: {e}"),
                ConsoleOutcome::ReloadWorld => println!("[console] world reloaded from save"),
            },
            Err(e) => println!("[console] error: {e}"),
        }
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,wgpu=warn,naga=warn".into()),
        )
        .init();
    let args = parse_args()?;
    let content = load_content(&args.data, &args.mods).with_context(|| {
        format!(
            "loading content from {} (mods {})",
            args.data.display(),
            args.mods.display()
        )
    })?;

    // Load the world pack and resolve the zone to simulate: `--zone <id>`, or
    // the single zone when none was requested (fail-closed otherwise).
    let vfs = DirVfs::new(&args.data);
    let world_map = load_zones(&vfs, &content.tiles)
        .with_context(|| format!("loading zones from {}", args.data.display()))?;
    let zone = world_map
        .resolve(args.zone.as_deref())
        .with_context(|| format!("resolving zone for {}", args.data.display()))?;
    for id in zone.missing_tile_ids(&content.tiles) {
        tracing::warn!("zone uses tile id {id:?} with no definition (will draw as '?')");
    }

    // Headless path: no window, no event loop, no winit. Deterministic output.
    if let Some(n) = args.headless {
        let mut sim = build_sim(&content, zone)?;
        // Startup --load first (replaces live state from a save), then each
        // --exec line runs BEFORE the scripted actions and prints to stdout —
        // the headless console run contract.
        startup_console(&mut sim, &content, &args);

        let map_text = sim.run_headless(&content.tiles, n);
        print!("{map_text}");
        for line in sim.log() {
            println!("{line}");
        }
        // One-line status readout: hp + player-turn clock (the combat / death
        // / respawn behaviors' observable summary).
        println!(
            "status hp={} player_turns={} position=({},{})",
            sim.player_health(),
            sim.player_turns_public(),
            sim.position().0,
            sim.position().1
        );
        // Show what a mod load exercised, so the demo is observable from a
        // headless run: the loaded mod ids and the sample's added creature.
        if !content.mod_ids.is_empty() {
            let lib = content
                .world
                .library()
                .read()
                .unwrap_or_else(|p| p.into_inner());
            println!("mods loaded: {}", content.mod_ids.join(", "));
            if lib.contains("Bogling") {
                println!(
                    "mod demo: Bogling spawned with sample-mod Charge; Goblin tile+strength patched"
                );
            }
        }
        return Ok(());
    }

    let mut sim = build_sim(&content, zone)?;
    // Apply --load / --exec at windowed startup too (the run contract is
    // identical; only the transcript rendering differs and comes later).
    startup_console(&mut sim, &content, &args);

    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();

    // Dev hot reload: any change under the data dir — base *or* mods — triggers
    // a reload (a single miss keeps the whole previous content, mods included,
    // fail-closed). Watch the mods root too when it exists, so edits to local
    // directory / zip mods are picked up like base edits.
    let watcher = {
        let p = proxy.clone();
        let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res
                && !ev.kind.is_access()
            {
                let _ = p.send_event(UserEvent::ContentChanged);
            }
        })?;
        let base_ok = w.watch(&args.data, RecursiveMode::Recursive).is_ok();
        let mods_ok = args.mods.is_dir() && w.watch(&args.mods, RecursiveMode::Recursive).is_ok();
        if base_ok || mods_ok {
            Some(w)
        } else {
            tracing::warn!("hot reload disabled");
            None
        }
    };

    let mut app = App {
        args,
        window: None,
        renderer: None,
        _watcher: watcher,
        content,
        sim,
        frames_drawn: 0,
        atlas_dirty: true,
        #[cfg(feature = "devtools")]
        devtools: None,
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mv(dx: i32, dy: i32) -> Option<PlayerAction> {
        Some(PlayerAction::Move { dx, dy })
    }

    /// A physical numpad code, as the event handler reports it.
    fn numpad(code: KeyCode) -> PhysicalKey {
        PhysicalKey::Code(code)
    }

    /// A non-numpad physical code — falls through to the logical-key match.
    fn other() -> PhysicalKey {
        PhysicalKey::Code(KeyCode::KeyA)
    }

    /// All eight numpad directions map to a Move (physical key codes, so the
    /// NumLock state does not change the binding).
    #[test]
    fn numpad_all_eight_directions() {
        assert_eq!(
            key_to_action(numpad(KeyCode::Numpad8), &Key::Character("8".into())),
            mv(0, -1)
        );
        assert_eq!(
            key_to_action(numpad(KeyCode::Numpad2), &Key::Character("2".into())),
            mv(0, 1)
        );
        assert_eq!(
            key_to_action(numpad(KeyCode::Numpad4), &Key::Character("4".into())),
            mv(-1, 0)
        );
        assert_eq!(
            key_to_action(numpad(KeyCode::Numpad6), &Key::Character("6".into())),
            mv(1, 0)
        );
        assert_eq!(
            key_to_action(numpad(KeyCode::Numpad7), &Key::Character("7".into())),
            mv(-1, -1)
        );
        assert_eq!(
            key_to_action(numpad(KeyCode::Numpad9), &Key::Character("9".into())),
            mv(1, -1)
        );
        assert_eq!(
            key_to_action(numpad(KeyCode::Numpad1), &Key::Character("1".into())),
            mv(-1, 1)
        );
        assert_eq!(
            key_to_action(numpad(KeyCode::Numpad3), &Key::Character("3".into())),
            mv(1, 1)
        );
    }

    #[test]
    fn numpad_five_waits() {
        assert_eq!(
            key_to_action(numpad(KeyCode::Numpad5), &Key::Character("5".into())),
            Some(PlayerAction::Wait)
        );
    }

    #[test]
    fn arrow_keys_still_move() {
        assert_eq!(
            key_to_action(other(), &Key::Named(NamedKey::ArrowLeft)),
            mv(-1, 0)
        );
        assert_eq!(
            key_to_action(other(), &Key::Named(NamedKey::ArrowRight)),
            mv(1, 0)
        );
        assert_eq!(
            key_to_action(other(), &Key::Named(NamedKey::ArrowUp)),
            mv(0, -1)
        );
        assert_eq!(
            key_to_action(other(), &Key::Named(NamedKey::ArrowDown)),
            mv(0, 1)
        );
    }

    #[test]
    fn vi_keys_still_move() {
        assert_eq!(
            key_to_action(other(), &Key::Character("h".into())),
            mv(-1, 0)
        );
        assert_eq!(
            key_to_action(other(), &Key::Character("j".into())),
            mv(0, 1)
        );
        assert_eq!(
            key_to_action(other(), &Key::Character("k".into())),
            mv(0, -1)
        );
        assert_eq!(
            key_to_action(other(), &Key::Character("l".into())),
            mv(1, 0)
        );
        assert_eq!(
            key_to_action(other(), &Key::Character("y".into())),
            mv(-1, -1)
        );
        assert_eq!(
            key_to_action(other(), &Key::Character("u".into())),
            mv(1, -1)
        );
        assert_eq!(
            key_to_action(other(), &Key::Character("b".into())),
            mv(-1, 1)
        );
        assert_eq!(
            key_to_action(other(), &Key::Character("n".into())),
            mv(1, 1)
        );
    }

    #[test]
    fn wasd_keys_still_move() {
        assert_eq!(
            key_to_action(other(), &Key::Character("a".into())),
            mv(-1, 0)
        );
        assert_eq!(
            key_to_action(other(), &Key::Character("d".into())),
            mv(1, 0)
        );
        assert_eq!(
            key_to_action(other(), &Key::Character("w".into())),
            mv(0, -1)
        );
        assert_eq!(
            key_to_action(other(), &Key::Character("s".into())),
            mv(0, 1)
        );
    }

    #[test]
    fn wait_and_zap_still_map() {
        assert_eq!(
            key_to_action(other(), &Key::Character(".".into())),
            Some(PlayerAction::Wait)
        );
        assert_eq!(
            key_to_action(other(), &Key::Character(" ".into())),
            Some(PlayerAction::Wait)
        );
        assert_eq!(
            key_to_action(other(), &Key::Character("e".into())),
            Some(PlayerAction::Zap)
        );
    }

    #[test]
    fn unmapped_keys_are_none() {
        assert_eq!(key_to_action(other(), &Key::Character("z".into())), None);
        assert_eq!(
            key_to_action(
                PhysicalKey::Code(KeyCode::KeyQ),
                &Key::Character("q".into())
            ),
            None
        );
        assert_eq!(
            key_to_action(numpad(KeyCode::Numpad0), &Key::Character("0".into())),
            None
        );
    }
}
