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

sub(main, [
    ("let mut it = std::env::args.skip(1);", "let mut it = std::env::args().skip(1);"),
    ('"--mods()" => mods() = it.next().context("--mods() needs a path")?.into(),',
     '"--mods" => mods = it.next().context("--mods needs a path")?.into(),'),
    ("let proxy = event_loop.create_proxy()?;", "let proxy = event_loop.create_proxy();"),
    ("!ev.kind.is_access", "!ev.kind.is_access()"),
])
sub_re(main, r"\.broker\(\)", ".broker")
sub_re(main, r"(?m)^(\s+)\.library\n", r"\1.library()\n")
sub(main, [("ctx.save_hooks()", "ctx.save_hooks")])

sub(cons, [
    ("line[pos..].parse::<u64>.unwrap()", "line[pos..].parse::<u64>().unwrap()"),
    (".map(|p| (p.x, p.y))", ".map(|p| (p.x, p.y))"),
])
sub_re(cons, r"\.parse\(\)::<i64>", ".parse::<i64>()")
sub_re(cons, r"\.parse\(\)::<u64>", ".parse::<u64>()")
sub_re(cons, r"\.parse\(\)::<i32>", ".parse::<i32>()")
sub_re(cons, r"\.parse::<i64>(\s*$)", ".parse::<i64>()\\1")
sub_re(cons, r"\.collect\(\)::<", ".collect::<")
print("done")