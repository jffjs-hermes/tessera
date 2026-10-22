#!/usr/bin/env python3
"""De-corrupt attempt-1's stale-method-call syntax: add parens to bare
no-arg method tokens (attempt 1 dropped the parens on zero-arg calls and
split constructor calls into `X::new()(args)` form). Only applies to
tokens followed by a non-`(` character, so already-correct calls are
untouched. Field accesses (.values, .world, .map, .part_map, .get, .read,
.write, .lock, into_inner) are exempted by explicit token allowlist."""
import re
import sys

FILES = [
    "crates/tessera-core/src/effects.rs",
    "crates/tessera-core/src/recompute.rs",
    "crates/tessera-script/src/host.rs",
    "crates/tessera/src/devtools/console.rs",
    "crates/tessera/src/main.rs",
    "crates/tessera/src/scripting.rs",
    "crates/tessera/src/sim.rs",
    "crates/tessera/tests/world_seam.rs",
    "crates/tessera/tests/s1_stats_pipeline.rs",
]

# tokens that are always methods in this codebase -> add () when bare
METHODS = [
    "len", "is_empty", "is_none", "is_some", "is_ok", "is_err", "trim",
    "cloned", "copied", "spawn", "into_iter", "iter_mut", "next", "to_bits",
    "to_owned", "to_vec", "as_ref", "as_ptr", "as_mut", "as_any", "as_bytes",
    "borrow", "borrow_mut", "skip_turn_map", "skip_turns", "expect", "unwrap",
    "is_failure", "try_into", "unwrap_err", "unwrap_or_default", "statuses",
    "as_str", "into", "collect", "is_computed", "or_default", "values_mut",
    "spend", "signum", "ok", "consume_skip_turn", "first_mut", "entries",
    "iter_storage", "create_table", "as_slice", "bytes", "drain", "finish",
    "is_quarantinable", "known_events", "map_size", "next_u32", "pairs",
    "pending_mut", "sandbox", "snapshot",
    "sort_unstable", "trim_start", "zone_start", "is_dir", "is_file",
    "glyph_chars", "save_hooks", "as_deref", "renderables", "request_redraw",
    "exit", "reload", "build_cells", "player_health", "player_turns_public",
    "display", "init", "create_proxy",
    "downcast_ref", "read_zone", "ticks", "read_world", "try_lock",
    "to_f32", "sync_player", "sync_derived_speeds", "recompute_live",
    "ready", "player_id", "drain_recompute_queue", "drain_deaths",
    "world_state_id", "drain_recompute", "advance_until_ready",
    "position",
]
# zero-arg helper functions whose call parens were stripped (used bare in
# `name.`/`name;`/`name,` positions where the compiler demands a call)
FUNCS = [
    "demo_zone", "test_world", "temp_dir", "fixture", "parse_args",
    "try_from_default_env", "default_attributes", "with_user_event",
    "tracing_subscriber_fmt",
    "fixture_vfs", "fixture_library", "effects_context", "sim_context",
    "full_registry",
]
# field tokens that must never gain parens
FIELDS = {"values", "world", "map", "part_map", "get", "read", "write",
          "lock", "into_inner", "names", "statuses"}

pat = re.compile(r"\.(" + "|".join(METHODS) + r")\b(?!\(|:)")
fpat = re.compile(r"\b(" + "|".join(FUNCS) + r")\b(?!\(|:)")

total = 0
for f in FILES:
    with open(f) as fh:
        text = fh.read()
    new = pat.sub(lambda m: "." + m.group(1) + "()", text)
    new = fpat.sub(lambda m: m.group(1) + "()", new)
    if new != text:
        total += 1
        with open(f, "w") as fh:
            fh.write(new)
        print(f"updated {f}")
print(f"files updated: {total}")

# residual report: any bare method token left (excluding field allowlist)
res = pat.finditer
for f in FILES:
    with open(f) as fh:
        for i, line in enumerate(fh, 1):
            for m in pat.finditer(line):
                tok = m.group(1)
                if tok in FIELDS and tok != "into":
                    continue
                print(f"RESIDUAL {f}:{i}: {line.rstrip()}")