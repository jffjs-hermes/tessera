//! Tessera demo: a data-driven `@` spawned from `[Player]` (a blueprint with
//! `parts.Render = { tile = "creature.player" }`), drawn from the tile registry.
//! Edit `data/base/blueprints/` while running and the blueprint library reloads
//! (new spawns use the new library; existing entities keep their parts).
//!
//! Turn-by-turn: input is routed through the
//! event/scheduler harness in [`sim::Sim`]. Arrow keys / WASD move the `@`,
//! `.` / Space wait; a wall-blocked move is cancelled by an EARLY map-collision
//! handler (no energy cost). `--headless <n>` runs `n` scripted actions with no
//! window and prints the final map + event log — the deterministic CI smoke
//! path. The renderer stays a pure reader: it never dispatches events or
//! mutates sim state; the redraw on hot reload is unchanged.

mod scripting;
mod sim;

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use notify::{RecursiveMode, Watcher};
use sim::{PlayerAction, Sim};
use tessera_core::ecs::{EntityBroker, Part};
use tessera_core::events::{Health, PART_HEALTH, PART_POSITION, Position};
use tessera_core::map::Map;
use tessera_core::tiles::TileRegistry;
use tessera_core::turns::ActorEnergy;
use tessera_core::vfs::{DirVfs, Vfs};
use tessera_core::world::{RenderTile, World};
use tessera_render::atlas::Atlas;
use tessera_render::{Cell, Renderer};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

use crate::scripting::ScriptBridge;

const GRID_W: u32 = 80;
const GRID_H: u32 = 50;
/// The `[Player]` blueprint id in `data/base/blueprints/creatures.toml`.
const PLAYER_BLUEPRINT: &str = "Player";
/// The reference demo Zapper blueprint (`data/base/blueprints/zapper.toml`).
const ZAPPER_BLUEPRINT: &str = "Zapper";

enum UserEvent {
    ContentChanged,
}

struct Args {
    data: PathBuf,
    /// Exit after this many frames (windowed smoke tests).
    frames: Option<u32>,
    /// Run this many scripted actions headlessly, print final map + log, exit 0.
    headless: Option<u32>,
}

fn parse_args() -> Result<Args> {
    let mut data = PathBuf::from("data/base");
    let mut frames = None;
    let mut headless = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--data" => data = it.next().context("--data needs a path")?.into(),
            "--frames" => frames = Some(it.next().context("--frames needs a number")?.parse()?),
            "--headless" => {
                headless = Some(it.next().context("--headless needs a number")?.parse()?)
            }
            "-h" | "--help" => {
                println!(
                    "tessera [--data <dir>] [--frames <n>] [--headless <n>]\n\
                     \n\
                     Interactive: arrow keys / WASD / hjklyubn to move, `.` or Space = wait,\n\
                     E = Zap (Attack toward the last faced direction), Esc to quit.\n\
                     `--headless <n>`: run n scripted actions with no window, print final map + event log, exit 0."
                );
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument {other:?}"),
        }
    }
    Ok(Args {
        data,
        frames,
        headless,
    })
}

struct Content {
    tiles: TileRegistry,
    atlas: Atlas,
    /// Shared, hot-reloadable blueprint library + registries.
    world: World,
    /// Shared broker the sim and the script host both operate on.
    broker: Arc<RwLock<EntityBroker>>,
    /// Script host bridge (loads + dispatches scripts against `broker`).
    bridge: ScriptBridge,
}

/// Build the world: register the engine's known part types and the dynamic
/// parts (Position / ActorEnergy / Health), load every script in the pack
/// (registering their `define_part` schemas via the host) and sync those
/// schemas into the world registry so blueprint `parts.<ScriptPart>` keys
/// validate — THEN load the blueprint set. On any error returns an anyhow
/// error carrying `file + id` diagnostics.
fn build_world(vfs: &DirVfs, bridge: &ScriptBridge) -> Result<World> {
    let mut world = World::new();
    // Engine component seam: `parts.Render` deserializes into a `RenderTile`
    // hecs component (tile id resolved against the atlas/tile registry at draw).
    world
        .register_component::<RenderTile>("Render")
        .map_err(|e| anyhow::anyhow!("registering engine part 'Render': {e}"))?;
    // Dynamic parts: Position is the event-facing
    // grid cell, ActorEnergy drives the scheduler, Health backs minimal combat.
    world
        .register_part::<Position>(PART_POSITION)
        .map_err(|e| anyhow::anyhow!("registering part 'Position': {e}"))?;
    world
        .register_part::<ActorEnergy>(tessera_core::turns::PART_ENERGY)
        .map_err(|e| anyhow::anyhow!("registering part 'ActorEnergy': {e}"))?;
    world
        .register_part::<Health>(PART_HEALTH)
        .map_err(|e| anyhow::anyhow!("registering part 'Health': {e}"))?;

    // Load scripts (deterministic, lexicographic) BEFORE the blueprint library
    // so their `define_part` schemas are in the world registry when
    // `parts.Charge` etc are validated. §4.3: scripts load before any spawn.
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

fn load_content(data: &std::path::Path) -> Result<Content> {
    let vfs = DirVfs::new(data);
    let tiles = TileRegistry::load(&vfs)?;
    let mut chars = tiles.glyph_chars();
    chars.push('?');
    let atlas = Atlas::build(&chars);
    let broker = Arc::new(RwLock::new(EntityBroker::new()));
    let bridge = ScriptBridge::new(broker.clone());
    let world = build_world(&vfs, &bridge)?;
    Ok(Content {
        tiles,
        atlas,
        world,
        broker,
        bridge,
    })
}

/// Spawn the `[Player]` and a `[Zapper]` (if the blueprint exists) into the
/// shared broker, seed positions, and build the sim harness (with script
/// bridge + enemy) around them.
fn build_sim(content: &Content, map: Map) -> Result<Sim> {
    let mut broker = content
        .broker
        .write()
        .map_err(|_| anyhow::anyhow!("broker lock poisoned"))?;
    let player = content
        .world
        .spawn_blueprint(PLAYER_BLUEPRINT, &mut broker)
        .with_context(|| format!("spawning {PLAYER_BLUEPRINT:?}"))?;
    broker.insert_part(
        player,
        PART_POSITION,
        Box::new(Position { x: 2, y: 2 }) as Box<dyn Part>,
    )?;
    drop(broker);

    // The reference demo Zapper: spawn adjacent to the player opposite its
    // initial facing, only if the base pack ships the blueprint (falling back
    // to a scripts-only run otherwise).
    let has_zapper = match content.world.library().read() {
        Ok(l) => l.contains(ZAPPER_BLUEPRINT),
        Err(_) => false,
    };
    let zapper = if has_zapper {
        let mut broker = content
            .broker
            .write()
            .map_err(|_| anyhow::anyhow!("broker lock poisoned"))?;
        let z = content
            .world
            .spawn_blueprint(ZAPPER_BLUEPRINT, &mut broker)
            .with_context(|| format!("spawning {ZAPPER_BLUEPRINT:?}"))?;
        broker.insert_part(
            z,
            PART_POSITION,
            Box::new(Position { x: 3, y: 2 }) as Box<dyn Part>,
        )?;
        Some(z)
    } else {
        None
    };

    Ok(Sim::from_shared(
        content.broker.clone(),
        Some(content.bridge.clone()),
        map,
        player,
        zapper,
    ))
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
}

impl App {
    /// Build the draw cells by reading sim + map state only (pure reader). It
    /// never dispatches events and never mutates sim state.
    fn build_cells(&self) -> Vec<Cell> {
        let map = self.sim.map();
        let mut cells = Vec::with_capacity(map.width * map.height);
        let mut push = |x: usize, y: usize, id: &str, under: Option<&tessera_core::tiles::Tile>| {
            let t = self.content.tiles.get(id);
            let tessera_core::tiles::TileVisual::Glyph { ch } = t.visual;
            let bg =
                t.bg.or(under.and_then(|u| u.bg))
                    .unwrap_or(tessera_core::tiles::Rgb::BLACK);
            let (f, b) = (t.fg.to_f32(), bg.to_f32());
            cells.push(Cell {
                x: x as u32,
                y: y as u32,
                glyph: self.content.atlas.slot(ch),
                fg: [f[0], f[1], f[2], 1.0],
                bg: [b[0], b[1], b[2], 1.0],
            });
        };
        // Read the player's tile id from its live RenderTile each frame so the
        // draw path stays data-driven (spawned from `[Player]`, not hardcoded).
        let player_tile = self.sim.player_tile();
        let (px, py) = self.sim.position();
        for y in 0..map.height {
            for x in 0..map.width {
                let terrain = map.tile_id(x, y);
                if (x as i32, y as i32) == (px, py) {
                    let under = self.content.tiles.get(terrain).clone();
                    push(x, y, &player_tile, Some(&under));
                } else {
                    push(x, y, terrain, None);
                }
            }
        }
        cells
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
    ///   untouched (§6.3). New spawns in a fresh session use the new content.
    /// - The newly built script host is a fresh sandbox whose `engine.on`
    ///   handlers replace the previous set deterministically; `SimRng` in the
    ///   sim is never touched.
    fn reload(&mut self) {
        match load_content(&self.args.data) {
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
        match ev {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::Resized(s) => {
                if let Some(r) = &mut self.renderer {
                    r.resize(s.width, s.height);
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                // Arrow keys / WASD (and the existing hjklyubn) map to a Move,
                // `.` / Space map to Wait, per spec §3.2. The action is
                // submitted to the sim, which runs the scheduler until the
                // player's turn and dispatches the event.
                let action = match &event.logical_key {
                    Key::Named(NamedKey::Escape) => return el.exit(),
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
                };
                if let Some(action) = action {
                    if let Err(e) = self.sim.submit_action(action) {
                        tracing::warn!("submit_action failed: {e:#}");
                    }
                    if let Some(w) = &self.window {
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

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,wgpu=warn,naga=warn".into()),
        )
        .init();
    let args = parse_args()?;
    let content = load_content(&args.data)
        .with_context(|| format!("loading content from {}", args.data.display()))?;
    let map = Map::demo(GRID_W as usize, GRID_H as usize);
    for id in map.missing_tile_ids(&content.tiles) {
        tracing::warn!("map uses tile id {id:?} with no definition (will draw as '?')");
    }

    // Headless path: no window, no event loop, no winit. Deterministic output.
    if let Some(n) = args.headless {
        let mut sim = build_sim(&content, map)?;
        let map_text = sim.run_headless(&content.tiles, n);
        print!("{map_text}");
        for line in sim.log() {
            println!("{line}");
        }
        return Ok(());
    }

    let sim = build_sim(&content, map)?;

    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();

    // Dev hot reload: any change under the data dir triggers a reload.
    let watcher = {
        let p = proxy.clone();
        let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                if !ev.kind.is_access() {
                    let _ = p.send_event(UserEvent::ContentChanged);
                }
            }
        })?;
        match w.watch(&args.data, RecursiveMode::Recursive) {
            Ok(()) => Some(w),
            Err(e) => {
                tracing::warn!("hot reload disabled: {e}");
                None
            }
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
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}
