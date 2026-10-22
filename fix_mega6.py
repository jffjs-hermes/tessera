#!/usr/bin/env python3
import re

s = open("crates/tessera/src/devtools/console.rs").read()
s = s.replace("assert!(save_file.exists, \"save file written\");",
              "assert!(save_file.exists(), \"save file written\");")
open("crates/tessera/src/devtools/console.rs", "w").write(s)

s = open("crates/tessera/src/sim.rs").read()
s = re.sub(r"roundtrip_hooks\b(?!\()", "roundtrip_hooks()", s)
s = s.replace("let (map_a, log_a) = run;", "let (map_a, log_a) = run();")
s = s.replace("let (map_b, log_b) = run;", "let (map_b, log_b) = run();")
s = s.replace("    let run = || {", "    let run = || {")  # noop
open("crates/tessera/src/sim.rs", "w").write(s)
print("patched")