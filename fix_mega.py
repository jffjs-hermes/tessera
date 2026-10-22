#!/usr/bin/env python3
"""Final mega-batch: fn-items, fields-vs-methods, visibility, turbofish."""
import re

def sub(path, pairs):
    t = open(path).read()
    for a, b in pairs:
        n = t.count(a)
        if n == 0:
            print(f"  !! no match {path}: {a[:64]!r}")
        else:
            t = t.replace(a, b)
            print(f"  ok {n}x {path}: {a[:56]!r}")
    open(path, "w").write(t)

def sub_re(path, pattern, repl):
    t = open(path).read()
    t, n = re.subn(pattern, repl, t)
    open(path, "w").write(t)
    print(f"  re {n}x {path}: {pattern[:56]!r}")

sim = "crates/tessera/src/sim.rs"
sc = "crates/tessera/src/scripting.rs"
main = "crates/tessera/src/main.rs"
cons = "crates/tessera/src/devtools/console.rs"
s1 = "crates/tessera/tests/s1_stats_pipeline.rs"
ws = "crates/tessera/tests/world_seam.rs"

# --- fixer lists ---
t = open("fix_bare_calls.py").read()
t = t.replace('"sort_unstable", "trim_start", "zone_start",',
              '"sort_unstable", "trim_start", "zone_start", "is_dir", "is_file",\n    "glyph_chars", "save_hooks", "as_deref", "renderables", "request_redraw",\n    "exit", "reload", "build_cells", "player_health", "player_turns_public",\n    "display", "init", "create_proxy", "broker",')
t = t.replace('"full_registry", "mods",', '"full_registry",')
t = t.replace('FUNCS = [\n', 'FUNCS = [\n    "demo_zone", "test_world", "temp_dir", "fixture", "parse_args",\n    "try_from_default_env", "default_attributes", "with_user_event",\n    "tracing_subscriber_fmt",\n')
open("fix_bare_calls.py", "w").write(t)

# --- visibility ---
sub(sim, [("    log: Vec<String>,", "    pub log: Vec<String>,")])
sub(sc, [("    pub(crate) host: Rc<Mutex<ScriptHost>>,", "    pub host: Rc<Mutex<ScriptHost>>,")])

# --- main.rs: mods variable corruptions + fn main + event loop ---
sub(main, [
    ('let mut mods() = PathBuf::from("data/mods()");', 'let mut mods = PathBuf::from("data/mods");'),
    ("mods(),", "mods,"),  # struct literal field - done above
])
sub(main, [
    ("    mods(),", "    mods,"),
    ("// Discover mods: directory + zip sources under the mods() root.", "// Discover mods: directory + zip sources under the mods root."),
    ("if mods().is_dir {", "if mods.is_dir() {"),
    ("tessera::mods::discover_sources(mods())?", "tessera::mods::discover_sources(&mods)?"),
    ("fn main() -> Result<> {", "fn main() -> anyhow::Result<()> {"),
    ("let args = parse_args?;", "let args = parse_args()?;"),
    ("let event_loop = EventLoop::<UserEvent>::with_user_event.build?;", "let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;"),
])
sub_re(main, r"\bargs\.mods\(\)", "args.mods")
sub_re(main, r"mods\(\)\.", "mods.")
sub_re(main, r"tracing_subscriber::fmt\b(?!\()", "tracing_subscriber::fmt()")
sub_re(main, r"EnvFilter::try_from_default_env\b(?!\()", "EnvFilter::try_from_default_env()")
sub(main, [(".init;", ".init();"), ("event_loop.create_proxy;", "event_loop.create_proxy()?;")])
sub(main, [("println!(\"[console] loaded {name} from {}\", path.display)", "println!(\"[console] loaded {name} from {}\", path.display())")])

# --- console.rs ---
sub(cons, [
    ("lib.ids.cloned().collect()", "lib.ids.cloned().collect()"),
])
sub_re(cons, r"test_world\b(?!\()", "test_world()")
sub_re(cons, r"temp_dir\b(?!\()", "temp_dir()")
sub_re(cons, r"world\.library\b", "world.library()")
sub_re(cons, r"sim\.broker\b", "sim.broker()")
sub(cons, [("path.display))", "path.display()))")])
sub_re(cons, r"b\.world\b", "b.world()")

# --- s1 test ---
sub_re(s1, r"demo_zone\b(?!\()", "demo_zone()")
sub_re(s1, r"sim\.broker\b", "sim.broker()")

# --- world_seam ---
sub_re(ws, r"sim\.player_health\b", "sim.player_health()")

# --- effects.rs mods error (FUNCS misfire on shadow) ---
t = open("crates/tessera-core/src/effects.rs").read()
t = t.replace("assert_eq!(mods().skip_turns(), 1);", "assert_eq!(mods.skip_turns(), 1);")
open("crates/tessera-core/src/effects.rs", "w").write(t)

print("mega-batch done")