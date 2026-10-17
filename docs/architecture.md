# Tessera architecture plan

Status: plan as of M1. Decisions marked **(decided)** were settled with the project owner;
everything else is a proposal to revisit when its milestone starts. Library choices were made
from prior knowledge, so re-check versions and maintenance status before adopting each one.

## 1. Decisions

| Topic | Decision |
|---|---|
| Language | Rust (stable, 2024 edition) **(decided)** |
| Platform | Desktop only. Web is out of scope **(decided)** |
| Look | ASCII/glyph tiles first; sprites possible later; tiles easy to swap per entity **(decided)** |
| Scripting | Luau via `mlua` (proposed; confirm at M4) |
| Engine style | Thin custom engine on small libraries, not a game framework |
| Name | Tessera: a single tile in a mosaic **(decided)** |

## 2. Design principles (borrowed from Caves of Qud)

1. **Blueprints.** Entities are described in data and inherit from each other
   (`Creature -> Humanoid -> Goblin`).
2. **Parts.** An entity is a bag of small components. Mods can add new ones.
3. **Events.** Parts talk through named events (`BeforeMove`, `TakeDamage`). Any part or script
   can intercept, modify or veto them. This is what produces emergent interactions and makes
   mods powerful.

Content goes in data files, behavior in scripts, and a small native core underneath.

## 3. Technology choices

| Concern | Choice | Why |
|---|---|---|
| Windowing/input | `winit` | Standard, maintained. `sdl3` is the alternative if gamepad/audio become painful |
| Rendering | `wgpu` + custom instanced cell renderer | One quad per cell and a texture atlas. Runs on Vulkan, Metal, DX12 |
| Dev UI | `egui` | Inspectors and a console (M7) |
| ECS / entity store | `hecs` or standalone `bevy_ecs` for native components, plus a dynamic part map for mod-defined data | Fast queries without Bevy's API churn. Bevy proper is avoided: its ECS wants compile-time components, which clashes with mods that define components at runtime |
| Scripting | Luau via `mlua` | Built-in sandboxing, gradual types, small. Plain Lua 5.4 is the fallback if Luau's build is a problem |
| Data formats | TOML (blueprints, tilesets, manifests), JSON for patches, via `serde` | Human-editable, strictly typed on load |
| Hot reload | `notify` | Instant content iteration (already used for tilesets) |
| RNG | `rand_chacha` (ChaCha8) behind `SimRng` | Deterministic worldgen and replays |
| Pathfinding/FOV | Own implementations (A*, Dijkstra maps, shadowcasting), or the `pathfinding` crate | Small, and you will want to tune them |
| Noise | `noise` or `fastnoise-lite` | Procgen |
| Save/load | `serde` + `postcard`/`bincode`, versioned schemas, zstd | |
| Logging/profiling | `tracing`, Tracy | |
| Tests | `cargo test`, `insta` snapshots, `criterion` benchmarks, `cargo-nextest` | |

`bracket-lib`/`rltk` are avoided: convenient, but lightly maintained and not built for heavy moddability.

Versions actually in use at M1: wgpu 30, winit 0.30, rand_chacha 0.10, toml 1, serde 1, notify 8,
font8x8 0.3.

### Alternatives considered

| Option | Verdict |
|---|---|
| C# + Godot 4 | Closest to Qud's stack, but Godot's node tree fights a custom ECS; it would be a renderer only |
| C++ + flecs + SDL3 + Lua | flecs suits mods well, but build and memory-safety costs are higher |
| Go + Ebitengine | Weak generics for ECS, few scripting options |
| Python + python-tcod | Fast prototyping, too slow for a large simulation, poor mod story |

## 4. Workspace layout

Current crates are `tessera-core`, `tessera-render` and `tessera` (binary). Planned shape:

```
crates/
  tessera-core/     # ECS world, blueprint loader, event bus, turn scheduler, RNG, VFS, tile registry
  tessera-script/   # Luau runtime, sandboxing, API bindings          (M4)
  tessera-mods/     # discovery, manifests, dependency/load order, patching   (M5)
  tessera-world/    # map/zone/chunk model, FOV, pathfinding, worldgen hooks  (M6)
  tessera-render/   # wgpu cell renderer, atlas
  tessera-ui/       # in-game UI, egui dev tools                      (M7)
  tessera/          # binary: wires everything; base content loads as a "mod"
data/base/          # shipped content, same format as mods
docs/
```

Crates are split when a second consumer or a dependency boundary justifies it, not up front.

## 5. Core model

- **Blueprints.** A data file defines an entity: `inherits`, a list of parts and their
  parameters, and event handlers (native or script).
- **Events.** A typed bus with priorities and cancel/modify semantics. Scripts and native parts
  register handlers on equal terms.
- **Turn scheduler.** Energy/speed-based (as in Qud and Brogue), supporting varied speeds and effects.
- **Simulation and presentation are separate.** The simulation is headless and deterministic, so
  it runs in tests and a CLI. The renderer only reads state.
- **Mods are the base game's format.** Base content is just a mod. Mods can add, override or
  patch (`replace`, `merge`, `remove`) any blueprint, and add scripts, tiles, strings and
  worldgen hooks.
- **VFS.** All content loads through the `Vfs` trait (directory and in-memory today; zip for
  distributing mods at M5). Tests use in-memory content.

### Tile ids (implemented in M1)

Entities never reference a glyph or sprite directly:

```toml
# blueprints
[Goblin]
inherits = "Humanoid"
parts.Render = { tile = "creature.goblin" }

# tiles/ascii.toml
"creature.goblin" = { glyph = "g", fg = "#c8a060" }

# tiles/sprites.toml  (later, optional)
"creature.goblin" = { sprite = "creatures/goblin.png", tint = ["#c8a060", "#503820"] }
```

- One render path: the renderer draws instanced quads from an atlas. Glyphs are bitmap-font
  entries packed into it; sprites become more atlas rectangles, so moving to sprites adds
  content, not a renderer.
- Fallback chain: active tileset, then glyph tileset, then a visible magenta `?`.
- Later tileset files override earlier ones per id, so one entity can get a sprite at a time
  and a sprite pack is just a mod containing a tileset.
- Hot reload updates the screen on save.
- Qud-style recoloring: sprites authored in grayscale (or two marker colors) and tinted from
  `fg`/`detail` at draw time.

## 6. Mod and script safety

- Each mod has a `mod.toml` manifest: id, version, dependencies, load-order hints, engine API version.
- Scripts run in a Luau sandbox with no `io`/`os`, an instruction/time budget per call, and
  access only to the exposed API. No network access.
- The API is versioned and documented, and the base game uses it too, so it is always exercised.
- Native plugins (dynamic libraries) are possible on desktop but deferred; do not start there.
- Keep hot loops (FOV, pathfinding, AI queries) native; scripts orchestrate.

## 7. Determinism rules

See [determinism.md](determinism.md). Short form: seeded `SimRng` per system via `fork`, no
`HashMap` iteration in simulation code, single-threaded simulation, simulation crates have no
windowing/GPU dependencies.

## 8. Milestones

Run as small batches with human review between each.

- [x] **M0 Scaffold:** workspace, CI (fmt, clippy, test), dual MIT/Apache-2.0 license, VFS trait, seeded RNG, determinism rules.
- [x] **M1 Tile renderer:** atlas renderer, tile-id registry, movable `@` on a hardcoded map, tileset hot reload.
- [ ] **M2 Data-driven entities:** blueprint loader with inheritance, spawning from data, hot reload, ECS plus dynamic part storage.
- [ ] **M3 Events and turns:** `Move`, `Attack`, `TakeDamage` events, speed-based scheduler, headless simulation tests.
- [ ] **M4 Scripting:** embed Luau, minimal API (`spawn`, get/set part, `on(event)`), one behavior (e.g. poison) written entirely in script.
- [ ] **M5 Mod loader:** manifests, load order, patch operations, zip-able folders via the VFS, sample mod that changes a blueprint and adds a script.
- [ ] **M6 World slice:** zones/chunks, cave worldgen, FOV, pathfinding, basic AI (wander/chase).
- [ ] **M7 Saves and tooling:** versioned saves, debug console, egui entity inspector.

After M7: inventory, stats, mutations, factions, quests, and layered worldgen.

## 9. Risks

- **Over-engineering the mod API early.** Build real content on it first, then extract the API from that.
- **Scripting performance.** Native hot loops; scripts orchestrate.
- **Determinism.** Decided at M0 (rules above); enforce in review.
- **Rust iteration speed.** Mitigate with `cargo-nextest`, `mold`/`lld`, dev-profile optimization of dependencies (already set), and putting most behavior in hot-reloadable data and scripts.
- **Luau build on all desktop targets.** `mlua` vendors Luau (C++), so CI should build it on Linux, macOS and Windows at M4.
- **8x8 bitmap font limits.** Fine for development; swap to a rasterized TTF in the same atlas when needed.
