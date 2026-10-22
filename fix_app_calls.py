#!/usr/bin/env python3
"""Batch-fix the remaining attempt-1 corruption classes in tessera/src."""
import re

def sub(path, pairs):
    for a, b in pairs:
        with open(path) as f:
            t = f.read()
        n = t.count(a)
        if n == 0:
            print(f"  !! no match in {path}: {a[:70]!r}")
        else:
            t = t.replace(a, b)
            with open(path, "w") as f:
                f.write(t)
            print(f"  ok {n}x  {path}: {a[:60]!r}")

def sub_re(path, pattern, repl):
    with open(path) as f:
        t = f.read()
    t, n = re.subn(pattern, repl, t)
    with open(path, "w") as f:
        f.write(t)
    print(f"  regex {n}x  {path}: {pattern[:60]!r}")

sim = "crates/tessera/src/sim.rs"
sc = "crates/tessera/src/scripting.rs"

sub(sim, [
    ("scheduler\n            .actors\n            .any(|e| e == entity)",
     "scheduler\n            .actors()\n            .any(|e| e == entity)"),
    ("let actors = scheduler\n            .actors\n",
     "let actors = scheduler\n            .actors()\n"),
    ("s.actors.find(|e| *e != player)", "s.actors().find(|e| *e != player)"),
    ("sched.actors.collect()::<Vec<_>>", "sched.actors().collect::<Vec<_>>()"),
    ("b.world.get::<&RenderTile>(e)", "b.world().get::<&RenderTile>(e)"),
    ("Some(PartValue::Int(n)) => Some(*n),", "Some(PartValue::Int(n)) => Some(n),"),
    ("self.player() = e;", "self.player = e;"),
    ("self.player_turns() += 1;", "self.player_turns += 1;"),
    ("Some(self.player().to_bits())", "Some(self.player.to_bits())"),
])
sub_re(sim, r"(\.downcast_ref::<[^>]+>)(?!\()", r"\1()")
sub_re(sim, r"self\.player\(\)", "self.player")
sub_re(sim, r"self\.player_turns\(\)", "self.player_turns")
sub_re(sim, r"data\.player\(\)", "data.player")
sub_re(sim, r"data\.player_turns\(\)", "data.player_turns")

sub(sc, [
    ("!self.shared.stats_armed", "!self.shared.stats_armed.load(Ordering::SeqCst)"),
    ("if let Some(sched) = self.shared.scheduler",
     "if let Some(sched) = self.shared.scheduler.read().unwrap_or_else(|p| p.into_inner()).as_ref()"),
    ("&& let Some(sched) = self.shared.scheduler",
     "&& let Some(sched) = self.shared.scheduler.read().unwrap_or_else(|p| p.into_inner()).as_ref()"),
    ("pub fn entity_to_id(e: Entity) -> u64 {\n    e.to_bits()",
     "pub fn entity_to_id(e: Entity) -> u64 {\n    e.to_bits().get()"),
    ("m.names.map(|n| n.to_string())", "m.names().map(|n| n.to_string())"),
])
# SharedSim: AtomicU64 FIELDS named like the accessor methods -> field access
sub(sc, [
    ("let p = self.player().load(Ordering::Relaxed);", "let p = self.player.load(Ordering::Relaxed);"),
    ("self.player().store(", "self.player.store("),
    ("self.player_turns().load(Ordering::Relaxed)", "self.player_turns.load(Ordering::Relaxed)"),
    ("self.player_turns().store(n, Ordering::Relaxed)", "self.player_turns.store(n, Ordering::Relaxed)"),
    ("let w = self.world_state().load(Ordering::Relaxed);", "let w = self.world_state.load(Ordering::Relaxed);"),
    ("self.world_state().store(id, Ordering::Relaxed)", "self.world_state.store(id, Ordering::Relaxed)"),
])
sub_re(sc, r"(\.downcast_ref::<[^>]+>)(?!\()", r"\1()")
print("done")