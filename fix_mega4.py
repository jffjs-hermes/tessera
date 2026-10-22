#!/usr/bin/env python3
import re

# 1. drop save_hooks from the fixer METHODS (ConsoleContext.save_hooks is a field;
#    World.save_hooks() is a method; field sites must not gain parens)
t = open("fix_bare_calls.py").read()
t = t.replace('"glyph_chars", "save_hooks", "as_deref", "renderables", "request_redraw",',
              '"glyph_chars", "as_deref", "renderables", "request_redraw",')
open("fix_bare_calls.py", "w").write(t)

# 2. console.rs / main.rs fixes
for f in ["crates/tessera/src/devtools/console.rs", "crates/tessera/src/main.rs"]:
    s = open(f).read()
    s = re.sub(r"save_hooks\(\)", "save_hooks", s)
    s = re.sub(r"\.parse::<i64>\n", ".parse::<i64>()\n", s)
    s = re.sub(r"\.parse::<u64>\n", ".parse::<u64>()\n", s)
    s = re.sub(r"\.parse::<i32>\n", ".parse::<i32>()\n", s)
    s = re.sub(r"\.collect::<Vec<_>>\n", ".collect::<Vec<_>>()\n", s)
    s = re.sub(r"\.collect::<Vec<_>>\.", ".collect::<Vec<_>>().", s)
    open(f, "w").write(s)
    print("patched", f)
print("done")