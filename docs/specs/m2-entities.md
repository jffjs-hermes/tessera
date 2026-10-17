# M2 spec: data-driven entities

Status: research proposal for M2 (author: researcher bot, awaiting human approval).
Grounded in [architecture.md](../architecture.md) §2, §3, §5, §8 and
[determinism.md](../determinism.md). M2 scope per architecture §8: *blueprint loader
with inheritance, spawning from data, hot reload, ECS plus dynamic part storage*.

---

## 1. ECS decision

Three candidates were compared against the hard requirements from the architecture
plan: mods must be able to define **new parts at runtime** (Luau, M4/M5), parts must
be deserialized from TOML on load (strict typing), the simulation must stay
deterministic (no `HashMap` iteration, single-threaded, no GPU deps in the sim crate),
and the engine stays a "thin custom engine on small libraries".

### Comparison

| Criterion | `hecs` | `bevy_ecs` (standalone) | custom ECS / part map |
|---|---|---|---|
| Archetype storage & fast queries | ✅ yes | ✅ yes (best in class, parallel) | ❌ we would write and optimize it ourselves |
| Runtime-defined component types (mods) | ⚠️ compile-time `#[derive(Component)]` types only; runtime-typed storage is not exposed usefully | ⚠️ supports dynamic components via `ComponentId`/`ComponentInfo` + `UnsafeCellComponent`, but the API is raw and version-unstable; v0.17 rewrote internals again | ✅ trivially: part data lives in a per-entity typed map |
| API churn | low (0.11.x, slow-moving, actively maintained — updated as recently as this month) | high; each Bevy release moves APIs | none (ours) |
| Dependency weight | tiny, no deps of note | large, pulls many bevy subcrates even standalone | none |
| Determinism friendliness | fine; iteration order is ours to control via `BTreeMap`-keyed part lists | fine, but its parallel schedulers tempt away from the single-threaded rule | fine |
| Cost of writing our own | — | — | high for queries; low if we don't need archetype queries |

### Recommendation: **hecs + a dynamic `PartMap` sidecar**

- `hecs::World` owns entities and all **engine-known** component types (`Position`,
  `RenderTile`, later `Health`, `Body`…), giving fast archetype queries for the hot
  renderer/simulation paths.
- A separate `PartMap` (a `BTreeMap<PartTypeId, Box<dyn Part>>` per entity, keyed by a
  string part name) holds **mod-defined parts** whose types can't exist at compile
  time. Access goes through downcasting; iteration order is deterministic
  (`BTreeMap`, per determinism rule 3).
- This is exactly the shape the architecture table already proposes
  ("`hecs` or standalone `bevy_ecs` for native components, plus a dynamic part map for
  mod-defined data"); this spec settles it on `hecs` because bevy_ecs's dynamic
  component API is unstable across releases and far heavier than we need. If M2 later
  discovers we need dozens of engine components with `&mut` aliasing tricks, revisit
  before M3 — the `PartMap` design is ECS-agnostic either way.
- `bevy_ecs` standalone is the documented fallback; a custom archetype store is
  rejected outright (we would be rebuilding hecs badly).

One `EntityBroker` wrapper pairs `hecs::Entity` ids with `PartMap`s so game code deals
with a single entity handle; despawn removes both.

## 2. Blueprint schema

Files: `data/base/blueprints/*.toml` (recursive, loaded through the existing `Vfs`,
sorted path order like `TileRegistry::load`). One TOML file holds many blueprints;
the top-level table name is the blueprint id. A single file format mirrors
`tiles/ascii.toml` and keeps mods simple.

```toml
# data/base/blueprints/creatures.toml

[Actor]
parts = {}

[Humanoid]
inherits = "Actor"
parts.Render    = { tile = "creature.humanoid" }
parts.Stats     = { strength = 10, agility = 10 }
parts.Throwable = {}          # mod-defined, unknown to the engine core

[Snapjaw]
inherits = "Humanoid"
parts.Render = { tile = "creature.snapjaw" }
parts.Stats  = { strength = 18, agility = 6 }   # overrides parent value
```

Rules:

- **Top-level table = blueprint id.** Ids are unique; case-sensitive strings.
- **`inherits`** (optional, string): exactly one parent. Single inheritance chain;
  no multiple inheritance at M2 (Qud-style single chain, keeps diagnostics simple).
- **`parts.<Name> = { ... }`**: the part payload is a TOML table deserialized into the
  part's registered type (below). Omitted fields fall back to the type's serde
  defaults; unknown fields are an error (`deny_unknown_fields`) so typos fail loudly.
- **Inheritance resolution is deterministic:** all files load in sorted path order
  (`Vfs::list` already sorts); later files may *redefine* a blueprint id (last wins,
  matching the tile registry) but may not change the resolved tree mid-load. After
  collecting every id, resolution walks each chain parent→child. A part overrides a
  parent part with the same name (whole-part replace, not deep merge — deep merge of
  arbitrary part payloads is deferred to M5 patching, where it belongs).
- **Diagnostics carry `file + id`:** `unknown parent 'Humaniod' for 'Snapjaw'
  (data/base/blueprints/creatures.toml)` and `inheritance cycle: A -> B -> C -> A
  (defined at <file>:<id>, cycle entered at <file>:<id>)`. Errors are collected per
  load pass where possible so one typo doesn't hide ten others; on any error the
  whole batch is rejected (see hot reload below).
- **Typed/serialized parts:** each part name maps to a registered Rust type
  (`parts.Stats -> stats::Stats`), stored in a `PartRegistry: BTreeMap<&'static str,
  PartFactory>` built at startup from engine-known parts; mods add entries through
  the same registry (M4/M5 hook — scripts supply a name plus a Luau-side schema; for
  M2, unknown part names in TOML that have no registry entry are a load **error**,
  so `Throwable` in the example above must be registered by the content pack that
  ships it). Blueprint text is kept as `toml::Value` per part until spawning, so a
  part added to the registry later can still parse the same file.
- **Spawning:** `World::spawn_blueprint(id, &mut broker) -> Entity` resolves the
  flattened blueprint, instantiates registered engine components into `hecs`
  (e.g. `RenderTile` from `parts.Render.tile` via the existing tile registry), and
  puts remaining parts into the entity's `PartMap`. Scripts receive `get_part` /
  `set_part` over the `PartMap` at M4. Event handlers on blueprints are **out of M2
  scope** (M3/M4: events, scripting); if the reserved `handlers` table appears in a
  blueprint, the loader **errors** — silent ignore hides bugs and content should
  wait until M3 supports it.
- **Hot reload:** `notify` watches `blueprints/`; on change, re-resolve the *entire*
  blueprint set into a fresh `BlueprintLibrary`; on success, atomically swap the
  shared `Arc<RwLock<…>>` and (existing live entities keep their old parts — no
  migration at M2; new spawns use the new library). On any parse/resolution error,
  log the `file + id` diagnostics and **keep the previous library unchanged**
  (same keep-old-on-error semantics as the tileset hot reload in M1). Reload is
  content-only: it must not touch `SimRng` or existing entities, preserving
  determinism (a replay's spawn order is an input, not affected by a mid-run reload
  that can only fail closed).

### Registry-shaped base data

Ship `data/base/blueprints/` with `Actor → Humanoid → {Player, Snapjaw}` so M2
demonstrates the whole chain end-to-end, and the hardcoded `@` in `main.rs` becomes
data-driven (`[Player]` with `parts.Render = { tile = "creature.player" }`).

## 3. Determinism constraints honored

From [determinism.md](../determinism.md), applied here:

1. All blueprint collections and the `PartRegistry`/`PartMap` use `BTreeMap` keyed by
   id; part lists iterate in sorted name order.
2. Spawning consumes no randomness at M2; when worldgen spawns later (M6), it must
   spawn in deterministic order from a forked `SimRng`-seeded system.
3. No HashMap/HashSet anywhere in the blueprint loader or part store.
4. Single-threaded load + spawn; `notify`'s watcher thread only *triggers* a
   rebuild and hands the result over — never mutates sim state directly.
5. The blueprint loader lives in `tessera-core` (no windowing/GPU deps).

## 4. Open decisions and risks

- **hecs version pin:** current stable 0.11.x; verify API at implementation time
  (architecture doc already flags re-checking maintenance).
- **Mod part schemas (M4):** how Luau-defined parts validate their TOML payload is
  undecided; M2 keeps the seam (registry + stored `toml::Value`) so either a
  Luau-side schema check or a serde-from-Lua path can be slotted in.
- **Part overrides:** whole-part replace only. Deep-merge semantics are deferred to
  M5 mod patching to avoid inventing two merge systems.
- **Reserved keys** (`inherits`, `parts`, `handlers`): unknown top-level keys error,
  keeping the schema tight as it grows.
- **Risk: split storage (hecs + PartMap) means two lookups for entity access.**
  Acceptable: blueprints are small; the hot renderer path reads only `hecs`
  components and never touches the `PartMap`.
- **Risk: content error surface grows with mods.** Mitigated by file+id diagnostics
  and fail-closed hot reload.

## 5. Implementation boundaries (for builder cards)

**In scope (M2):**
- `tessera-core::ecs`: `PartMap`, `PartRegistry`, `EntityBroker` wrapping `hecs`.
- `tessera-core::blueprints`: loader over `Vfs` (sorted files), inheritance
  resolution with cycle/unknown-parent diagnostics, typed part deserialization,
  `BlueprintLibrary`, spawn-from-data.
- Hot reload of `blueprints/` with keep-old-on-error swap.
- `data/base/blueprints/creatures.toml` (Actor/Humanoid/Player/Snapjaw) +
  `data/base/blueprints/objects.toml` if useful; wire the existing `@` player to
  `[Player]`.
- Unit tests via `MemVfs`: inheritance chains, overrides, cycle + unknown parent +
  bad part payload errors (file+id in message), hot-reload error keeps old set,
  deterministic resolve order.

**Out of scope (explicitly):**
- Events, handlers, scheduler (M3); Luau/script parts (M4); mod manifests, load
  order, patch/merge ops (M5); worldgen spawning (M6); sprites (later).
- Multiple inheritance; per-field part merging; save/load of entities.

**Do not:** add bevy_ecs; put GPU/window deps in tessera-core; introduce HashMap in
sim code; silently ignore malformed blueprints.

**Builder card sketch:** (1) `ecs` module + PartMap/registry + broker; (2) blueprint
loader + inheritance + diagnostics + tests; (3) spawn path + base content + hot
reload + integration into `main.rs`. Cards 1–2 are independent; 3 depends on both.
