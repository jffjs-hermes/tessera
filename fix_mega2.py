#!/usr/bin/env python3
import re

def sub(path, pairs):
    t = open(path).read()
    for a, b in pairs:
        n = t.count(a)
        t = t.replace(a, b)
        print(f"  {n}x {path}: {a[:56]!r}")
    open(path, "w").write(t)

def sub_re(path, pattern, repl):
    t = open(path).read()
    t, n = re.subn(pattern, repl, t)
    open(path, "w").write(t)
    print(f"  re {n}x {path}: {pattern[:56]!r}")

main = "crates/tessera/src/main.rs"
cons = "crates/tessera/src/devtools/console.rs"

# main.rs: Content.broker and Content.save_hooks are FIELDS
sub_re(main, r"\bcontent\.broker\(\)", "content.broker")
sub_re(main, r"\bcontent\.save_hooks\(\)", "content.save_hooks")
sub(main, [
    ("content\n        .world\n        .library\n", "content\n        .world\n        .library()\n"),
    ("self.sim.map,", "self.sim.map(),"),
    ("errors.iter().map(|e| e.to_string()).collect()::<Vec<_>>",
     "errors.iter().map(|e| e.to_string()).collect::<Vec<_>>()"),
])

# console.rs
sub_re(cons, r"\bctx\.save_hooks\(\)", "ctx.save_hooks")
sub(cons, [
    ("ctx\n                .world\n                .library\n                .read()",
     "ctx\n                .world\n                .library()\n                .read()"),
    ("lib.ids {", "lib.ids() {"),
    ("lib.ids.cloned().collect()", "lib.ids().cloned().collect()"),
    ("(None, _) =>", "None =>"),
    ("t == rt.tile", "rt.tile == t"),
    ("if let Some(parent) = path.parent {", "if let Some(parent) = path.parent() {"),
])
sub_re(cons, r"\.parse\(\)::<", ".parse::<")
sub_re(cons, r"\.collect\(\)::<", ".collect::<")
sub_re(cons, r"\.join\(\)::<", ".join::<")
print("done")