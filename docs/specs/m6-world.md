# M6 spec: world slice — data-driven map, cave worldgen, pathfinding, script-owned AI

Status: research spec for M6 (author: researcher bot; pending Jeff's approval). Grounded in
[architecture.md](../architecture.md) §3 (Pathfinding/FOV, Noise rows), §4 (planned
`tessera-world` crate), §5 ("Core (native) owns: grid and map storage, walkable and occupied
queries, distance, deterministic A* pathfinding, FOV (later), the seeded RNG streams, event
dispatch, the scheduler, spawn and despawn, and emitting a `Died` event…"), §6 ("Scripts (Luau)
own: the AI policy (wander, chase when within range, attack when adjacent), the death and
respawn policy…"), §8 (M6 scope and the five demo acceptance items), [determinism.md](../determinism.md),
and the shipped M2–M4 code (`crates/tessera-core/src/{map,rng,events,turns}.rs`,
`crates/tessera/src/{sim,scripting}.rs`, `crates/tessera-script/src/host.rs`) and specs
([m2-entities.md](m2-entities.md), [m3-events-turns.md](m3-events-turns.md),
[m4-scripting.md](m4-scripting.md)).

External references used (verify against source at implementation time):

- Cellular automata cave generation: the standard "start with random fill, apply the
  4/5 rule for a few passes" pipeline (Jim Babcock, "Cellular Automata Method for Generating
  Random Cave-Like Levels", RogueBasin; subsequent RogueBasin articles unify the variant
  parameters). Integer-only, seedable from any RNG, small enough to inline.
- A* on grids: Hart–Nilsson–Raphael (1968); 8-direction octile costs are the standard grid
  heuristic. The `pathfinding` crate (architecture §3 alternative) was considered and
  rejected for M6 (§4.3): it does not expose deterministic tie-breaking control, and owning
  ~150 lines of A* keeps the determinism surface inside our clippy-guarded crates.
- Recursive shadowcasting (Björn Bergström, RogueBasin) is recorded in §5.3 as the **future**
  FOV variant. FOV is explicitly *not* part of M6 — no FOV code ships in this milestone.

Owner decisions this spec incorporates (from the parent task, treated as fixed requirements):
map loaded from data; player and goblin spawned from blueprints; goblin seeded wandering;
straight-line distance ≤ 10 triggers path movement/chase and adjacent attack; player death
respawns at the start tile or the nearest free walkable tile; goblin respawns after 5 PLAYER
turns at a random walkable empty tile; FOV deferred; the native/script ownership split of
architecture §5/§6; a versioned scripting read API and turn hook preserving the M4
sandbox/budget rules; native calls for path and nearest/random tile with **no map loops in
scripts**.

---

## 1. Decisions at a glance

| Question | Decision |
|---|---|
| Cave generation | **Cellular automata** (4/5 rule, 2 refinement passes) seeded from a forked `SimRng`; `noise` crate rejected for M6 (§3.1) |
| Zone/chunk model | One **`Zone`** (fixed-size grid, `Vec` storage) replaces `Map`; **no streaming, no chunks now** — whole zone in memory, streaming explicitly deferred (§3.2) |
| Worldgen shape | Generator **parameters live in data** (`zones/*.toml`); generation itself is native in `tessera-core::worldgen` (§3.4) |
| FOV | **Deferred entirely** (owner decision). §5.3 records recursive shadowcasting as the future variant; detection stays straight-line distance (§5.4) |
| Distance | **Integer squared Euclidean** `dx² + dy² ≤ 100` for the "within 10" rule — no floats, no sqrt (§5.4, determinism rule 5) |
| Pathfinding | **Own deterministic A\*** in `tessera-core::path`; 8-directional, octile integer costs, total tie-break order, no corner cutting (§4) |
| AI behavior model | **Script-owned policy on the existing `ActorTurnStart` event** (native turn hook), stateless per turn: distance test → `engine.path` step, else seeded wander (§6) |
| Death/respawn | Native emits the reserved **`Death`** event when `hp` hits 0; Luau scripts own the respawn policy via a world-state part and the player-turn counter (§7) |
| Script API | **Additive to v1** (`TESSERA_API_VERSION` stays 1; nothing outside this milestone consumes v1 yet). New read/navigation calls + `spawn_at`; removals or semantic changes bump to 2 (§6.3) |
| Map loops in scripts | None — one native call per decision: `engine.path`, `engine.nearest_walkable`, `engine.random_walkable_tile` (§6.2) |

## 2. Owner decisions (resolved — restated as implementation requirements)

These come from Jeff and are not open for re-litigation in review; each maps to a section:

1. **Map loaded from data** → §3.4: zones are TOML in `data/base/zones/`; `Map::demo` goes away.
2. **Player and goblin spawned from blueprints** → §7.2: `Player` and `Goblin` blueprints
   already exist; M6 adds *where* they spawn (zone `start` tile; goblin at a seeded random
   walkable empty tile).
3. **Goblin seeded wandering** → §6.1: wander steps draw from `SimRng::fork("goblin-ai")`,
   the same stream every run for a given seed.
4. **Straight-line distance ≤ 10 triggers path chase; adjacent attack** → §5.4 and §6.1.
5. **Player death → respawn at start tile, else nearest free walkable tile** → §7.1.
6. **Goblin death → respawn after 5 PLAYER turns at a random walkable empty tile** → §7.1.
7. **FOV deferred** → §5.3 records only the future design; zero FOV code in M6.
8. **Native/script ownership split** → §8 (module boundaries mirror architecture §5/§6 verbatim).
9. **Versioned scripting read API + turn hook, sandbox/budget rules preserved** → §6.3;
   the M4 sandbox (`Lua::sandbox`, memory limit, instruction budget, quarantine) is untouched.
10. **Native calls for path and nearest/random tile; no map loops in scripts** → §6.2.

## 3. World: zones, maps, worldgen

### 3.1 Cave generation: cellular automata (decided; alternatives rejected)

**Decision: two-stage cellular automata**, all integer math, seeded from
`SimRng::fork("worldgen")` (a *worldgen-only* stream — determinism rule 2 gives worldgen its
own stream so adding a roll to gameplay never shifts cave shapes, and rule 4 permits
worldgen work off-thread later with its own forked seed).

Pipeline (all in `tessera-core::worldgen`, native):

1. **Seed fill.** Every non-border cell is wall with probability `fill = 45%`, drawn from
   the forked stream via `SimRng::below(100) < 45`. Border ring is always wall.
2. **Refinement.** Two passes of the 4/5 rule: a wall with ≥ 5 wall neighbors (8-neighborhood)
   stays wall; a floor with ≥ 5 wall neighbors becomes wall; else it flips to the majority
   terrain. Neighbor counts are pure integer arithmetic.
3. **Region cleanup.** One BFS flood fill (deterministic: scan row-major, take the first
   unvisited floor cell as each region's root) labels connected floor regions; every region
   except the largest is filled back to wall. This guarantees the playable area is a single
   connected cave — required by acceptance items 2–5 (a goblin or player stranded in a pocket
   would make chase/respawn untestable).
4. **Start tile.** The first floor cell of the largest region in row-major scan order —
   deterministic without consuming RNG, and stable under test.

Why CA and not noise: the `noise` crate is float-based (`f64`/`f32` field values), which
collides with determinism rule 5 ("don't use floating point for gameplay outcomes where
platforms could differ") unless we re-quantize every sample — re-quantized noise is
effectively a grid of random numbers anyway, which is what the CA seed fill already is, so
the crate buys nothing here. CA also needs no new dependency at all. Noise terrain
(biomes, layered worldgen) is the "after M7" backlog item in architecture §8 and can adopt
`fastnoise-lite` with integer quantization when that milestone starts; that decision is
recorded there, not here.

Tuning (`fill`, pass count, thresholds) lives in the zone TOML (§3.4) — data, not code, per
the "scripts/data own tuning" split.

### 3.2 Zone/chunk model: single in-memory zone now; streaming deferred

**Decision: one `Zone` per loaded map, whole thing in memory, no streaming, no chunk
subdivision in M6.** The demo grid is 80×50 (existing `GRID_W`/`GRID_H`) — 4,000 cells; even
a 10× larger cave is trivially resident, and chunking would add asynchronous seams to a
simulation that determinism rule 4 keeps deliberately single-threaded.

Concretely:

- `tessera-core::zone::Zone` replaces `tessera-core::map::Map` (which is explicitly labelled
  "a later zone/chunk model replaces it"). `Zone` keeps the same shape — `width`, `height`,
  row-major `Vec<TerrainId>` — plus `id: String`, `start: Point`, and the walkability query.
  `TerrainId` becomes an owned `String` key into the tile registry (the current `&'static str`
  only works for hardcoded ids; data-loaded maps need owned strings). `Map` is deleted, not
  deprecated: the M4 demo harness is the only consumer, and a dead alias would rot.
- A `WorldMap { zones: BTreeMap<String, Zone> }` **type exists** (so portals/multi-zone are a
  data question later, not a refactor) but M6 loads and simulates exactly one zone. `BTreeMap`
  per determinism rule 3.
- **Streaming is explicitly deferred**, with the trigger written down: adopt chunked
  streaming only when a single zone stops fitting comfortably in memory or worldgen exceeds
  ~100 ms. Neither is close at demo scale. The `Zone` boundary (id + owned storage + query
  traits) is deliberately the seam a future `ChunkedZone` would implement behind.

### 3.3 Walkability and occupancy

- `Zone::is_walkable(x, y)` — terrain-based, same semantics as today (out of bounds = wall).
- **Occupancy** (an actor standing on a cell) is a broker query, not map state: native
  `occupied(x, y)` scans scheduled actors' `Position` parts (the existing `entities_at`
  logic, promoted into core). "Walkable **empty**" = `is_walkable && !occupied` — the
  predicate every respawn and spawn-target call uses.
- Actors never share a cell: a `Move` whose target cell is occupied is cancelled by an EARLY
  handler (extends the existing wall-collision handler). This makes "empty" stable for the
  respawn guarantees in §7.

### 3.4 Map from data: `data/base/zones/*.toml`

One zone file, loaded through the existing VFS with the same fail-closed validation as
blueprints:

```toml
# data/base/zones/start.toml
id = "start"
[gen]
type = "caves"        # the only generator in M6; unknown types are a load error
width = 80
height = 50
seed = 20260318       # zone-local worldgen seed; SimRng::fork("worldgen") inside it
fill = 45             # % initial wall fill
passes = 2            # CA refinement passes
[gen.overlay]         # optional: force cells after generation (applied in file order)
"10,5" = "terrain.wall"
```

- `gen.type = "caves"` runs §3.1. An `[ascii]` section (array of strings, one per row, glyph
  → terrain-id mapping) is an **alternative generator type** (`type = "ascii"`) for
  hand-authored test maps — the known-path-layout tests in §9 are authored this way, so the
  tests do not depend on CA shape.
- Terrain ids validate against the tile registry at load (same pattern as `missing_tile_ids`).
- The zone to load is a `--zone <id>` CLI argument, defaulting to the single zone in the pack
  (a load error if `--zone` names an unknown id, or if the pack has multiple zones and no
  default is given — fail closed, no silent pick).

## 4. Deterministic native pathfinding

### 4.1 Choice: own A* in `tessera-core::path`

Architecture §3 offers "own implementations … or the `pathfinding` crate". **Decision: own
implementation.** The `pathfinding` crate's `astar` takes a sorted-neighborhood closure but
does not guarantee a unique path among equal-cost alternatives (its heap order is an
implementation detail, and its tie-breaking is not a documented, version-stable contract).
Determinism here means *the same seed and inputs produce the same path bytes*, and the
cheapest way to guarantee that is to own the ~150 lines: fixed neighbor order, explicit
tie-break, integer costs, our own clippy.toml enforcement. Rejection is recorded so a future
reviewer doesn't re-open it without new evidence.

### 4.2 Algorithm contract (the deterministic rules)

- **8-directional**, matching the demo's existing move set (`dx, dy ∈ {-1,0,1}`).
- **Neighbor order is fixed**: N, NE, E, SE, S, SW, W, NW (row-major by `dy` then `dx`).
  Every equal-cost path resolves to the same one because parent assignment follows this order.
- **Costs are integers**: straight = 10, diagonal = 14 (octile geometry scaled ×10 — no
  floats). Heuristic `h` = octile distance ×10, admissible and consistent for these costs.
- **Frontier is a `BinaryHeap`** of `(f, h, seq, node)` with `Reverse` ordering — ties break
  on `h` (prefer closer-to-goal), then on the monotonic push counter `seq` (FIFO among full
  ties). `seq` is a plain `u64`, so no hash container and no float compare anywhere.
- **Closed set**: a `Vec<u32>` generation-stamp array (`seen: Vec<u32>`, `gen: u32`) instead
  of a `HashSet` — O(1) reset per search by bumping `gen`, and structurally incapable of
  violating determinism rule 3.
- **No corner cutting**: the diagonal step N→NE requires both N and E walkable.
- `path(zone, from, to, max_nodes) -> Option<Vec<Point>>` — full path *excluding* `from`,
  *including* `to`, or `None` (unreachable / budget exceeded). `max_nodes` (default 4096) is
  the DoS guard for scripted callers: a hostile script cannot turn one `engine.path` call
  into an unbounded search. A `None` from budget exhaustion is indistinguishable from
  unreachable to the caller — acceptable in M6 (both mean "don't move along a path"), noted
  so a future milestone can add an explicit `Truncated` variant if AI needs to distinguish.

### 4.3 Distance and related queries (native, integer)

- `straight_line_dist2(a, b) -> u32` = `dx² + dy²` — the *only* distance the "within 10" rule
  uses: `dist2 ≤ 100`. Integer, exact, no sqrt (determinism rule 5). Scripts receive this as
  `engine.distance(a_entity, b_entity)` returning the **integer `dist2`**, documented as
  such (name says distance, doc says squared) — the alternative (exposing a threshold of
  100 instead of 10) hides the owner's "≤ 10" wording behind a different constant, which is
  worse for review.
- `nearest_walkable_empty(zone, broker, x, y) -> Option<Point>` — BFS ring search outward
  from `(x, y)` in the fixed neighbor order, first hit wins (deterministic; no RNG). Used by
  player respawn (§7.1) and "nearest free tile" generally.
- `random_walkable_empty(zone, broker, rng) -> Option<Point>` — collect walkable-empty cells
  in row-major order into a `Vec`, then `rng.below(len)` indexes it. Row-major collection +
  one indexed draw is fully deterministic for a given stream state. Returns `None` when no
  cell qualifies (callers must handle it; §7.1 specifies retry semantics).

## 5. FOV: deferred (owner decision) — design note only

### 5.1 Scope

**No FOV code, no FOV data, no renderer visibility changes ship in M6.** The demo renders the
whole zone; the goblin "sees" the player via distance (§5.4). This section exists only to
record the future direction so M6's data structures don't paint us into a corner.

### 5.2 What M6 must not do to keep FOV cheap later

- Keep walkability a pure function of terrain (already true) — shadowcasting consumes exactly
  that predicate.
- Keep the zone's storage row-major `Vec` — shadowcasting scans octants by rows.
- Do **not** add a `visible` bitfield to `Zone` "while we're in there". Visibility is
  per- viewer derived state, not terrain state; adding it now would be the classic premature
  cache.

### 5.3 Future variant (recorded, not built): symmetric recursive shadowcasting

Recursive shadowcasting per octant (Bergström, RogueBasin) is the pick when FOV lands:
it is integer-only, deterministic under a fixed octant order, produces the natural
"wall tops visible" look players expect, and is the variant most roguelike references
implement. The known wart — asymmetric cells (I see you, you don't see me) — is handled by
post-pass symmetry enforcement if it bothers playtesting. Alternative noted: Bresenham
line-of-sight per cell (simpler, slower, no octant machinery). Decision deferred to that
milestone; nothing in M6's API surface mentions FOV.

### 5.4 Detection in M6: straight-line distance

The goblin's detection test is `straight_line_dist2(player, goblin) ≤ 100` (i.e. Euclidean
distance ≤ 10). Walls do **not** block detection in M6 — that is FOV's job later, and the
owner's acceptance item says "straight-line distance" without occlusion. The AI script owns
the threshold as tuning data (§6.1); native owns the distance function.

## 6. Scripting surface: turn hook + versioned read API

### 6.1 AI behavior model: script policy on `ActorTurnStart`

**Decision: the AI policy is a Luau script reacting to the existing native `ActorTurnStart`
event; there is no separate "AI system" and no native AI code beyond the primitives.**
`ActorTurnStart` already exists (M3, dispatched by the scheduler for every actor turn) and is
already in the host's `known_events` — the turn hook is the *exposure* of that event to
scripts plus a couple of new read calls, not a new event type.

The demo goblin's script (`data/base/scripts/goblin_ai.luau`, loaded because `Goblin`'s
blueprint gains `scripts = ["scripts/goblin_ai.luau"]` — the same wiring M4 established for
`Zapper`):

```lua
-- tuning lives in the blueprint as script-part data (hot-reloadable, moddable):
-- parts.GoblinBrain = { detect = 10 }
engine.on("ActorTurnStart", function(ev)
  local me  = ev.actor
  local foe = engine.player()                  -- native: the demo's player entity id
  if not foe or not engine.is_alive(me) then return end
  local d2 = engine.distance(me, foe)          -- squared straight-line distance
  if d2 <= 100 then                            -- within 10 tiles (tuning read: parts.GoblinBrain.detect)
    local adj = d2 <= 2                        -- adjacent (Chebyshev 1: 8-neighborhood, includes diagonals)
    if adj then
      engine.attack(me, foe)                   -- dispatches Attack through the normal bus
    else
      local path = engine.path(me, foe)        -- ONE native A* call; script never loops the map
      if path and path[1] then engine.step(me, path[1]) end
    end
  else
    engine.wander(me)                          -- seeded one-step wander (§6.4)
  end
end)
```

Model properties:

- **Stateless per turn.** No memory, no state machine in M6: every turn re-tests distance.
  Wander/chase/attack emerges from the test alone, which keeps the script small, keeps
  determinism trivial (no hidden script state to serialize at M7), and matches the acceptance
  items exactly. A state machine (e.g. "keep chasing for N turns after losing the player") is
  a natural v2 of this script and needs no engine change — the part store already persists
  script state.
- **Adjacent** = Chebyshev distance 1 (`dx² ≤ 1 and dy² ≤ 1`, equivalently `d2 ≤ 2` under
  8-directional geometry — the script uses `d2 ≤ 2`).
- **Every map-sensitive decision is one native call**: `engine.path` (A*), `engine.distance`,
  `engine.nearest_walkable`, `engine.random_walkable_tile`, `engine.wander`. The script never
  iterates cells — architecture §6's "hot loops stay native" rule is enforced by the API
  simply not offering a cell-iteration primitive.
- The script does **not** choose the whole path per turn; it steps one cell (`path[1]`) and
  re-decides next turn, so the player moving invalidates the plan naturally and each turn's
  cost is one bounded A*.

### 6.2 New native calls backing the policy (the "one call" primitives)

- `engine.step(entity, point)` — dispatch a native `TurnAction::MoveTo` for that actor's turn
  (goes through the same EARLY walkable/occupied handlers as the player's move; a blocked step
  is a cancelled move, and the wander fallback below covers "blocked again").
- `engine.attack(attacker, defender)` — dispatch `Attack` on the bus (identical to the
  Zapper's scripted strike path).
- `engine.wander(entity)` — native primitive: pick one of the walkable-empty neighbor cells
  in fixed order, filtered by `rng.below` on the actor's **own forked AI stream**
  (`SimRng::fork("goblin-ai")` — seeded wandering, owner decision 3); step if one exists, else
  stay. Kept native (not scripted) because it is exactly the "no map loops in scripts" hot
  path, and its determinism belongs to core.

### 6.3 Versioning: additive to v1

`TESSERA_API_VERSION` **stays 1**: M5 (mods) has not shipped, so nothing outside this
milestone depends on the v1 surface, and the M4 spec's own rule ("the base game uses the API
too, so it is always exercised") is satisfied by the goblin script using the new calls. The
contract, recorded for review:

- **Additive-only within v1**: new functions on `engine`, new optional event names, new
  blueprint fields — never a behavior change to an existing call.
- **Removal, reordering of arguments, or semantic change bumps to 2**, with the M4 header
  check (`api = 1`) rejecting scripts that don't declare the matching version.
- The full v1 surface (M4's calls + this milestone's additions) is enumerated in this spec's
  §6.5 so the API doc story stays "one list, one version".

### 6.4 RNG streams for AI (determinism rule 2 applied)

- `goblin-ai` stream: one `SimRng::fork("goblin-ai")` per **entity** — the fork label combines
  the stream name and the entity's spawn index (`fork(label)` takes a u64; core hashes
  `"goblin-ai"` to a constant and mixes the entity's spawn ordinal), so adding a second goblin
  never shifts the first one's wander sequence, and removing one never shifts the others.
  Concretely: `SimRng::fork(HASH_GOBLIN_AI ^ (spawn_ordinal * GOLDEN))`.
- `worldgen` stream: per zone id (§3.1). Worldgen consumes it only at load, never at turn time.
- Scripts never see raw RNG state; `engine.wander`/`engine.random_walkable_tile` consume the
  streams natively. (The M4 `engine.rng` host stream remains for scripts that need their own
  rolls — respawn timing dice, etc. — and its determinism contract is unchanged.)

### 6.5 The full v1 scripting surface after M6

Existing (M4): `api_version`, `log`, `define_part`, `on`, `get_part`, `set_part`, `entity_parts`,
`spawn`, `despawn`, `rng`, `try_entity`.
Added by M6 (all read/navigation/actions above): `map_size`, `is_walkable`, `is_occupied`,
`position`, `player`, `is_alive`, `distance`, `path`, `nearest_walkable`, `random_walkable_tile`,
`step`, `attack`, `wander`, `player_turns`, `spawn_at`.
Events: `Move`, `Attack`, `TakeDamage`, `ActorTurnStart` (existing), **`Death`** (newly
dispatched; §7.1). `engine.spawn` is wired for real in M6 (it errors today, §6.3 of the M4
spec): scripts can spawn a blueprint at a position — the respawn policy needs it.

## 7. Death, respawn, and the player-turn clock

### 7.1 Native death event

The existing reserved `Death { target }` event (M3 events.rs) stops being reserved: the
`TakeDamage` applicator dispatches `Death` immediately after it clamps `hp` to 0 (once —
guarded so a second reducing hit on an already-dead entity doesn't re-emit). Core owns the
*emission* (architecture: "emitting a `Died` event when health reaches 0"); scripts own the
*response*:

- **Player death (script policy on `Death`):** despawn the player entity, then respawn
  `Player` from its blueprint at the zone's `start` tile; if that tile is occupied, at
  `engine.nearest_walkable(start)`. The fresh entity gets a fresh `Position` and is
  re-registered with the scheduler (native `engine.spawn_at` handles scheduler registration —
  spawning an entity with an `ActorEnergy` part makes it schedulable, same as the M2/M3
  spawn path).
- **Goblin death:** despawn the goblin, then schedule the respawn. The 5-PLAYER-turn delay
  needs a clock: **core owns a `player_turns: u64` counter** incremented once per completed
  player turn in `Sim::submit_action` (a completed turn = a dispatched player action,
  cancelled or not — the player "took" the turn either way; this is the only reading under
  which "after 5 PLAYER turns" is well-defined when the player is dead for part of the
  window). Exposed to scripts as `engine.player_turns()`.
- **Respawn scheduling is script-owned state**: the goblin script stores
  `{ due = player_turns_at_death + 5, blueprint = "Goblin" }` in a script part on a
  **world-state entity** (a singleton spawned at load with a `WorldState` script part;
  it is *not* a scheduled actor — the script checks it from the *player's*
  `ActorTurnStart` handler, which fires every player turn). When
  `engine.player_turns() >= due`, the script calls
  `engine.spawn_at("Goblin", engine.random_walkable_tile())`.
- **`random_walkable_tile` returning `None`** (a full map) is handled by retrying on the next
  player turn (leave the pending entry in place). Recorded because a pathological cave with
  one walkable cell and two actors is constructible; the retry rule makes the policy total.
- `engine.spawn_at(blueprint, point|nil)` — native: validate the blueprint id (fail-closed,
  quarantine rules apply to the calling script), place `Position` at `point` or at a seeded
  random walkable empty tile when `nil`, register with the scheduler if it has
  `ActorEnergy`, return the new entity id. This is the real implementation behind M4's
  stubbed `apply_spawn`.

### 7.2 Spawn points in data

`data/base/blueprints/creatures.toml` gains a `[Zone]`-referenced spawn table — kept minimal:

```toml
# data/base/zones/start.toml (additions)
[spawns]
start = "Player"            # spawned at the zone's start tile
random = ["Goblin"]         # spawned at seeded random walkable empty tiles (§4.3 stream)
```

Spawning order is file order within each list, and the random picks consume the
`"zone-spawn"` forked stream — so the goblin's initial position is a pure function of
(seed, zone file). Player and goblin both come from blueprints (owner decision 2); the zone
file decides *where*, the blueprints decide *what*.

## 8. Module and API boundaries

```
crates/tessera-core/
  src/zone.rs      NEW  Zone, WorldMap, walkable/occupied queries, zone TOML loader (VFS)
  src/worldgen.rs  NEW  CA cave generator (§3.1) + ascii generator, `Zone::generate(params)`
  src/path.rs      NEW  A* (§4.2), dist2, nearest_walkable, random_walkable_empty
  src/map.rs       DEL  replaced by zone.rs (its test moves/extends into zone.rs)
  src/events.rs    chg  TakeDamage applicator dispatches Death (needs SimContext); nothing else
  src/lib.rs       chg  module list
crates/tessera-script/
  src/host.rs      chg  new engine.* calls (§6.5) over an extended WorldAccess trait
  (sandbox.rs, error.rs, budget: UNTOUCHED — owner rule)
crates/tessera-core (WorldAccess impl lives in the binary today):
crates/tessera/src/
  sim.rs           chg  zone instead of map; player_turns counter; goblin turn resolution
                        via ActorTurnStart script dispatch instead of the hardcoded enemy attack
  scripting.rs     chg  WorldAccess gains zone/path/occupied methods; spawn_at wiring
  main.rs          chg  --zone arg; load zone from data; world-state singleton; demo spawns
data/base/
  zones/start.toml     NEW  cave zone + overlay + spawn table
  blueprints/…         chg  Goblin gains scripts = ["scripts/goblin_ai.luau"]
  scripts/goblin_ai.luau  NEW  AI + death/respawn policy (§6.1, §7.1)
```

Dependency rules (determinism rule 6 enforced structurally):

- `tessera-core` gains `zone`, `worldgen`, `path` with **no** new external crates
  (no `noise`, no `pathfinding`). Worldgen uses only `tessera_core::rng`.
- `tessera-script` depends on `tessera-core` only (unchanged) — the WorldAccess *trait*
  grows methods, the *impl* stays in the binary crate.
- The renderer keeps reading zones exactly as it read maps (`Zone::is_walkable` +
  tile ids) — zero render changes beyond the type name.
- New simulation crate rule: `zone.rs`/`worldgen.rs`/`path.rs` live in `tessera-core`, which
  already has the `clippy.toml` disallowed-types list; no new crate means no new clippy.toml
  to maintain. The planned `tessera-world` crate (architecture §4) is **not** created in M6 —
  the split triggers "when a second consumer or a dependency boundary justifies it", and
  `path`/`worldgen` have exactly one consumer today. Moving them later is a mechanical
  extraction of self-contained modules.

## 9. Determinism rules for M6 (delta on determinism.md)

1. All worldgen and AI randomness via `SimRng` forks: `worldgen` (per zone id), `zone-spawn`
   (per zone id), `goblin-ai` (per entity spawn ordinal), plus the M4 host stream (unchanged).
2. No `HashMap`/`HashSet` anywhere in the new modules — A*'s closed set is a stamp `Vec`,
   zone storage is a `Vec`, world maps are `BTreeMap`s. `tessera-core`'s existing clippy.toml
   covers the new modules automatically.
3. Integer-only: distance is squared; path costs are 10/14; CA counts are integers; no `f32`
   enters zone, path, worldgen, or script parameters (script numbers arriving as `f64` are
   coerced via `i64` like M4's `coerce_int`).
4. Single-threaded (rule 4) — worldgen *may* later move off-thread with its own forked seed,
   but M6 generates at load on the main thread.
5. Script behavior adds no new nondeterminism: script-visible state (parts, positions, hp,
   player_turns) is all deterministic sim state; the sandbox/budget/quarantine rules of M4
   §3/§6 are unchanged and re-tested.

## 10. Tests

### 10.1 Worldgen: pinned checksum

- `cave_checksum(seed, zone_toml) -> u64`: FNV-1a 64 over the generated zone's text rendering
  (the same glyph map `text_map` uses) — implemented in core (six lines, no new dep).
- **Pinned test**: seed 20260318 + the shipped `start.toml` parameters must produce the
  checksum constant recorded in the test at implementation time. Any worldgen change that
  shifts a single cell fails the test — the roguelike-equivalent of the M4 RNG
  `known_values_are_stable` pin. Re-pinning requires a deliberate test edit, i.e. review.
- Structural tests independent of the pin: exactly one floor region after cleanup; borders
  all wall; `start` tile walkable; ≥ some minimum floor fraction; same seed twice → identical
  cells; different seeds → (over N seeds) differing cells.

### 10.2 Pathfinding: known layouts (hand-authored `[ascii]` zones)

Authored as zone TOML fixtures so they double as data-format tests:

- **Corridor around a wall**: an L-shaped detour is the *only* path; assert the exact
  `Vec<Point>` (fixed neighbor order ⇒ unique expected path).
- **Equal-cost tie**: two symmetric routes around a central block; assert the path takes the
  neighbor-order winner (N before NE before E…), pinning the tie-break contract.
- **No corner cutting**: diagonal squeeze between two walls is not taken; the orthogonal
  detour is.
- **Unreachable** returns `None`; **start == goal** returns empty path; `max_nodes` truncation
  on a maze larger than the budget returns `None`.
- **Octile costs**: a 45° zig-zag where the diagonal route is cheaper than the orthogonal one
  by the 10/14 geometry.

### 10.3 Headless AI tests (no window, deterministic event log — the M3/M4 pattern)

- **Wander determinism**: fixed seed, player parked far away; the goblin's move sequence over
  N player turns is byte-identical across two runs (event-log compare, like M3 §2.4).
- **Chase trigger**: hand-authored zone, player placed 10 tiles straight-line away → goblin
  paths toward the player; at distance 11 (dist2 = 121) → it wanders instead. Assert on log
  lines (`EVENT Move … handled` toward the player vs. wander-pattern moves).
- **Adjacent attack**: place the goblin adjacent; next goblin turn logs an `Attack` against
  the player and the player's hp drops by the base hit.
- **Player death/respawn**: drive the player to hp 0 (repeated goblin attacks with the player
  waiting); assert the old player entity despawns, a fresh `Player` exists at the start tile
  (or the nearest free tile when a fixture occupies the start), and `player_turns` kept
  counting through the death turn.
- **Goblin respawn timing**: kill the goblin (zaps), assert no goblin exists, then advance
  exactly 5 player turns — the goblin reappears at a walkable empty tile; with a pinned seed,
  assert the exact tile; a 6th check that turn 4 has no goblin pins the "after 5" boundary.
- **Sandbox/budget regression**: a hostile `goblin_ai` variant (infinite loop in the handler,
  map-loop attempt via repeated `engine.path` calls) trips the existing budget and
  quarantine counters — M4's tests re-run against the enlarged API surface.

### 10.4 Data/loader tests

- Zone TOML validation: unknown `gen.type`, out-of-range `fill`/`passes`, unknown terrain id,
  `--zone` unknown id, multiple zones without a default — each a load error naming the file
  (fail-closed, matching the blueprint loader's diagnostics style).
- Overlay application order and ascii-generator round-trip.

## 11. Demo mapping and README implications

Every M6 feature must be observable in the running demo (`cargo run`), per the milestone's
"shown in the running demo" acceptance rule:

| Feature | Demo evidence |
|---|---|
| Map from data | The demo loads `data/base/zones/start.toml` — the cave is visibly not the old hardcoded room; editing the TOML (hot reload already covers blueprints/tiles; zone hot reload is **out of scope**, noted) and restarting changes the map |
| Blueprints spawn | `@` (Player) and `g` (Goblin) appear at the zone's start / seeded random tile |
| Seeded wandering | The `g` meanders when you keep distance |
| Distance-10 chase + adjacent attack | Walk toward the goblin: inside 10 tiles it paths to you and bites (hp drops — the demo can print hp in the status line; if that's new UI, it's one `text_map`-adjacent line, counted in card 5) |
| Player death/respawn | Die to the goblin: you reappear at the start tile |
| Goblin respawn after 5 player turns | Kill it (E-zap ×3 as today): 5 moves later a fresh `g` appears somewhere walkable |
| FOV | Absent — whole map visible, deliberately |

README: the controls section is **unchanged** (arrows/WASD/hjklyubn move, `.`/Space wait,
E zap, Esc quit) — M6 adds no input. The README gains a short "The demo cave" paragraph:
what the goblin does, the distance-10 rule, both respawn behaviors, and that `--headless <n>`
now prints the cave + goblin AI log (the CI smoke path), plus the new `--zone` flag.

## 12. Staged builder-card decomposition (each ≤ ~300–500 changed lines)

Sequenced so every card is testable on merge, with stacked branches only where code is
genuinely shared. All cards: `completion_contract="local-only"`, `skills=["sdlc-review"]`,
PR authored by jffjs-bots, Jeff reviews/merges, bots never merge or push main.

| # | Card | Size est. | Depends on | Delivers |
|---|---|---|---|---|
| 1 | **Zone model + data-driven maps**: `zone.rs` (Zone/WorldMap/loader incl. ascii generator), delete `map.rs`, sim/main/render type swap, loader tests | ~400 | — | Feature: demo runs on a data-defined *ascii* zone; every existing test green |
| 2 | **Cave worldgen**: `worldgen.rs` CA + region cleanup + start tile; `gen.type="caves"` wired; checksum + structural tests; ship `start.toml` as caves | ~300 | 1 | Feature: the demo cave is generated from data |
| 3 | **Native path/dist primitives**: `path.rs` (A*, dist2, nearest/random walkable-empty, occupied query), known-layout tests | ~400 | 1 | No user-visible change; unit-tested seam |
| 4 | **Script API + turn hook**: WorldAccess growth, all `engine.*` additions incl. `spawn_at` + `spawn` wiring + `player_turns`; `Death` dispatch; sandbox/budget regression tests | ~450 | 3 | No user-visible change alone; v1 API doc list updated |
| 5 | **AI/death/respawn + demo**: `goblin_ai.luau`, blueprint wiring, world-state singleton, zone `[spawns]`, sim goblin-turn resolution, headless AI tests, README + hp status line | ~450 | 2, 4 | All five acceptance items visible in the demo |

Rationale for the shape: 2 and 3 are independent (worldgen needs only 1; path needs only 1),
so they can run as parallel branches off card 1; card 4 needs 3 (it exposes the path
primitives) but not 2; card 5 is the only card touching both generated caves and scripts, so
it gates on both lines. Nothing here requires the M5 mod loader.

## 13. Unresolved items (explicitly parked for Jeff — not guessed)

1. **Goblin respawn cap.** The acceptance text implies infinite goblin respawn. Unstated: is
   there a maximum concurrent goblin count? The spec implements unbounded respawn (one dead
   → one queued) since that is the literal reading; if Jeff wants a cap (e.g. 1 concurrent
   goblin — trivially true today since there is exactly one), say so and the script gains one
   constant.
2. **Zone hot reload.** Blueprint and tile hot reload exist; hot-reloading a *generated* zone
   means re-running worldgen and re-seating actors, which is a design question (what happens
   to the goblin mid-chase?). Deferred; editing a zone requires a restart in M6. Fine to
   leave, but it is a visible gap next to M2's reload story.
3. **Diagonal movement feel.** 8-directional movement (already implicit in the demo's move
   set and kept by this spec) makes distance-10 detection and Chebyshev-adjacency natural,
   but diagonals are free of corner-cutting only for *actors* (§4.2); the player can still
   diagonally slip between two diagonal walls since `Move` is cancelled only on
   non-walkable targets. If Jeff wants player corner-cutting forbidden too, it is one
   predicate in the EARLY move handler (card 5, ~5 lines).
4. **HP display.** §11 flags a one-line hp/status readout as demo evidence for combat; it is
   new UI surface in an otherwise simulation-only milestone. Included in card 5's estimate;
   drop it if Jeff prefers the log to be the only evidence.
