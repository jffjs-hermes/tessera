#!/usr/bin/env python3
import re

cons = "crates/tessera/src/devtools/console.rs"
main = "crates/tessera/src/main.rs"

for f in [cons, main]:
    s = open(f).read()
    s = re.sub(r"test_sim\b(?!\()", "test_sim()", s)
    s = re.sub(r"test_zone\b(?!\()", "test_zone()", s)
    s = re.sub(r"world\.save_hooks\b(?!\()", "world.save_hooks()", s)
    s = s.replace('"Goblin".into()()', '"Goblin".into()')
    s = s.replace('"quicksave".into()()', '"quicksave".into()')
    s = s.replace('"mygame".into()()', '"mygame".into()')
    s = s.replace("std::process::id,", "std::process::id(),")
    s = s.replace("std::time::SystemTime::now\n", "std::time::SystemTime::now()\n")
    s = s.replace("std::time::SystemTime::now.", "std::time::SystemTime::now().")
    s = re.sub(r"\.as_nanos\b(?!\()", ".as_nanos()", s)
    s = re.sub(r"path\.exists\b(?!\()", "path.exists()", s)
    open(f, "w").write(s)
    print("patched", f)
print("done")