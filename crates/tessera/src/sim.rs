//! Demo simulation harness (energy-clock, deterministic event bus).
//!
//! Wires the core event bus and energy scheduler into the demo: the player is
//! the single scheduled actor, keyboard input is converted to a
//! [`PlayerAction`] and submitted to [`Sim::submit_action`], which runs the
//! scheduler until the player's turn then dispatches the action event through
//! the bus. A wall-collision `EARLY` handler on [`Move`] cancels any move
//! whose target is not walkable — the first content-level handler in the
//! shipped demo, and the path that proves event cancellation outside tests.
//!
//! The renderer is a strict reader: [`Sim::text_map`] / `build_cells` only
//! *read* sim state (map + player position); they never dispatch events or
//! mutate state. Redraw on hot reload is unchanged.
//!
//! # Headless
//!
//! [`Sim::run_headless`] advances the sim through `n` scripted actions with no
//! window and returns the final map + a stable, one-event-per-line event log
//! (KEY <NAME> tick=.. [fields]) suitable as the seed of the future replay
//! format. Deterministic for a fixed script and map (the sim consumes no
//! randomness — no AI, no f32, integer-only energy).

use hecs::Entity;
use tessera_core::ecs::EntityBroker;
use tessera_core::events::{EARLY, HandlerAction, Move, Point, SimContext};
use tessera_core::map::Map;
use tessera_core::rng::SimRng;
use tessera_core::tiles::TileRegistry;
use tessera_core::turns::{
    ActorEnergy, BASE_SPEED, PART_ENERGY, Scheduler, TurnAction, TurnReport,
};
use tessera_core::world::RenderTile;

/// Deterministic seed for the demo's forked RNG streams. The sim never reads
/// randomness (no AI), so this keeps headless output identical run to
/// run for a fixed script.
const SIM_SEED: u64 = 20_260_317;

/// An action the player can submit to the simulation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayerAction {
    /// Try to move one cell in a direction (dx, dy ∈ {-1, 0, 1}).
    Move { dx: i32, dy: i32 },
    /// Do nothing this turn.
    Wait,
}

/// The demo simulation harness: entity broker + map + rng + typed event bus +
/// energy scheduler, plus the deterministic event log.
pub struct Sim {
    ctx: SimContext,
    scheduler: Scheduler,
    player: Entity,
    log: Vec<String>,
}

impl Sim {
    /// Build the harness around an already-spawned `player` whose broker
    /// carries a `Position` part. The player's `ActorEnergy.speed` (from the
    /// blueprint) becomes its scheduler speed; it starts exactly at threshold so
    /// the first keypress acts immediately.
    pub fn new(broker: EntityBroker, map: Map, player: Entity) -> Self {
        let mut ctx = SimContext::new(broker, map, SimRng::from_seed(SIM_SEED));
        // Content-level wall collision: cancel any Move whose target is not
        // walkable. This is the shipped cancellation handler (spec §3.2).
        ctx.register::<Move>(EARLY, |ctx, ev| {
            if ctx.map.is_walkable(ev.to.x, ev.to.y) {
                HandlerAction::Continue
            } else {
                HandlerAction::Cancel
            }
        });
        let speed = ctx
            .broker
            .typed_part::<ActorEnergy>(player, &PART_ENERGY.into())
            .map(|a| a.speed)
            .unwrap_or(BASE_SPEED);
        let mut scheduler = Scheduler::new();
        scheduler.add_actor(player, ActorEnergy::at_threshold(speed));
        Sim {
            ctx,
            scheduler,
            player,
            log: Vec::new(),
        }
    }

    /// The player's current grid position (from the `Position` part).
    pub fn position(&self) -> (i32, i32) {
        match self.ctx.position(self.player) {
            Some(p) => (p.x, p.y),
            None => (0, 0),
        }
    }

    /// Read-only map access (renderer / headless output).
    pub fn map(&self) -> &Map {
        &self.ctx.map
    }

    /// The player's rendered tile id (from its `RenderTile` component).
    pub fn player_tile(&self) -> String {
        self.ctx
            .broker
            .world()
            .get::<&RenderTile>(self.player)
            .ok()
            .map_or_else(|| "creature.player.missing".to_string(), |r| r.tile.clone())
    }

    /// The accumulated event log, one entry per player action. Deterministic
    /// for a fixed script and map. This is the seed of the replay format.
    pub fn log(&self) -> &[String] {
        &self.log
    }

    /// Submit one player action: run the scheduler until the player's turn
    /// comes up, then dispatch the corresponding event. A wall-blocked move is
    /// cancelled by the EARLY handler (no energy charged, position unchanged).
    pub fn submit_action(&mut self, action: PlayerAction) -> anyhow::Result<TurnReport> {
        if let Err(stalled) = self.scheduler.advance_until_ready() {
            anyhow::bail!("{stalled}");
        }
        let from = self
            .ctx
            .position(self.player)
            .map(|p| (p.x, p.y))
            .unwrap_or((0, 0));
        let turn = match action {
            PlayerAction::Move { dx, dy } => {
                TurnAction::MoveTo(Point::new(from.0 + dx, from.1 + dy))
            }
            PlayerAction::Wait => TurnAction::Wait,
        };
        let report = self.scheduler.take_turn(&mut self.ctx, self.player, turn)?;
        self.log.push(self.format_log_line(&report, from));
        Ok(report)
    }

    /// Pretty-print one event-log line: stable one-event-per-line format
    /// `KEY <Event::NAME> tick=..` with the outcome for moves.
    fn format_log_line(&self, report: &TurnReport, from: (i32, i32)) -> String {
        match report.action {
            TurnAction::MoveTo(to) => {
                let outcome = if report.charged {
                    "handled"
                } else {
                    "cancelled"
                };
                format!(
                    "EVENT Move tick={} from=({},{}) to=({},{}) {}",
                    report.tick, from.0, from.1, to.x, to.y, outcome
                )
            }
            TurnAction::Wait => format!("EVENT Wait tick={}", report.tick),
            _ => format!("EVENT {:?} tick={}", report.action, report.tick),
        }
    }

    /// Render the final map as ASCII text (uses the tile registry glyphs; the
    /// player overlays `@`). A pure reader — no dispatch, no mutation.
    pub fn text_map(&self, tiles: &TileRegistry) -> String {
        let (px, py) = self.position();
        let floor = tile_glyph(tiles, "terrain.floor");
        let wall = tile_glyph(tiles, "terrain.wall");
        let mut s = String::with_capacity(self.ctx.map.width * self.ctx.map.height);
        for y in 0..self.ctx.map.height {
            for x in 0..self.ctx.map.width {
                if (x as i32, y as i32) == (px, py) {
                    s.push('@');
                } else {
                    s.push(if self.ctx.map.is_walkable(x as i32, y as i32) {
                        floor
                    } else {
                        wall
                    });
                }
            }
            s.push('\n');
        }
        s
    }

    /// Run `n` scripted actions headlessly (no window), returning the final
    /// map text. The event log is available via [`Self::log`]. The script
    /// deterministically exercises ordinary handled movement, a wall-collision
    /// cancellation, and Wait — the full demo obligation — so CI can assert on
    /// it without a display.
    pub fn run_headless(&mut self, tiles: &TileRegistry, n: u32) -> String {
        for i in 0..n {
            let action = headless_script(i as usize);
            // A wall block or other bus error is a content bug: surface it as
            // part of the run rather than silently swallowing (a cancelled move
            // must never look like it cost energy).
            if let Err(e) = self.submit_action(action) {
                self.log
                    .push(format!("ERROR tick={} {e}", self.scheduler.ticks()));
                break;
            }
        }
        self.text_map(tiles)
    }
}

/// Deterministic headless action script. Cycle of `[right, right, down, down,
/// wait]`, so within a few steps the player walks into a map collision (the
/// pillar at x%8==4 && y%6==3) and the cancellation path is exercised.
fn headless_script(i: usize) -> PlayerAction {
    match i % 5 {
        0 => PlayerAction::Move { dx: 1, dy: 0 },
        1 => PlayerAction::Move { dx: 1, dy: 0 },
        2 => PlayerAction::Move { dx: 0, dy: 1 },
        3 => PlayerAction::Move { dx: 0, dy: 1 },
        _ => PlayerAction::Wait,
    }
}

/// The glyph for a tile id, falling back to `'?'`.
fn tile_glyph(tiles: &TileRegistry, id: &str) -> char {
    match tiles.get(id).visual {
        tessera_core::tiles::TileVisual::Glyph { ch } => ch,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_core::events::Position;

    /// A demo fixture: one positioned, scheduled player in a demo map, with a
    /// spawn-order entity id and a `Position` part.
    fn fixture() -> Sim {
        let mut broker = EntityBroker::new();
        let player = broker.spawn();
        broker
            .insert_part(player, PART_ENERGY, Box::new(ActorEnergy::new(100)))
            .unwrap();
        broker
            .insert_part(player, tessera_core::events::PART_POSITION, {
                use tessera_core::ecs::Part;
                Box::new(Position { x: 1, y: 1 }) as Box<dyn Part>
            })
            .unwrap();
        let mut sim = Sim::new(broker, Map::demo(20, 10), player);
        sim.log.clear(); // the harness doesn't log setup
        sim
    }

    #[test]
    fn ordinary_move_is_handled_and_commits_position() {
        let mut sim = fixture();
        sim.submit_action(PlayerAction::Move { dx: 1, dy: 0 })
            .unwrap();
        assert_eq!(sim.position(), (2, 1), "player moved one cell right");
        let line = &sim.log()[0];
        assert!(line.contains("Move") && line.contains("handled"), "{line}");
    }

    #[test]
    fn wall_blocked_move_is_cancelled_no_position_change() {
        let mut sim = fixture();
        // (0,?) is out of bounds to the left -> wall block at EARLY.
        sim.submit_action(PlayerAction::Move { dx: -1, dy: 0 })
            .unwrap();
        assert_eq!(sim.position(), (1, 1), "cancelled move leaves position");
        let line = &sim.log()[0];
        assert!(
            line.contains("Move") && line.contains("cancelled"),
            "{line}"
        );
    }

    #[test]
    fn wait_records_a_turn() {
        let mut sim = fixture();
        sim.submit_action(PlayerAction::Wait).unwrap();
        assert_eq!(sim.position(), (1, 1));
        assert!(sim.log()[0].contains("Wait"), "{}", sim.log()[0]);
    }

    #[test]
    fn headless_run_is_deterministic_and_covers_all_paths() {
        let tiles = TileRegistry::new();
        let run = || {
            let mut sim = fixture();
            let map = sim.run_headless(&tiles, 12);
            (map, sim.log().to_vec())
        };
        let (map_a, log_a) = run();
        let (map_b, log_b) = run();
        assert_eq!(map_a, map_b, "final map identical across runs");
        assert_eq!(log_a, log_b, "event log identical across runs");

        // Full coverage of the demo obligation in one headless run:
        // ordinary handled move, wall cancellation, and Wait.
        assert!(
            log_a
                .iter()
                .any(|l| l.contains("Move") && l.contains("handled")),
            "expected a handled Move, got {log_a:?}"
        );
        assert!(
            log_a
                .iter()
                .any(|l| l.contains("Move") && l.contains("cancelled")),
            "expected a cancelled wall Move, got {log_a:?}"
        );
        assert!(
            log_a.iter().any(|l| l.contains("Wait")),
            "expected a Wait, got {log_a:?}"
        );
    }

    #[test]
    fn log_lines_are_stable_one_event_per_line() {
        let mut sim = fixture();
        for _ in 0..5 {
            sim.submit_action(PlayerAction::Move { dx: 1, dy: 0 })
                .unwrap();
        }
        for line in sim.log() {
            assert!(line.starts_with("EVENT "), "unexpected log line: {line}");
        }
    }
}
