//! Headless debug console: line grammar → typed [`ConsoleCommand`] → executor.
//!
//! This is the *engine* half of the debug console: the parser is pure (no sim),
//! and the executor runs commands **between** turns against the live [`Sim`]
//! through the public surfaces that module exposes, appending a deterministic
//! `CMD ...` line to the sim's event log for every executed command (so a
//! headless replay log is a complete record of how a state was reached).
//!
//! The windowed egui transcript/input panel is a separate later change; only
//! the parser + executor live here, and they are compiled unconditionally (no
//! egui dep).
//!
//! Grammar (whitespace-tokenized; `#` starts a comment; numbers are `i32`/`u64`):
//! ```text
//! help                              — list commands
//! list                              — list blueprint ids in the loaded library
//! entities                          — list live entities: <id> <blueprint> <pos> <hp>
//! spawn <blueprint> [x y]           — spawn; default = zone start tile
//! kill <entity-id> | kill all <blueprint>
//! print <entity-id>                 — dump an entity's parts (the inspector text path)
//! sethp <entity-id> <n>             — write the Health part
//! setpart <entity-id> <Attributes|Level> <key>=<value> ... — write a stat part (recomputes Derived)
//! tp <entity-id> <x> <y>            — teleport (writes Position)
//! save [name]                       — write a save (default name "quicksave")
//! load <name>                       — replace live state from a save
//! clear                             — clear the transcript
//! ```
//! Malformed input (missing args, non-numeric numbers, unknown commands)
//! returns a usage error string — never panics.

use std::path::{Path, PathBuf};

use hecs::Entity;
use tessera_core::ecs::{EntityBroker, PartTypeId};
use tessera_core::events::{Health, PART_HEALTH, PART_POSITION, Position};
use tessera_core::save::{SaveError, SaveHooks, load as load_save_file, write_atomic};
use tessera_core::world::World;

use crate::scripting::entity_to_id;
use crate::sim::Sim;

/// A parsed console command. The typed grammar the executor dispatches on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsoleCommand {
    Help,
    List,
    Entities,
    Spawn {
        blueprint: String,
        at: Option<(i32, i32)>,
    },
    Kill {
        id: u64,
    },
    KillAll {
        blueprint: String,
    },
    Print {
        id: u64,
    },
    SetHp {
        id: u64,
        n: i32,
    },
    SetPart {
        id: u64,
        part: String,
        fields: Vec<(String, i64)>,
    },
    Tp {
        id: u64,
        x: i32,
        y: i32,
    },
    Save {
        name: String,
    },
    Load {
        name: String,
    },
    Clear,
}

/// The result of running one console command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsoleOutcome {
    /// A normal result; the string is the transcript/CLI line to render.
    Ok(String),
    /// A command-level error (bad id, missing part, ...). The string is shown.
    Err(String),
    /// `load` replaced live state; the windowed UI must re-sync, headless just
    /// reports the save it loaded.
    ReloadWorld,
}

/// Parse one console line into a typed [`ConsoleCommand`].
///
/// Whitespace-tokenized; a `#` starts a comment (the rest of the line is
/// ignored). Malformed numeric / missing arguments and unknown commands
/// return a usage error string — never panic.
pub fn parse(line: &str) -> Result<ConsoleCommand, String> {
    // Strip a trailing `# comment`.
    let line = match line.find('#') {
        Some(i) => &line[..i],
        None => line,
    };
    let mut toks: Vec<&str> = line.split_whitespace().collect();
    if toks.is_empty() {
        // Blank / comment-only line: nothing to execute.
        return Ok(ConsoleCommand::Clear);
    }
    let cmd = toks.remove(0);
    match cmd {
        "help" => take_no_args(cmd, &toks).map(|_| ConsoleCommand::Help),
        "list" => take_no_args(cmd, &toks).map(|_| ConsoleCommand::List),
        "entities" => take_no_args(cmd, &toks).map(|_| ConsoleCommand::Entities),
        "clear" => take_no_args(cmd, &toks).map(|_| ConsoleCommand::Clear),
        "spawn" => parse_spawn(&toks),
        "kill" => parse_kill(&toks),
        "print" => {
            let id = one_u64_arg(&toks)?;
            if toks.len() != 1 {
                return Err("usage: print <entity-id>".into());
            }
            Ok(ConsoleCommand::Print { id })
        }
        "sethp" => {
            if toks.len() != 2 {
                return Err("usage: sethp <entity-id> <n>".into());
            }
            let id = parse_entity_id(toks[0])?;
            let n = parse_i32(toks[1], "n")?;
            Ok(ConsoleCommand::SetHp { id, n })
        }
        "setpart" => parse_setpart(&toks),
        "tp" => {
            if toks.len() != 3 {
                return Err("usage: tp <entity-id> <x> <y>".into());
            }
            let id = parse_entity_id(toks[0])?;
            let x = parse_i32(toks[1], "x")?;
            let y = parse_i32(toks[2], "y")?;
            Ok(ConsoleCommand::Tp { id, x, y })
        }
        "save" => {
            if toks.len() > 1 {
                return Err("usage: save [name]".into());
            }
            let name = toks
                .first()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "quicksave".into());
            Ok(ConsoleCommand::Save { name })
        }
        "load" => {
            if toks.len() != 1 {
                return Err("usage: load <name>".into());
            }
            Ok(ConsoleCommand::Load {
                name: toks[0].to_string(),
            })
        }
        other => Err(format!(
            "unknown command '{other}' — try 'help' \
             (commands: help list entities spawn kill print sethp setpart tp save load clear)"
        )),
    }
}

fn parse_spawn(toks: &[&str]) -> Result<ConsoleCommand, String> {
    match toks.len() {
        0 => Err("usage: spawn <blueprint> [x y]".into()),
        1 => Ok(ConsoleCommand::Spawn {
            blueprint: toks[0].to_string(),
            at: None,
        }),
        3 => Ok(ConsoleCommand::Spawn {
            blueprint: toks[0].to_string(),
            at: Some((parse_i32(toks[1], "x")?, parse_i32(toks[2], "y")?)),
        }),
        _ => Err("usage: spawn <blueprint> [x y]".into()),
    }
}

fn parse_kill(toks: &[&str]) -> Result<ConsoleCommand, String> {
    match toks {
        [id] => Ok(ConsoleCommand::Kill {
            id: parse_entity_id(id)?,
        }),
        ["all", bp] => Ok(ConsoleCommand::KillAll {
            blueprint: (*bp).to_string(),
        }),
        _ => Err("usage: kill <entity-id> | kill all <blueprint>".into()),
    }
}

/// `setpart <entity-id> <part> <key>=<value> ...` — the console stat-write
/// path . Only `Attributes`/`Level` are writable; any other
/// part name is rejected at execution, and the native bounds are invariants.
fn parse_setpart(toks: &[&str]) -> Result<ConsoleCommand, String> {
    if toks.len() < 3 {
        return Err("usage: setpart <entity-id> <Attributes|Level> <key>=<value> ...".into());
    }
    let id = parse_entity_id(toks[0])?;
    let part = toks[1].to_string();
    let mut fields = Vec::new();
    for tok in &toks[2..] {
        let (key, value) = tok
            .split_once('=')
            .ok_or_else(|| format!("field '{tok}' must be `key=value`"))?;
        let value = value
            .parse()::<i64>
            .map_err(|_| format!("field '{tok}': value must be an integer"))?;
        fields.push((key.to_string(), value));
    }
    Ok(ConsoleCommand::SetPart { id, part, fields })
}

fn take_no_args(cmd: &str, toks: &[&str]) -> Result<(), String> {
    if toks.is_empty() {
        Ok(())
    } else {
        Err(format!("usage: {cmd} (takes no arguments)"))
    }
}

fn one_u64_arg(toks: &[&str]) -> Result<u64, String> {
    match toks {
        [tok] => parse_entity_id(tok),
        _ => Err("expected a single entity-id argument".into()),
    }
}

fn parse_entity_id(tok: &str) -> Result<u64, String> {
    tok.parse()::<u64>
        .map_err(|_| format!("'{tok}' is not a valid entity id"))
}

fn parse_i32(tok: &str, what: &str) -> Result<i32, String> {
    tok.parse()::<i32>
        .map_err(|_| format!("'{what}' must be an integer, got '{tok}'"))
}

/// The file path a named save maps to inside the save dir (`<name>.tess`).
fn save_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.tess"))
}

// ---------------------------------------------------------------------------
// Executor
// ---------------------------------------------------------------------------

/// The executor context: everything a successful command needs beyond the sim.
/// Kept as a small struct so `execute` needs just one context argument and the
/// windowed UI can reuse it directly.
pub struct ConsoleContext<'a> {
    pub world: &'a World,
    pub save_hooks: &'a SaveHooks,
    pub save_dir: &'a Path,
}

/// Execute one parsed command against a live [`Sim`].
///
/// Commands run **between** turns (never during event dispatch). Every command
/// appends a deterministic `CMD <verb> ... tick=<tick>` line to the sim's
/// event log. `load` replaces live state and returns [`ConsoleOutcome::ReloadWorld`].
/// A command-level failure (bad id, missing part, unknown blueprint) returns
/// [`ConsoleOutcome::Err`] — never panics.
pub fn execute(sim: &mut Sim, ctx: &ConsoleContext<'_>, cmd: ConsoleCommand) -> ConsoleOutcome {
    match cmd {
        ConsoleCommand::Help => ok_quiet(sim, "help", help_text),
        ConsoleCommand::List => {
            let lib = ctx
                .world
                .library
                .read()
                .unwrap_or_else(|p| p.into_inner());
            let ids: Vec<String> = lib.ids.cloned().collect();
            let text = if ids.is_empty() {
                "(no blueprints loaded)".to_string()
            } else {
                ids.join("\n")
            };
            ok_quiet(sim, "list", text)
        }
        ConsoleCommand::Entities => ok_quiet(sim, "entities", entities_text(sim, ctx.world)),
        ConsoleCommand::Spawn { blueprint, at } => {
            let tick = sim.ticks();
            let (x, y) = at.unwrap_or(sim_position_or_start(sim));
            match sim.spawn_blueprint_console(ctx.world, &blueprint, at) {
                Ok(e) => {
                    let id = entity_to_id(e);
                    sim.log_push(format!(
                        "CMD spawn {blueprint} tick={tick} at={x},{y} id={id}"
                    ));
                    ConsoleOutcome::Ok(format!("spawned {blueprint} id={id} at ({x},{y})"))
                }
                Err(e) => ConsoleOutcome::Err(format!("spawn {blueprint}: {e}")),
            }
        }
        ConsoleCommand::Kill { id } => {
            let tick = sim.ticks();
            match sim.entity_from_id(id) {
                Some(e) => match sim.despawn_entity(e) {
                    Ok(()) => {
                        sim.log_push(format!("CMD kill {id} tick={tick}"));
                        ConsoleOutcome::Ok(format!("killed entity {id}"))
                    }
                    Err(e) => ConsoleOutcome::Err(format!("kill {id}: {e}")),
                },
                None => ConsoleOutcome::Err(format!("no live entity {id}")),
            }
        }
        ConsoleCommand::KillAll { blueprint } => {
            let tick = sim.ticks();
            let ids = entities_by_blueprint(sim, ctx.world, &blueprint);
            let mut killed = 0u64;
            for (id, e) in ids {
                if sim.despawn_entity(e).is_ok() {
                    killed += 1;
                    let _ = id;
                }
            }
            sim.log_push(format!("CMD kill all {blueprint} tick={tick} n={killed}"));
            ConsoleOutcome::Ok(format!("killed {killed} {blueprint}"))
        }
        ConsoleCommand::Print { id } => {
            let tick = sim.ticks();
            match sim.entity_from_id(id) {
                Some(e) => {
                    let text = print_text(sim, ctx.save_hooks(), e);
                    sim.log_push(format!("CMD print {id} tick={tick}"));
                    ConsoleOutcome::Ok(text)
                }
                None => ConsoleOutcome::Err(format!("no live entity {id}")),
            }
        }
        ConsoleCommand::SetHp { id, n } => {
            let tick = sim.ticks();
            match (sim.entity_from_id(id), ) {
                (Some(e), ) => match sim.set_entity_hp(e, n) {
                    Ok(()) => {
                        sim.log_push(format!("CMD sethp {id} {n} tick={tick}"));
                        ConsoleOutcome::Ok(format!("entity {id} hp set to {n}"))
                    }
                    Err(e) => ConsoleOutcome::Err(format!("sethp {id}: {e}")),
                },
                (None, _) => ConsoleOutcome::Err(format!("no live entity {id}")),
            }
        }
        ConsoleCommand::SetPart { id, part, fields } => {
            let tick = sim.ticks();
            let fields_map: std::collections::BTreeMap<String, i64> = fields.into_iter().collect();
            match sim.entity_from_id(id) {
                Some(e) => match sim.set_part_console(e, &part, &fields_map) {
                    Ok(()) => {
                        sim.log_push(format!("CMD setpart {id} {part} tick={tick}"));
                        ConsoleOutcome::Ok(format!("entity {id} {part} updated"))
                    }
                    Err(e) => ConsoleOutcome::Err(format!("setpart {id} {part}: {e}")),
                },
                None => ConsoleOutcome::Err(format!("no live entity {id}")),
            }
        }
        ConsoleCommand::Tp { id, x, y } => {
            let tick = sim.ticks();
            match sim.entity_from_id(id) {
                Some(e) => match sim.set_entity_position(e, x, y) {
                    Ok(()) => {
                        sim.log_push(format!("CMD tp {id} {x} {y} tick={tick}"));
                        ConsoleOutcome::Ok(format!("entity {id} moved to ({x},{y})"))
                    }
                    Err(e) => ConsoleOutcome::Err(format!("tp {id}: {e}")),
                },
                None => ConsoleOutcome::Err(format!("no live entity {id}")),
            }
        }
        ConsoleCommand::Save { name } => {
            let tick = sim.ticks();
            match save_named(sim, ctx, &name) {
                Ok(path) => {
                    sim.log_push(format!("CMD save {name} tick={tick}"));
                    ConsoleOutcome::Ok(format!("saved to {}", path.display()))
                }
                Err(e) => ConsoleOutcome::Err(format!("save {name}: {e}")),
            }
        }
        ConsoleCommand::Load { name } => {
            let tick = sim.ticks();
            let path = save_path(ctx.save_dir, &name);
            match load_save_file(&path) {
                Ok(data) => match sim.restore_from_save(data, ctx.save_hooks()) {
                    Ok(()) => {
                        // The loaded sim has a fresh (empty) log; record the
                        // load so the replay log is self-describing.
                        sim.log_push(format!("CMD load {name} tick={tick}"));
                        ConsoleOutcome::ReloadWorld
                    }
                    Err(e) => ConsoleOutcome::Err(format!("load {name}: {e}")),
                },
                Err(e) => ConsoleOutcome::Err(format!("load {name}: {e}")),
            }
        }
        ConsoleCommand::Clear => ConsoleOutcome::Ok("cleared".to_string()),
    }
}

/// A successful command result with a quiet `CMD` line (reads: help/list/entities).
fn ok_quiet(sim: &mut Sim, verb: &str, text: String) -> ConsoleOutcome {
    sim.log_push(format!("CMD {verb} tick={}", sim.ticks()));
    ConsoleOutcome::Ok(text)
}

fn help_text() -> String {
    "commands:\n  help                              list commands\n  \
     list                              list blueprint ids in the loaded library\n  \
     entities                          list live entities: <id> <blueprint> <pos> <hp>\n  \
     spawn <blueprint> [x y]           spawn; default = zone start tile\n  \
     kill <entity-id> | kill all <blueprint>\n  \
     print <entity-id>                 dump an entity's parts (the inspector text path)\n  \
     sethp <entity-id> <n>             write the Health part\n  \
     setpart <entity-id> <part> <k>=<v>... write Attributes/Level (recomputes Derived)\n  \
     tp <entity-id> <x> <y>            teleport (writes Position)\n  \
     save [name]                       write a save (default name \"quicksave\")\n  \
     load <name>                       replace live state from a save\n  \
     clear                             clear the transcript"
        .to_string()
}

fn sim_position_or_start(sim: &Sim) -> (i32, i32) {
    sim.position()
}

/// The `entities` transcript: one `<id> <blueprint> (x,y) hp=<n>` line per live
/// positioned entity, in deterministic entity-id order.
fn entities_text(sim: &Sim, world: &World) -> String {
    let b = sim.broker().read().unwrap_or_else(|p| p.into_inner());
    let mut rows: Vec<(u64, String)> = b
        .entities_with_part(PART_POSITION)
        .into_iter()
        .map(|e| {
            let id = entity_to_id(e);
            let (x, y, hp) = row_of(&b, e);
            (
                id,
                format!("{id} {} ({x},{y}) hp={hp}", blueprint_of(world, &b, e)),
            )
        })
        .collect();
    if rows.is_empty() {
        return "(no entities)".to_string();
    }
    rows.sort_by_key(|(id, _)| *id);
    rows.into_iter()
        .map(|(_, line)| line)
        .collect()::<Vec<_>>
        .join("\n")
}

fn row_of(b: &EntityBroker, e: Entity) -> (i32, i32, i32) {
    let pos = b
        .typed_part::<Position>(e, &PART_POSITION.into())
        .map(|p| (p.x, p.y))
        .unwrap_or((0, 0));
    let hp = b
        .typed_part::<Health>(e, &PART_HEALTH.into())
        .map(|h| h.hp)
        .unwrap_or(0);
    (pos.0, pos.1, hp)
}

/// The `print` inspector text path: one `name = value` per part (values via the
/// save whitelist hooks so script parts render too), in deterministic
/// (sorted-name) order.
fn print_text(sim: &Sim, save_hooks: &SaveHooks, e: Entity) -> String {
    let b = sim.broker().read().unwrap_or_else(|p| p.into_inner());
    let mut lines = Vec::new();
    for (name_pt, part) in b
        .part_map(e)
        .map(|m| m.iter().collect()::<Vec<_>>)
        .unwrap_or_default()
    {
        let name = name_pt.as_str();
        match save_hooks.snapshot(&PartTypeId::new(name), part) {
            Ok(v) => lines.push(format!("{name} = {v}")),
            Err(_) => lines.push(format!("{name} = <unserializable>")),
        }
    }
    if lines.is_empty() {
        "(no parts)".to_string()
    } else {
        lines.join("\n")
    }
}

/// All live entities whose fingerprint blueprint == `bp`, as (id, entity) pairs
/// in deterministic order.
fn entities_by_blueprint(sim: &Sim, world: &World, bp: &str) -> Vec<(u64, Entity)> {
    let b = sim.broker().read().unwrap_or_else(|p| p.into_inner());
    b.entities_with_part(PART_POSITION)
        .into_iter()
        .filter(|e| blueprint_of(world, &b, *e) == bp)
        .map(|e| (entity_to_id(e), e))
        .collect()
}

/// Fingerprint which blueprint a live entity was spawned from, by matching its
/// `Render` tile id against blueprints' `parts.Render.tile`. Falls back to the
/// tile id itself when no blueprint declares it (still a stable, searchable
/// label for `kill all`).
fn blueprint_of(world: &World, b: &EntityBroker, e: Entity) -> String {
    use tessera_core::world::RenderTile;
    let Ok(rt) = b.world().get::<&RenderTile>(e) else {
        return "<no-render>".to_string();
    };
    let lib = world.library().read().unwrap_or_else(|p| p.into_inner());
    for id in lib.ids {
        if let Some(bp) = lib.get(id)
            && let Some(t) = bp
                .part("Render")
                .and_then(|v| v.get("tile"))
                .and_then(|v| v.as_str())
            && t == rt.tile
        {
            return id.clone();
        }
    }
    rt.tile.clone()
}

/// Save the current sim state to `save_dir/<name>.tess` (atomic). Returns the
/// written path on success.
fn save_named(sim: &Sim, ctx: &ConsoleContext<'_>, name: &str) -> Result<PathBuf, SaveError> {
    let data = sim.save(ctx.save_hooks())?;
    let path = save_path(ctx.save_dir, name);
    if let Some(parent) = path.parent {
        std::fs::create_dir_all(parent).map_err(|src| SaveError::WriteFailed {
            path: parent.into(),
            source: src,
        })?;
    }
    write_atomic(&path, &data)?;
    Ok(path)
}

/// The windowed console panel's client-side state: the rendered transcript, the
/// line being typed, and whether the panel is visible (backtick toggles).
///
/// This is deliberately ungated (no egui dependency): the transcript/command
/// logic — including the invariant that *console commands never move the
/// player* — is unit-testable under a plain `cargo test`, and the feature-gated
/// egui module is only a thin renderer over this state.
///
/// In a default (non-`devtools`) binary build the struct has no non-test
/// callers, so suppress the dead-code lint there (it is constructed by the
/// feature-gated UI and by tests).
#[cfg_attr(not(feature = "devtools"), allow(dead_code))]
#[derive(Debug, Default)]
pub struct ConsolePanel {
    /// Rendered transcript lines, oldest first.
    pub transcript: Vec<String>,
    /// The line currently being typed in the input field.
    pub input: String,
    /// Whether the panel is currently shown (backtick toggles).
    pub visible: bool,
}

#[cfg_attr(not(feature = "devtools"), allow(dead_code))]
impl ConsolePanel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Submit the current input line: echo it, parse + execute it, and append
    /// the outcome to the transcript. `clear` wipes the transcript. Never
    /// panics on malformed input (parse errors render as an `error:` line).
    pub fn submit(&mut self, sim: &mut Sim, ctx: &ConsoleContext<'_>) {
        let line = std::mem::take(&mut self.input);
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        self.transcript.push(format!("> {line}"));
        match parse(line) {
            Ok(ConsoleCommand::Clear) => self.transcript.clear(),
            Ok(cmd) => match execute(sim, ctx, cmd) {
                ConsoleOutcome::Ok(text) => self.transcript.extend(text.lines().map(str::to_owned)),
                ConsoleOutcome::Err(e) => self.transcript.push(format!("error: {e}")),
                ConsoleOutcome::ReloadWorld => {
                    self.transcript.push("(world reloaded from save)".into())
                }
            },
            Err(e) => self.transcript.push(format!("error: {e}")),
        }
        // Bound the transcript so a long session does not grow unbounded.
        if self.transcript.len() > 200 {
            let drop = self.transcript.len() - 200;
            self.transcript.drain(..drop);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use tessera_core::ecs::Part;
    use tessera_core::events::Point;
    use tessera_core::script_part::{FieldDef, PartValue, ScriptPartType};
    use tessera_core::turns::{ActorEnergy, PART_ENERGY};
    use tessera_core::vfs::MemVfs;
    use tessera_core::world::RenderTile;
    use tessera_core::zone::Zone;

    // ---- parser tests ----------------------------------------------------

    #[test]
    fn parse_help_list_entities_clear() {
        assert_eq!(parse("help").unwrap(), ConsoleCommand::Help);
        assert_eq!(parse("list").unwrap(), ConsoleCommand::List);
        assert_eq!(parse("entities").unwrap(), ConsoleCommand::Entities);
        assert_eq!(parse("clear").unwrap(), ConsoleCommand::Clear);
        // whitespace / comments around commands
        assert_eq!(parse("  help  ").unwrap(), ConsoleCommand::Help);
        assert_eq!(parse("help # comment").unwrap(), ConsoleCommand::Help);
        assert_eq!(parse("# only a comment").unwrap(), ConsoleCommand::Clear);
    }

    #[test]
    fn parse_no_args_rejects_trailing_tokens() {
        assert!(parse("help x").is_err());
        assert!(parse("list 1").is_err());
        assert!(parse("entities extra").is_err());
        assert!(parse("clear all").is_err());
    }

    #[test]
    fn parse_spawn_happy_paths() {
        assert_eq!(
            parse("spawn Goblin").unwrap(),
            ConsoleCommand::Spawn {
                blueprint: "Goblin".into(),
                at: None
            }
        );
        assert_eq!(
            parse("spawn Goblin 5 5").unwrap(),
            ConsoleCommand::Spawn {
                blueprint: "Goblin".into(),
                at: Some((5, 5))
            }
        );
        // comment stripped after coords
        assert_eq!(
            parse("spawn Goblin 3 -2 # where").unwrap(),
            ConsoleCommand::Spawn {
                blueprint: "Goblin".into(),
                at: Some((3, -2))
            }
        );
    }

    #[test]
    fn parse_spawn_malformed() {
        // Missing blueprint arg.
        assert!(parse("spawn").is_err());
        assert_eq!(
            parse("spawn").unwrap_err(),
            "usage: spawn <blueprint> [x y]"
        );
        // Only one coordinate.
        assert!(parse("spawn Goblin 5").is_err());
        // Non-numeric coordinate.
        assert!(parse("spawn Goblin x 5").is_err());
        assert!(parse("spawn Goblin 5 y").is_err());
        // Too many args.
        assert!(parse("spawn Goblin 5 5 6").is_err());
    }

    #[test]
    fn parse_kill_happy_paths() {
        assert_eq!(parse("kill 42").unwrap(), ConsoleCommand::Kill { id: 42 });
        assert_eq!(
            parse("kill all Goblin").unwrap(),
            ConsoleCommand::KillAll {
                blueprint: "Goblin".into()()
            }
        );
    }

    #[test]
    fn parse_kill_malformed() {
        // Missing argument.
        assert!(parse("kill").is_err());
        // Non-numeric entity id.
        assert!(parse("kill abc").is_err());
        // `all` without a blueprint.
        assert!(parse("kill all").is_err());
        // Too many tokens.
        assert!(parse("kill 1 2").is_err());
    }

    #[test]
    fn parse_print_happy_and_malformed() {
        assert_eq!(parse("print 7").unwrap(), ConsoleCommand::Print { id: 7 });
        // Missing id / non-numeric.
        assert!(parse("print").is_err());
        assert!(parse("print abc").is_err());
        assert!(parse("print 1 2").is_err());
    }

    #[test]
    fn parse_sethp_happy_and_malformed() {
        assert_eq!(
            parse("sethp 3 15").unwrap(),
            ConsoleCommand::SetHp { id: 3, n: 15 }
        );
        assert!(parse("sethp").is_err());
        assert!(parse("sethp 3").is_err());
        assert!(parse("sethp 3 abc").is_err());
        assert!(parse("sethp abc 3").is_err());
    }

    #[test]
    fn parse_tp_happy_and_malformed() {
        assert_eq!(
            parse("tp 3 10 20").unwrap(),
            ConsoleCommand::Tp {
                id: 3,
                x: 10,
                y: 20
            }
        );
        assert!(parse("tp").is_err());
        assert!(parse("tp 3").is_err());
        assert!(parse("tp 3 10").is_err());
        // Non-numeric x.
        assert!(parse("tp 3 x 20").is_err());
        // Non-numeric y (the exact malformed coordinate case required).
        assert!(parse("tp 3 10 y").is_err());
        assert!(parse("tp abc 10 20").is_err());
    }

    #[test]
    fn parse_save_load_happy_and_malformed() {
        assert_eq!(
            parse("save").unwrap(),
            ConsoleCommand::Save {
                name: "quicksave".into()()
            }
        );
        assert_eq!(
            parse("save mygame").unwrap(),
            ConsoleCommand::Save {
                name: "mygame".into()()
            }
        );
        assert_eq!(
            parse("load mygame").unwrap(),
            ConsoleCommand::Load {
                name: "mygame".into()()
            }
        );
        // save with too many names; load missing / extra.
        assert!(parse("save a b").is_err());
        assert!(parse("load").is_err());
        assert!(parse("load a b").is_err());
    }

    #[test]
    fn parse_unknown_command_is_usage_error() {
        assert!(
            parse("teleport 1 2 3")
                .unwrap_err()
                .contains("unknown command")
        );
        assert!(!parse("").is_err()); // blank -> Clear
    }

    // ---- executor tests --------------------------------------------------

    /// A minimal world with `Player` and `Goblin` blueprints, mirroring the
    /// engine registrations (Render component + Position/ActorEnergy/Health
    /// dynamic parts + a `Charge` script part; the native `Attributes`/`Level`/
    /// `Bonuses` parts come from `World::new()`).
    fn test_world() -> World {
        let mut w = World::new();
        w.register_component::<RenderTile>("Render").unwrap();
        w.register_part::<Position>(PART_POSITION).unwrap();
        w.register_part::<ActorEnergy>(PART_ENERGY).unwrap();
        w.register_part::<Health>(PART_HEALTH).unwrap();
        let mut schema = BTreeMap::new();
        schema.insert(
            "charges".to_string(),
            FieldDef::int.default(PartValue::Int(1)).min(0).max(99),
        );
        w.register_script_part(ScriptPartType::new("Charge", schema))
            .unwrap();
        let vfs = MemVfs::new().with(
            "blueprints/creatures.toml",
            r##"
[Actor]
parts.Health = { hp = 20 }
# Legacy console test fixture: no Attributes/Level, so these entities take
# the pre-derivation path (no Derived cache, ActorEnergy speed) and the console
# command tests stay about commands, not stats.

[Humanoid]
inherits = "Actor"
parts.Render = { tile = "creature.humanoid" }
parts.ActorEnergy = { speed = 100 }

[Player]
inherits = "Humanoid"
parts.Render = { tile = "creature.player()" }

[Goblin]
inherits = "Humanoid"
parts.Render = { tile = "creature.goblin" }
parts.Health = { hp = 3 }
parts.Charge = { charges = 2 }
"##,
        );
        w.load(&vfs).unwrap();
        w
    }

    /// A demo-zone sim with a positioned, scheduled player.
    fn test_zone() -> Zone {
        let mut cells = vec!["terrain.floor".to_string(); 20 * 10];
        for y in 0..10 {
            for x in 0..20 {
                if x == 0 || y == 0 || x == 19 || y == 9 {
                    cells[y * 20 + x] = "terrain.wall".to_string();
                }
            }
        }
        Zone::from_cells("test", 20, 10, Point::new(1, 1), cells)
    }

    fn test_sim() -> (Sim, hecs::Entity) {
        let mut broker = EntityBroker::new();
        let player = broker.spawn();
        broker
            .insert_part(player, PART_ENERGY, Box::new(ActorEnergy::new(100)))
            .unwrap();
        broker
            .insert_part(
                player,
                PART_POSITION,
                Box::new(Position { x: 1, y: 1 }) as Box<dyn Part>,
            )
            .unwrap();
        broker
            .insert_part(
                player,
                PART_HEALTH,
                Box::new(Health { hp: 20 }) as Box<dyn Part>,
            )
            .unwrap();
        let sim = Sim::new(broker, test_zone, player);
        (sim, player)
    }

    fn temp_dir() -> std::path::PathBuf {
        let base = std::env::temp_dir();
        let dir = base.join(format!(
            "tessera-console-test-{}-{}",
            std::process::id,
            std::time::SystemTime::now
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Execute a line through parse + execute against a fresh sim, returning
    /// `(sim, outcome)`.
    fn run_line(
        world: &World,
        hooks: &SaveHooks,
        dir: &std::path::Path,
        mut sim: Sim,
        line: &str,
    ) -> (Sim, ConsoleOutcome) {
        let ctx = ConsoleContext {
            world,
            save_hooks: hooks,
            save_dir: dir,
        };
        let cmd = parse(line).expect("parse");
        let out = execute(&mut sim, &ctx, cmd);
        (sim, out)
    }

    #[test]
    fn executor_spawn_appends_cmd_and_lists_entity() {
        let world = test_world();
        let hooks = world.save_hooks();
        let dir = temp_dir();
        let (mut sim, _p) = test_sim;
        let ctx = ConsoleContext {
            world: &world,
            save_hooks: &hooks,
            save_dir: &dir,
        };

        let out = execute(&mut sim, &ctx, parse("spawn Goblin 5 5").unwrap());
        let ConsoleOutcome::Ok(text) = out else {
            panic!("spawn should succeed, got {out:?}");
        };
        assert!(text.contains("spawned Goblin"), "{text}");
        // Deterministic CMD line in the event log.
        assert!(
            sim.log.iter().any(|l| l.starts_with("CMD spawn Goblin")
                && l.contains("tick=0")
                && l.contains("at=5,5")),
            "log: {:?}",
            sim.log
        );
        // The goblin is now a live entity, listed by `entities`.
        let (sim, ent) = run_line(&world, &hooks, &dir, sim, "entities");
        let ConsoleOutcome::Ok(list) = ent else {
            panic!("entities should succeed, got {ent:?}");
        };
        assert!(
            list.contains("Goblin"),
            "entities should list the goblin:\n{list}"
        );
        let _ = sim;
    }

    #[test]
    fn executor_read_write_commands_and_cmd_lines() {
        let world = test_world();
        let hooks = world.save_hooks();
        let dir = temp_dir();
        let (mut sim, _p) = test_sim;
        let ctx = ConsoleContext {
            world: &world,
            save_hooks: &hooks,
            save_dir: &dir,
        };

        // spawn + capture id from the CMD line in the event log.
        execute(&mut sim, &ctx, parse("spawn Goblin 8 8").unwrap());
        let id = {
            let line = sim
                .log
                .iter()
                .find(|l| l.starts_with("CMD spawn Goblin"))
                .expect("spawn CMD line");
            let pos = line.rfind("id=").unwrap() + 3;
            line[pos..].parse()::<u64>.unwrap()
        };

        // sethp (write) against the spawned goblin.
        let ConsoleOutcome::Ok(_) =
            execute(&mut sim, &ctx, parse(&format!("sethp {id} 99")).unwrap())
        else {
            panic!("sethp should succeed");
        };
        // tp (write).
        let ConsoleOutcome::Ok(_) =
            execute(&mut sim, &ctx, parse(&format!("tp {id} 2 9")).unwrap())
        else {
            panic!("tp should succeed");
        };
        // print (read) shows the written parts.
        let ConsoleOutcome::Ok(printed) =
            execute(&mut sim, &ctx, parse(&format!("print {id}")).unwrap())
        else {
            panic!("print should succeed");
        };
        assert!(printed.contains("Health"), "{printed}");
        assert!(printed.contains("Position"), "{printed}");
        assert!(printed.contains("Charge"), "{printed}");
        // kill (write).
        let ConsoleOutcome::Ok(_) = execute(&mut sim, &ctx, parse(&format!("kill {id}")).unwrap())
        else {
            panic!("kill should succeed");
        };
        // CMD lines for each write landed in the log.
        let log = sim.log.to_vec();
        assert!(log.iter().any(|l| l.contains("CMD sethp")), "{log:?}");
        assert!(log.iter().any(|l| l.contains("CMD tp")), "{log:?}");
        assert!(log.iter().any(|l| l.contains("CMD print")), "{log:?}");
        assert!(log.iter().any(|l| l.contains("CMD kill")), "{log:?}");
    }

    #[test]
    fn executor_save_load_round_trip_via_cmd_lines() {
        let world = test_world();
        let hooks = world.save_hooks();
        let dir = temp_dir();
        let (mut sim, player) = test_sim;
        let player_id = player.to_bits();
        let ctx = ConsoleContext {
            world: &world,
            save_hooks: &hooks,
            save_dir: &dir,
        };

        execute(&mut sim, &ctx, parse("spawn Goblin 4 4").unwrap());
        // save writes the file.
        let ConsoleOutcome::Ok(_) = execute(&mut sim, &ctx, parse("save g1").unwrap()) else {
            panic!("save should succeed");
        };
        let save_file = dir.join("g1.tess");
        assert!(save_file.exists, "save file written");
        assert!(
            sim.log.iter().any(|l| l.contains("CMD save g1")),
            "{:?}",
            sim.log
        );

        // Move the player, then load restores state + returns ReloadWorld.
        execute(
            &mut sim,
            &ctx,
            parse(&format!("tp {player_id} 5 5")).unwrap(),
        );
        let (sim, out) = run_line(&world, &hooks, &dir, sim, "load g1");
        assert_eq!(out, ConsoleOutcome::ReloadWorld);
        assert!(
            sim.log.iter().any(|l| l.contains("CMD load g1")),
            "fresh log still records the load: {:?}",
            sim.log
        );
        let _ = sim;
    }

    #[test]
    fn executor_errors_never_panic() {
        let world = test_world();
        let hooks = world.save_hooks();
        let dir = temp_dir();
        let (mut sim, player) = test_sim;
        let ctx = ConsoleContext {
            world: &world,
            save_hooks: &hooks,
            save_dir: &dir,
        };
        let player_id = player.to_bits();

        // Unknown blueprint.
        let ConsoleOutcome::Err(e) = execute(&mut sim, &ctx, parse("spawn Nope 1 1").unwrap())
        else {
            panic!("unknown blueprint must Err");
        };
        assert!(e.contains("Nope"), "{e}");
        // No such entity.
        assert!(matches!(
            execute(&mut sim, &ctx, parse("print 99999999").unwrap()),
            ConsoleOutcome::Err(_)
        ));
        assert!(matches!(
            execute(&mut sim, &ctx, parse("kill 99999999").unwrap()),
            ConsoleOutcome::Err(_)
        ));
        // sethp on the player works (player has Health) — and a missing-file
        // load errors, never panics.
        let ConsoleOutcome::Ok(_) = execute(
            &mut sim,
            &ctx,
            parse(&format!("sethp {player_id} 10")).unwrap(),
        ) else {
            panic!("sethp on player should succeed");
        };
        assert!(matches!(
            execute(&mut sim, &ctx, parse("load missing").unwrap()),
            ConsoleOutcome::Err(_)
        ));
        let _ = sim;
    }

    #[test]
    fn kill_all_targets_blueprint_only() {
        let world = test_world();
        let hooks = world.save_hooks();
        let dir = temp_dir();
        let (mut sim, player) = test_sim;
        let player_id = player.to_bits();
        let ctx = ConsoleContext {
            world: &world,
            save_hooks: &hooks,
            save_dir: &dir,
        };
        execute(&mut sim, &ctx, parse("spawn Goblin 1 2").unwrap());
        execute(&mut sim, &ctx, parse("spawn Goblin 3 4").unwrap());
        let ConsoleOutcome::Ok(_) = execute(&mut sim, &ctx, parse("kill all Goblin").unwrap())
        else {
            panic!("kill all should succeed");
        };
        // The two spawned goblins are gone; the player remains.
        let (sim, ent) = run_line(&world, &hooks, &dir, sim, "entities");
        let ConsoleOutcome::Ok(list) = ent else {
            panic!("{ent:?}")
        };
        assert!(!list.contains("Goblin"), "goblins killed:\n{list}");
        // The player (no Render component in this fixture() -> <no-render>) survives.
        assert!(
            list.lines()
                .any(|l| l.starts_with(&format!("{player_id} "))),
            "player survives:\n{list}"
        );
        assert!(
            sim.log.iter().any(|l| l.contains("CMD kill all Goblin")),
            "{:?}",
            sim.log
        );
        let _ = sim;
    }

    // ---- ConsolePanel (windowed transcript state) --------------------------

    #[test]
    fn console_panel_typing_does_not_move_the_player() {
        let world = test_world();
        let hooks = world.save_hooks();
        let dir = temp_dir();
        let (mut sim, player) = test_sim;
        let player_id = player.to_bits();
        let ctx = ConsoleContext {
            world: &world,
            save_hooks: &hooks,
            save_dir: &dir,
        };
        let before = sim.position();

        // "Typing" a line into the panel (the same path the egui TextEdit
        // drives on Enter) must never move the player — the console is the
        // mutation path *beside* the game, not a movement input.
        let mut panel = ConsolePanel::new();
        panel.input = "spawn Goblin 5 5".to_string();
        panel.submit(&mut sim, &ctx);
        panel.input = format!("sethp {player_id} 30");
        panel.submit(&mut sim, &ctx);
        panel.input = "entities".to_string();
        panel.submit(&mut sim, &ctx);

        // Player position is byte-for-byte unchanged by all three commands.
        assert_eq!(sim.position(), before);
        // The transcript echoes the input and shows the outcomes.
        assert!(
            panel.transcript.iter().any(|l| l == "> spawn Goblin 5 5"),
            "{:?}",
            panel.transcript
        );
        assert!(
            panel
                .transcript
                .iter()
                .any(|l| l.contains("spawned Goblin")),
            "{:?}",
            panel.transcript
        );
        assert!(
            panel.transcript.iter().any(|l| l.contains("hp set to 30")),
            "{:?}",
            panel.transcript
        );
        // Input buffer was consumed.
        assert!(panel.input.is_empty());
    }

    #[test]
    fn console_panel_clear_wipes_transcript_and_input_never_panics() {
        let world = test_world();
        let hooks = world.save_hooks();
        let dir = temp_dir();
        let (mut sim, player) = test_sim;
        let player_id = player.to_bits();
        let ctx = ConsoleContext {
            world: &world,
            save_hooks: &hooks,
            save_dir: &dir,
        };
        let mut panel = ConsolePanel::new();
        panel.input = "spawn Goblin 1 1".to_string();
        panel.submit(&mut sim, &ctx);
        assert!(!panel.transcript.is_empty());

        // `clear` empties the transcript.
        panel.input = "clear".to_string();
        panel.submit(&mut sim, &ctx);
        assert!(panel.transcript.is_empty());

        // Malformed input renders as an error line, never panics.
        panel.input = "spawn".to_string();
        panel.submit(&mut sim, &ctx);
        assert!(
            panel.transcript.iter().any(|l| l.contains("error:")),
            "{:?}",
            panel.transcript
        );

        // Blank input is a no-op.
        let before = panel.transcript.len();
        panel.input = "   ".to_string();
        panel.submit(&mut sim, &ctx);
        assert_eq!(panel.transcript.len(), before);
        let _ = (sim, player_id);
    }
}
