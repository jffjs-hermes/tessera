#!/usr/bin/env python3
import re
for f in ["crates/tessera-core/src/effects.rs", "crates/tessera-core/src/recompute.rs"]:
    t = open(f).read()
    n = t.count("ctx.broker()")
    t = re.sub(r"\bctx\.broker\(\)", "ctx.broker", t)
    open(f, "w").write(t)
    print(f, "reverted", n)