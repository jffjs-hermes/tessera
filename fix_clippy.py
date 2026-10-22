#!/usr/bin/env python3
"""Silence dead-code warnings on spec-documented public seams (public API the
S1 contract names; not yet consumed by the runner). Fix needless_borrow."""
import re

sc = "crates/tessera/src/scripting.rs"
sim = "crates/tessera/src/sim.rs"
main = "crates/tessera/src/main.rs"

s = open(sc).read()
# SharedSim: accessors kept for the script-facing API surface (spec §5.2/§8)
s = s.replace("impl SharedSim {\n",
              "#[allow(dead_code)] // spec seams (stats_armed/scheduler accessors)\nimpl SharedSim {\n")
# ScriptBridge::host: kept as the documented host handle accessor
s = s.replace("    pub fn host(&self) -> &Rc<Mutex<ScriptHost>> {",
              "    #[allow(dead_code)] // documented host-handle accessor (spec §5.2)\n    pub fn host(&self) -> &Rc<Mutex<ScriptHost>> {")
open(sc, "w").write(s)

s = open(sim).read()
s = s.replace("    recompute_handler: HandlerId,",
              "    #[allow(dead_code)] // registered handler id, kept for reload wiring\n    recompute_handler: HandlerId,")
s = s.replace("impl Sim {\n",
              "#[allow(dead_code)] // S1 contract seams: actor_speed/reload_rules/log\nimpl Sim {\n")
s = s.replace("        tessera::mods::discover_sources(&mods)?",
              "        tessera::mods::discover_sources(mods)?")
open(sim, "w").write(s)

s = open(main).read()
s = s.replace("        tessera::mods::discover_sources(&mods)?",
              "        tessera::mods::discover_sources(mods)?")
open(main, "w").write(s)
print("patched")