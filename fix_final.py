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

sub("crates/tessera/src/scripting.rs", [
    ("    host: Rc<Mutex<ScriptHost>>,", "    pub(crate) host: Rc<Mutex<ScriptHost>>,"),
    ('("max".to_string(), PartValue::Int(c.max())),', '("max".to_string(), PartValue::Int(c.max)),'),
])
sub("crates/tessera/src/sim.rs", [
    (".actors\n            .any(|e| e == entity)",  ".actors()\n            .any(|e| e == entity)"),
    ("Some(PartValue::Int(n)) => Some(n),", "Some(PartValue::Int(n)) => Some(*n),"),
    ("        && let Some(world) = b.world\n    {", "        && let Some(world) = b.world()\n    {"),
    ("player: Some(self.player.to_bits()),", "player: Some(self.player.to_bits().get()),"),
    ("                    e.to_bits(),", "                    e.to_bits().get(),"),
    (".map(|e| e.to_bits())", ".map(|e| e.to_bits().get())"),
    ("e.to_bits()\n            .map", "e.to_bits().get()\n            .map"),
])
# fixer list: world_state is BOTH field and method (field wins inside impl)
t = open("fix_bare_calls.py").read()
t = t.replace(
    '"sort_unstable", "trim_start", "world_state", "zone_start",',
    '"sort_unstable", "trim_start", "zone_start",')
open("fix_bare_calls.py", "w").write(t)
print("fixer updated")