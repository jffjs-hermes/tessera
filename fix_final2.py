#!/usr/bin/env python3
def sub(path, pairs):
    t = open(path).read()
    for a, b in pairs:
        n = t.count(a)
        if n == 0:
            print(f"  !! no match {path}: {a[:60]!r}")
        else:
            t = t.replace(a, b)
            print(f"  ok {n}x {path}: {a[:60]!r}")
    open(path, "w").write(t)

sub("crates/tessera/src/sim.rs", [
    ("world.library.clone()", "world.library().clone()"),
    ("for line in h.drain_log {", "for line in h.drain_log() {"),
])
sub("crates/tessera/src/scripting.rs", [
    ("self.world_state().store(id, Ordering::Relaxed)", "self.world_state.store(id, Ordering::Relaxed)"),
    ("let w = self.world_state().load(Ordering::Relaxed);", "let w = self.world_state.load(Ordering::Relaxed);"),
])
print("final batch done")