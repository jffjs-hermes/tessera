//! Tessera M2 demo: a data-driven `@` spawned from `[Player]` (a blueprint with
//! `parts.Render = { tile = "creature.player" }`), drawn from the tile registry.
//! Edit `data/base/blueprints/` while running and the blueprint library reloads
//! (new spawns use the new library; existing entities keep their parts).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use notify::{RecursiveMode, Watcher};
use tessera_core::ecs::EntityBroker;
use tessera_core::map::Map;
use tessera_core::tiles::TileRegistry;
use tessera_core::vfs::DirVfs;
use tessera_core::world::{RenderTile, World};
use tessera_render::atlas::Atlas;
use tessera_render::{Cell, Renderer};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

const GRID_W: u32 = 80;
const GRID_H: u32 = 50;
/// The `[Player]` blueprint id in `data/base/blueprints/creatures.toml`.
const PLAYER_BLUEPRINT: &str = "Player";

enum UserEvent {
    ContentChanged,
}

struct Args {
    data: PathBuf,
    /// Exit after this many frames (smoke tests / CI).
    frames: Option<u32>,
}

fn parse_args() -> Result<Args> {
    let mut data = PathBuf::from("data/base");
    let mut frames = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--data" => data = it.next().context("--data needs a path")?.into(),
            "--frames" => frames = Some(it.next().context("--frames needs a number")?.parse()?),
            "-h" | "--help" => {
                println!("tessera [--data <dir>] [--frames <n>]");
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument {other:?}"),
        }
    }
    Ok(Args { data, frames })
}

struct Content {
    tiles: TileRegistry,
    atlas: Atlas,
    /// Shared, hot-reloadable blueprint library + registries.
    world: World,
}

/// Build the M2 world: register the engine's known part types, then load the
/// blueprint set. On any error returns an anyhow error carrying `file + id`
/// diagnostics from the loader.
fn build_world(vfs: &DirVfs) -> Result<World> {
    let mut world = World::new();
    // Engine component seam: `parts.Render` deserializes into a `RenderTile`
    // hecs component (tile id resolved against the atlas/tile registry at draw).
    world
        .register_component::<RenderTile>("Render")
        .map_err(|e| anyhow::anyhow!("registering engine part 'Render': {e}"))?;
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
    let world = build_world(&vfs)?;
    Ok(Content {
        tiles,
        atlas,
        world,
    })
}

struct App {
    args: Args,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    _watcher: Option<notify::RecommendedWatcher>,
    content: Content,
    map: Map,
    /// The live `[Player]` entity, spawned from data on startup and untouched by
    /// later blueprint reloads (existing entities keep their parts — M2).
    broker: EntityBroker,
    player: hecs::Entity,
    player_pos: (i32, i32),
    frames_drawn: u32,
    atlas_dirty: bool,
}

impl App {
    fn build_cells(&self) -> Vec<Cell> {
        let mut cells = Vec::with_capacity(self.map.width * self.map.height);
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
        let player_tile = self
            .broker
            .world()
            .get::<&RenderTile>(self.player)
            .ok()
            .map_or_else(|| "creature.player.missing".to_string(), |r| r.tile.clone());
        for y in 0..self.map.height {
            for x in 0..self.map.width {
                let terrain = self.map.tile_id(x, y);
                if (x as i32, y as i32) == self.player_pos {
                    let under = self.content.tiles.get(terrain).clone();
                    push(x, y, &player_tile, Some(&under));
                } else {
                    push(x, y, terrain, None);
                }
            }
        }
        cells
    }

    fn try_move(&mut self, dx: i32, dy: i32) {
        let (nx, ny) = (self.player_pos.0 + dx, self.player_pos.1 + dy);
        if self.map.is_walkable(nx, ny) {
            self.player_pos = (nx, ny);
        }
    }

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
                let (dx, dy) = match &event.logical_key {
                    Key::Named(NamedKey::Escape) => return el.exit(),
                    Key::Named(NamedKey::ArrowLeft) => (-1, 0),
                    Key::Named(NamedKey::ArrowRight) => (1, 0),
                    Key::Named(NamedKey::ArrowUp) => (0, -1),
                    Key::Named(NamedKey::ArrowDown) => (0, 1),
                    Key::Character(c) => match c.as_str() {
                        "h" | "a" => (-1, 0),
                        "l" | "d" => (1, 0),
                        "k" | "w" => (0, -1),
                        "j" | "s" => (0, 1),
                        "y" => (-1, -1),
                        "u" => (1, -1),
                        "b" => (-1, 1),
                        "n" => (1, 1),
                        _ => return,
                    },
                    _ => return,
                };
                self.try_move(dx, dy);
                if let Some(w) = &self.window {
                    w.request_redraw();
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
    // Spawn the player from `[Player]` blueprint data (M2), stamping the engine
    // RenderTile component. The single live entity is kept across later reloads.
    let mut broker = EntityBroker::new();
    let player = content
        .world
        .spawn_blueprint(PLAYER_BLUEPRINT, &mut broker)
        .with_context(|| format!("spawning {PLAYER_BLUEPRINT:?}"))?;
    let map = Map::demo(GRID_W as usize, GRID_H as usize);
    for id in map.missing_tile_ids(&content.tiles) {
        tracing::warn!("map uses tile id {id:?} with no definition (will draw as '?')");
    }

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
        map,
        broker,
        player,
        player_pos: (2, 2),
        frames_drawn: 0,
        atlas_dirty: true,
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}
