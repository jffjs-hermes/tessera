# M4 spec: Luau scripting (mlua) — sandbox, API, and the script-part model

Status: research spec for M4 (author: researcher bot; pending Jeff's approval). Grounded in
[architecture.md](../architecture.md) §3 (Scripting row, §6 "Mod and script safety"), §8 (M4 scope:
*embed Luau, minimal API (`spawn`, get/set part, `on(event)`), one behavior written entirely
in script*; the architecture's example behavior is named "zap" in this spec's demo — §9.1
resolves the PR #14 review naming decision), [determinism.md](../determinism.md), and the shipped M2/M3 code
(`crates/tessera-core/src/{ecs,events,world}.rs`) and specs
([m2-entities.md](m2-entities.md), [m3-events-turns.md](m3-events-turns.md)).

External references used (verify against source at implementation time):

- mlua 0.12.2 / mlua-sys 0.13 / luau0-src 0.18 (docs.rs, checked 2026-10): `Lua::new()`
  loads the *safe* stdlib subset; `Lua::sandbox(enabled)` (feature `luau`) marks libraries,
  built-in metatables and globals read-only and enables safeenv; `Thread::sandbox()` creates a
  per-script proxied global table; `Lua::set_memory_limit` returns on exceed `Error::MemoryError`;
  `Lua::set_interrupt` (feature `luau`) — interrupt callback "guaranteed to be called eventually
  (at any function call or any loop iteration)", may error to abort or `VmState::Yield`;
  `Lua::set_hook` with `HookTriggers::every_nth_instruction` also available on `luau`.
  `mlua::luau::{Require, FsRequirer}` provide require-by-string control.
- Luau sandboxing guide (luau.org): `io` removed, `os` reduced to `clock/date/difftime/time`,
  `dofile/loadfile` removed, `package` absent under mlua's Luau build; libraries/globals
  read-only via `luaL_sandbox`; per-thread globals via `luaL_sandboxthread`; VM interrupt
  callback is the sanctioned way to "interrupt and terminate hanging scripts".
- mlua maintenance status: actively maintained (0.12.x, October 2026), MIT, tested in its own
  CI on Windows/macOS/Linux incl. vendored Luau; execution environment for the classifier's claim.

Resolves architecture §9 risk: **"Luau build on all desktop targets. `mlua` vendors Luau (C++)…"**
(§9, CI/build-risk assessment in §8).

---

## 1. Decisions at a glance

| Question | Decision |
|---|---|
| VM | Luau via `mlua = { version = "0.12", features = ["luau", "send"] }` (vendored C++, no JIT) |
| Sanboxing | `Lua::sandbox(true)` + one sandboxed `Thread` per script + memory limit + instruction budget |
| Infinite-loop stopper | `Lua::set_interrupt` counting instructions; error out when the per-invocation budget is spent (§3.2) |
| Script part model | **Luau-declared schemas + one generic validated dynamic part** (`ScriptPart` in `PartMap`), not untyped TOML tables (§4) |
| Script/native relation | Scripts implement *parts* registered as event handlers; they read/modify `ScriptPart` data by value, never borrow hecs components (§4.4) |
| Versioning | `TESSERA_API_VERSION = 1`; queried via `engine.api_version()`; blueprint scripts carry `api = 1` in their header (§5.1) |
| Quarantine | A script handler that errors 3 consecutive times on the same hook is disabled (removed) and logged; the sim never panics because of script content (§6.2) |

Spelling note: §5 tracks the versioned API surface; §6 the failure semantics; §8 build/CI risk;
§9 module boundaries; §10 the test matrix.

## 2. Runtime and dependency shape

### 2.1 New crate: `tessera-script`

Per architecture §4, M4 adds `crates/tessera-script/`:

- Depends on: `tessera-core`, `mlua`, `serde` (for `ScriptPart` snapshots), `thiserror`, `toml`,
  `tracing`. Does **not** depend on `tessera-render` or `winit`/`wgpu` (determinism rule 6).
- Its `clippy.toml` gets the same `disallowed-types` list as `tessera-core` (determinism rule 3):
  the runtime keeps script-name registries in `BTreeMap`s; mlua internals may hash but that is a
  dependency's internal state, which determinism.md explicitly classifies as review-by-hand.
- The `tessera` binary wires the runtime (`crates/tessera/src/script_host.rs` or inline in
  `main.rs` — builder's choice, see §9); `tessera-core` gains only the generic `ScriptPart`
  plus schema storage so saves (M7) and the blueprint loader know it exists.

Why `mlua` and not a direct `mlua-sys`/lua-bindgen pairing or a custom `luau-sys` glue: mlua
already packages exactly what M4 needs (`sandbox`, `set_interrupt`, `set_memory_limit`, serde
bidirectional value conversion via `LuaSerdeExt`, `send` support for the `Send` bound the
watcher thread needs) and is the binding layer the architecture already named. Binding
ergonomics (§3.4) drive the choice of `UserData`-free design: the sim API is exposed as plain
functions over plain Luau values; no userdata types are needed in v1, which keeps the
`'static`/`MaybeSend` machinery out of scope.

### 2.2 Features and profiles

- `features = ["luau", "send"]`. Not `luau-jit`: the JIT is (a) unnecessary for orchestration
  scripts, (b) a determinism/brand-new-code-path risk, (c) currently unsupported on some CI
  runners; the interpreter is fast enough for script-level work. Not `luau-codegen`: same
  reasoning, and it drags in machine-code emission. Not `luau-vector4`: we do not expose vectors.
- No `vendored` feature needed — under the `luau` feature mlua *already* auto-vendors Luau from
  `luau0-src`; there is no pkg-config lookup path for Luau to fight over.
- JS decision recorded for builders: pins exact crate versions in the workspace `Cargo.toml`
  (`mlua = "0.12"; mlua-sys = "0.13"` minimum-effective via `cargo update -p` review), Luau
  version is whatever `luau0-src` 0.18 carries — record it in CI output for reproducibility.

## 3. Sandbox, budgets, and stopping infinite loops

### 3.1 The sandbox (what scripts can and cannot see)

Setup per script module (one Lua "environment instance" of the shared VM):

```rust
let lua = Lua::new();                    // safe stdlib only; io/package/library absent for Luau
lua.sandbox(true)?;                      // libraries/metatables/globals read-only, safeenv on
let thread = lua.create_thread(...)?;    // one thread per script
thread.sandbox()?;                       // proxied globals: writes stay local to the script
lua.set_memory_limit(SCRIPT_MEM_LIMIT)?; // ~4 MiB; exceed -> Error::MemoryError
lua.set_interrupt(interrupt)?;           // instruction budget, §3.2
```

- `Lua::new()` under the `luau` feature never exposes `io`; `os` is the Luau-sanctioned
  minimal set (`clock`, `date`, `difftime`, `time`), and we **further nil out `os.time`,
  `os.date`, `os.clock`, `os.difftime`** at install time (§7 — wall-clock is not deterministic).
- `load`/`loadstring` are available in the language but a script only ever loads its own source;
  `require` is not provided by mlua's safe Luau global set used here — cross-script reuse goes
  through v1's `require_mod(name)` shim (§5.3) implemented with `create_require_function` and a
  registry map, or simply by registering parts once per mod (preferred; `require` is **out of
  scope for v1** and explicitly documented as such).
- `collectgarbage` is restricted to `count` mode by `Lua::sandbox`, so scripts cannot poke GC
  timing; GC affects performance only, never game outcomes (the sim does not read GC state).
- No `debug` library. No `ffi`. No userdata. No network. No `LightUserData`. The only bridge to
  the engine is the `engine` table installed per script (§5).
- Threat model: scripts are *semi-trusted* (they come from mods). The boundary target is
  "wrong/hung script terminates *itself* with an error and is quarantined (§6), never writes
  outside the `PartMap`/API, never touches the filesystem/network/lifetime of the process."
  Luau's memory safety + read-only shared state + memory cap + instruction budget + the
  rejection of borrows across the boundary (§4.4) deliver that; defense-in-depth is cheap here
  and taken.

### 3.2 The instruction budget (the infinite-loop killer)

Mechanism: `Lua::set_interrupt` — Luau's built-in interrupt callback fires "at any function call
or any loop iteration", i.e. it **cannot be frozen inside a `while true do end`**, which is the
property a bare wall-clock timeout fails to guarantee against a malicious script.

Budget accounting (host-side, deterministic):

- The interrupt callback increments an `instruction counter` seeded from the **call budget**
  each invocation resets: every `engine`-API call entry point and every handler invocation begins
  a fresh budget (default `SCRIPT_INSTRUCTION_BUDGET = 5_000_000` instructions ≈ tens of ms on
  punctuation-free work — the value is a tunable constant, not a gameplay quantity).
- When the budget is exceeded the callback returns `Err(ScriptBudgetExceeded)` → the running
  chunk trips with that error, and the quarantine policy of §6.2 applies. The error is *returned*,
  never unwound as panic: `mlua` reports it as an `mlua::Error`, `tessera-script` maps it to
  `ScriptError::Budget { script, phase, used }` per its error type.
- Budget **does carry across nested engine→script→engine→script calls within one dispatch**:
  the counter is stored in the interrupt's captured `Cell<u64>` and only reset at the API entry
  boundary (the top of `ScriptHost::call_handler`), not per Lua call. A script cannot escape its
  budget by calling `engine.spawn` — that lands back in Lua with the counter still depleted.
  The nested-dispatch rules of M3 §1.5 bound the depth (16) regardless.
- Complementary coarse bound: `sim` measures **wall-clock** around a whole dispatch tick and
  raises `ScriptError::Budget` loudly (trace error) if a tick exceeds e.g. 1 s in a *debug*
  build; the instruction budget is the reliable stop, wall-clock is only a smoke alarm for
  accidentally expensive content. Release builds do not time out (no nondeterminism).

### 3.3 Rust→Luau→Rust re-entrancy (the one real sharp edge)

mlua's `send` build serializes VM access behind a reentrant mutex, but holding Rust borrows of
`SimContext` across a Lua function that calls back into `engine.*` mutably aliases. Rule (decided):

- Engine API functions exposed to Lua take `&mut SimContext` from the *one* owner — the API
  closure receives it as a plain owned arc/mutex-guarded struct and each handler invocation
  follows: `ScriptHost::dispatch_scripted_handlers(ctx)` holds `&mut ctx` for the whole handler
  batch; inside a *single* handler batch, nested API calls are forbidden at the Rust level
  (the API closures are invoked with a `&mut ScriptApi` facade whose mutable ops run before
  or after handler dispatch, not during).

Concretely, the v1 API surface (§5) is shaped so script handlers never run *while* Rust holds a
borrow on anything script-visible: handlers are called one at a time from the dispatch loop,
each call gets its own `(ctx_snapshot_for_reads, pending_actions)`, and mutations are applied
by the host *after* the handler returns (`Action` list). This is the same pattern the engine
already uses for post-`Move` position commit (M3 §3.2) and it keeps Luau from ever seeing a
second mutable borrow. Costs one small allocation per handler; fine at M4 scale, revisited only
if scripts handle thousands of events per tick.

### 3.4 Binding ergonomics

- Plain-function API (`lua.create_function`) over plain values: integers (`i32`/`i64`),
  booleans, strings, and serde round trip (`lua.from_value` / `lua.to_value`) for part payloads —
  no `UserData` derive, no `'static` lifetimes to fight, no `RefCell` puzzles.
- Identifiers: entity ids are u64 (`hecs` ids exposed as integers via `EntityBroker`; with
  generation baked in to keep stale-id errors honest — `engine.try_entity(id)` returns `nil`).
- Errors: engine errors surface to scripts as Luau errors with `engine.` prefix strings;
  scripts' errors surface to Rust as `ScriptError` (§6.1). No panics in either direction:
  mlua panics are converted via `LuaOptions` catching — the default is catchable.

## 4. The part-model decision: schemas + one generic dynamic part

### 4.1 The open question

M2 appended "scripts supply a name plus a Luau-side schema; for M2, unknown part names in TOML
that have no registry entry are a load error". Two candidate shapes for how a *script* defines
part data:

**(A) Untyped TOML-table part.** `parts.Charge = { charges = 3 }` is stored as a raw
`toml::Value` per entity; scripts read/write arbitrary keys.

**(B) Luau-declared schema + one validated generic dynamic part.** A script (or blueprint, at M5)
declares a part type — `engine.define_part("Charge", schema)` — and entity instances hold one
`ScriptPart` per declared type in the existing `PartMap`, validated **on write** against the
schema.

**Recommendation: (B).** Reasons:

1. **Determinism & save-safety.** `toml::Value` order-of-iteration and mixed-type keys have the
   exact iteration-order hazards determinism.md rule 3 bans. `ScriptPart` stores fields in a
   `BTreeMap<String, PartValue>` (a closed `enum PartValue { Int(i64), Bool, Str, List(Vec<PartValue>) }`
   — a closed set, not freeform TOML), so iteration order is sorted and the value space is
   finite. A tabular `Save` format at M7 (M2 §2) is trivial over (B); over (A) it needs a second
   ad-hoc serializer for `toml::Value` with round-trip ambiguity questions.
2. **Fail-closed content.** (A) turns every typo (`chargs = 3`) into a silent nil read — the
   classic mod bug class. (B) validates at define time (schema shape) and at write time (value
   type/range), so bad content is a loud load/handler error that the quarantine policy (§6) and
   CI (`--headless` smoke) surface immediately. M2 already chose fail-closed for *unknown* part
   names; (A) would quietly re-open that hole from the *inside*.
3. **It composes with the existing registry.** `PartRegistry` already stores name→factory with
   `StorageClass::{Component, Dynamic}`. (B) inserts a third entry pattern: a registered
   `ScriptPartType { name, schema }` whose factory builds an empty `ScriptPart` and whose
   blueprint TOML payload (plain `parts.Charge = { charges = 3 }`) is validated against the
   schema at spawn — the *same TOML authoring UX stays working*, which (A) would also give, but
   with validation the schema provides.
4. **Reuse from day one.** The schema system is exactly what M5 mod manifests will need for
   patchable parts (`merge/replace` need to know which fields are lists vs scalars).
5. (A)'s *only* real advantage — zero host machinery — is not worth the validation,
   determinism, and save-format costs.

Concretely: **not** "schema DSL living in TOML", and **not** a reverse type-generation pipeline.
The schema is declared *in script* (`engine.define_part`) as plain Lua tables and lives purely
on the host; blueprints stay TOML. Schema syntax (v1):

```lua
engine.define_part("Charge", {
  fields = {
    charges = { kind = "int", default = 1, min = 0, max = 99 },
    label   = { kind = "string", optional = true },
  },
})
```

`kind` ∈ {`int`, `bool`, `string`, `list<int>`, `list<string>`}. Defaults are applied host-side;
missing-without-default is a spawn error. Arrays-of-tables and nested tables are explicitly
**out of scope for v1** (add later only if content actually asks).

### 4.2 `ScriptPart` lives in `tessera-core` (not `tessera-script`)

```rust
// tessera-core/src/script_part.rs (new module)
pub struct ScriptPartType { name: PartTypeId, schema: BTreeMap<String, FieldDef> }
pub enum PartValue   { Int(i64), Bool(bool), Str(String), List(Vec<PartValue>) }
pub struct ScriptPart(pub BTreeMap<String, PartValue>);   // implements core::Part
```

- `tessera-core` owns the type + schema struct + validation ('blueprint TOML → `ScriptPart`'),
  because the blueprint loader and future saves live there. `tessera-script` translates Lua
  tables ↔ `PartValue` and drives registration through `Registry` APIs that already exist
  (`register` with a factory closure) — plus one new core API, `register_script_part(&ScriptPartType)`
  + `schema(&PartTypeId) -> Option<&ScriptPartType>`, so script-side `define_part` plugs in
  without touching core internals.
- `PartRegistry::build` gains a branch: if the id has a `ScriptPartType`, validate the
  `toml::Value` against it → `ScriptPart`. Unknown fields, wrong kinds, out-of-range ints and
  missing non-defaulted fields are `PartError::Schema` (fail-closed, matches M2 policy).
- `StorageClass` for script parts is always `Dynamic` (`PartMap`), never `Component`: scripts
  cannot inject compile-time hecs components. Typed engine components (`Position`, `Health`,
  `RenderTile`) stay read-only to scripts via accessor functions (§4.4).

### 4.3 How blueprints attach scripted parts (M4 shape)

```toml
[Bogling]
inherits = "Goblin"
parts.Charge = { charges = 2 }      # Schema registered by scripts/charge.luau (below)
scripts = ["scripts/charge.luau"]   # blueprint-level script names, loaded by host
```

- A blueprint may list `scripts = [...]` (new optional key, `BTreeSet<String>` of script paths
  relative to mod root). The host loads each as its own sandboxed thread §3.1 *before* any
  entity spawns from that blueprint, so `engine.define_part` and `engine.on(...)` calls inside
  the script's top level take effect at load time (declarative registration), not per-spawn.
- Script-registered handlers are *per blueprint* in v1: the script is only attached to entities
  spawned from blueprints that name it. Per-entity handler instances with `entity.on(...)` are
  M5 material (scripts per instance would multiply layout cost for no current need).
- Registration order is deterministic: scripts load in lexicographic path order per blueprint,
  then blueprint resolution order (as M2), and handler priority uses the M3 constants.

### 4.4 Scripted parts vs typed engine components

- Script handlers and native parts are peers under the event bus: both register handlers with
  `(priority, seq)` keys; script handlers slot into the same dispatch order; a script can veto
  `Move`/`TakeDamage` with `return false` exactly like a native mutator.
- Access boundary: scripts read engine components through API calls (`engine.get_part(e, "Health")`
  → `{ hp = 3 }`) and mutate their **own** `ScriptPart`s freely via `engine.set_part`; mutation
  of a *native* part through the API is provided only for whitelisted fields in v1 — nil for M4
  except `Health.hp` (needed for the Zapper demo's zap damage) — implemented as explicit API
  functions, desugared from
  `set_part` by type check: `set_part(e, "Health", v)` that targets a registered *native* part
  is `ScriptError::ImmutablePart` unless an API function exists.
- No script ever holds a Rust reference/borrow into the ECS. All bridging is by value at call
  boundaries (§3.3). Generated stale ids error, they never alias memory.

## 5. The versioned script API (v1)

### 5.1 Versioning

- `TESSERA_API_VERSION: u32 = 1` in `tessera-script`; every script starts with (parsed, not
  executed, from a header comment) `-- tessera-api: 1`; a mismatch is a load error before any
  sandbox executes a line from that file. `engine.api_version()` is also exposed at runtime.
- The API is additive-only within a version. Breaking changes bump the version and keep the old
  one shimmable for at least one milestone. The base game's own scripts use the API only —
  "API always exercised by base content" (architecture §6).

### 5.2 Surface

```lua
-- engine: host-injected global table (per-script, after sandboxing)
engine.api_version()              -> 1
engine.blueprint_name()           -> "Bogling"      -- the blueprint this script instance belongs to

engine.define_part(name, schema)  -- §4.1; load-time only; duplicate name = load error
engine.on(event_name, priority, fn)   -- script-level handler; priority uses named consts
                                       -- (engine.EARLY/-64, engine.NORMAL/0, engine.LATE/64)
                                       -- priority defaults to NORMAL; fn(ev_data) -> handled?
                                       --   false/nil = veto (mutator phase), true = pass

engine.spawn(blueprint, pos)      -> entity_id       -- dispatches through World::spawn_blueprint;
                                                     -- deterministic spawn order preserved
engine.despawn(entity_id)
engine.try_entity(entity_id)      -> boolean

engine.get_part(entity_id, name)  -> value|nil       -- ScriptPart fields as a plain table;
                                                     -- whitelisted native parts (Health -> { hp })
engine.set_part(entity_id, name, table) -> nil|error -- only ScriptParts (+§4.4 whitelist)

engine.pos(entity_id)             -> { x, y }|nil    -- Position accessor (read-only)
engine.rng(label)                 -> function        -- forked SimRng stream; returns deterministic
                                                     -- integers; *never* exposed as a Lua seedable
                                                     -- widget that could desync (determinism
                                                     -- rule 1-2): the Lua side cannot reseed
engine.log(message)               -> nil             -- tracing::info! under "script" target
engine.EARLY / engine.NORMAL / engine.LATE             -- priority constants (-64, 0, 64)
```

Explicitly **not in v1**: `require` (§3.1), timers/async, direct world queries (no iteration of
"all entities with part X" — needed at M5/M6 with mod scale; v1 keeps scripts event-driven over
explicitly passed entity ids), patching (M5), `os`/`io`/`bit`/`buffer`/`vector` (without the
standard libraries `engine` doesn't provide them; `buffer`/`vector` stdlib constants are simply
not selected via `new_with` — safe constants only).

### 5.3 `on(event)` mechanics

- `event_name` is the canonical event `NAME` strings from M3: `"Move"`, `"Attack"`,
  `"TakeDamage"`, `"ActorTurnStart"`. Unknown names are load errors (the set is known because
  core events ship `to_script_params`/`from_script_params` pairs — the seam the M3 spec §1.1
  mandated; a custom "script event" namespace `mod:<name>` may come at M5 for mod-defined
  events and is *reserved*, not implemented).
- The bridge dispatches in canonical handler key order; a script handler receives a plain table
  `{ target = id, amount = n, source = "card" }` (fields per event; ids as integers). Returning
  `false` vetoes (mutator semantics, `EventOutcome::Cancelled { by: <handler> }` recorded);
  modifications are expressed by *returning a replacement table* for whitelisted fields on
  `TakeDamage.amount` (mutate semantics carried in the returned table), matching the M3
  modify-in-place contract without in-place aliasing.
- Registration is inspected at load: script `engine.on` calls are collected and applied against
  the library at blueprint-load time; if an event name is unknown, load fails (fail-closed, same
  as M1 tileset / M2 reload policies).

## 6. Failure semantics: errors, quarantine, reload

### 6.1 Error taxonomy

```rust
pub enum ScriptError {
    Load       { script: String, msg: String },       // parse/compile, api-version, sandbox setup
    Register   { script: String, msg: String },       // define_part/on misuse at load time
    Handler    { script, event, entity, msg },        // runtime error during a handler
    Budget     { script, phase, used: u64 },          // §3.2 instruction cap
    Memory     { script },                            // Lua heap cap
    ImmutablePart { entity, name },
    UnknownPart / UnknownEntity / UnknownEvent,       // mirrors PartError/EventError cases
}
```

All map from mlua errors; nothing panics the sim. Script errors are *runtime content bugs*:
they surface as `tracing::error!` + counters, and the defeat policy in §6.2 decides what stops.

### 6.2 Quarantine policy (decided)

- A handler that errors (including budget/memory exhaustion) **skips that handler for the
  in-flight event** (the event continues with remaining handlers — a quarantined handler must not
  corrupt gameplay state because it is removed mid-flight: `EventOutcome` for the *event itself*
  stays valid; the host records `HandlerError` in the event log).
- **Quarantine = 3 consecutive errors on the same (script, event) pair ⇒ the handler is
  unregistered** (`EventBus::unregister`) and `tracing::warn!`s a summary. Consecutive means
  back-to-back invocations without a successful call — one success resets the count. This
  bounds repeated cost from a broken while-fraction handler without removing scripts that hit
  one unfortunate edge once.
- Quarantine is in-memory only: the next hot reload that re-loads the script resets it.
  Quarantine state is not serialized (it is a runtime hygiene mechanism, not game state).
- A script whose *top level* fails at load quarantines the whole file: the blueprint it belongs
  to fails to spawn (`SpawnError::ScriptLoad`), matching the blueprint pass's fail-closed
  behavior; during hot reload a bad script, like a bad TOML, keeps the previous library (M2 §2
  policy).
- The same 3-strike policy quarantines `define_part`-only load errors at *spawn time*: a
  blueprint whose schema-registration script is broken never spawns (loud, logged once per
  mode).

### 6.3 Script hot reload

- `notify` watches `data/*/scripts/` (existing watcher in `crates/tessera/src/main.rs` gains a
  second watch path — or is factored into a small shared watcher; builder's choice, §9).
- On change: compile and sandbox-init the new script; on success, re-run its top-level
  registrations (define_part/on calls) against a *clone* of the registry; only on complete
  success swap the script set atomically (same pattern as `World::load`'s whole-library swap).
- **Live entities keep their old parts** (M2 §2 rule): the swap changes what *new spawns* get;
  existing `ScriptPart`s' data is untouched (their schema stays in the registry; changing a
  live schema is rejected with `ScriptError::Register` — a schema change requires a mod
  restart at v1; document it loudly).
- Newly reloaded handlers replace that script's old handlers (same `(script, event)` key);
  determinism is preserved because replacement is keyed, not append-only.
- Reload never touches `SimRng` (determinism rule; M2 test for this pattern carries over).

## 7. Determinism constraints specific to scripts

1. `os.time/os.clock/os.date/os.difftime` are nil-ed at host setup; scripts cannot read
   wall-clock. (Time-based behavior must come from the engine's tick counts.)
2. `math.random` is **replaced** with a function backed by a forked `SimRng` stream
   (`math.randomseed` is removed). This is the biggest cross-machine nondeterminism trap in
   vanilla Lua embedding, and Luau's `math.random` is un-seedable from script precisely because
   Roblox doesn't trust it — the host owns it.
3. No `f32`/float in gameplay-visible script data: part schema `kind` set has no `float` (§4.1);
   event params cross as integers; Lua *doubles* appear only in display-only scripts and for
   pure functions like `math.min` on integers (Luau ints are doubles internally — `to_script_params`
   marshals `i64`→integer-valued doubles and validates `x == math.floor(x)` on the way back).
4. Iteration order of anything script-visible (part field lists, event param tables) is sorted
   host-side before crossing the boundary; Lua-side `pairs` over plain tables is internally
   deterministic per identical script, but the *host never branches on `pairs` order*.
5. Table serialization across the boundary uses fixed field order (serde value maps sorted).
6. Compile-time constant folding in Luau is deterministic in Luau itself (same VM build);
   recorded Luau version in CI output (§2.2) covers build-to-build diffs.

## 8. CI and build-risk assessment

- **Vendored Luau is C++ but well-packaged.** `luau0-src` builds Luau from source with the `cc`
  crate; mlua's own CI builds it on Windows (MSVC), macOS and Linux. Expected additions for
  tessera CI: nothing at all on ubuntu/macos (C++ toolchains present in rustlang images); on
  windows-latest MSVC is the default toolchain and `cl.exe`/C++ workload is present in the
  standard `windows-latest` runner image. The **known risk** is MinGW (`windows-gnu`): Luau's
  CMake-era sources have historically had MSVC-oriented pragmas; mlua documents that vendored
  *module mode* is not supported on Windows and recommends MSVC ([mlua README, feature list]) —
  we don't use module mode, so nothing changes for us, but builders must not enable
  `windows-gnu` targets in CI.
- **Mitigations to encode in CI/builder cards:**
  1. Build matrix rule: each PR touching `tessera-script` runs `cargo test` on all three OSes
     (matching the existing CI: test on Windows and macOS per commit 2c59956).
  2. Record the vendored Luau version in build output (`luau --version` equivalent via
     `luaL_newstate` + `luau_version`); exposes luau0-src drift between CI cache misses.
  3. Compile time is real: vendored Luau adds minutes to cold builds on the VM sources. Use
     `Swatinem/rust-cache` semantics already present; scope the crate so Luau is only a
     dependency of `tessera-script` (never of `tessera-core`), so non-script CI paths skip it
     where possible.
  4. Fallback plan documented, not implemented: plain Lua 5.4 (`lua54` + `vendored`) remains the
     architecture's named fallback (architecture §3); if a Luau build breaks a CI target, the
     API layer (§5) is deliberately Lua-flavored so the swap is bounded to `tessera-script`.
- Risk verdict: **low-moderate**. The choice the architecture flagged as a risk is mitigated:
  mlua owns the C++ build, upstream CI proves the matrix, and our own CI is a plain
  `cargo test` extension, not a new toolchain.

## 9. Module boundaries (for builder cards)

- `tessera-core::script_part`: `PartValue`, `ScriptPart`, `ScriptPartType`/`FieldDef` +
  validation (`toml::Value` → `ScriptPart`), `PartRegistry` API additions
  (`register_script_part`, `script_schema`). ~1 builder.
- `tessera-script`: VM lifecycle, sandbox setup, budgets/interrupt, `ScriptHost`
  (spawn/get/set/on/api), error taxonomy, quarantine bookkeeping, versioning. ~1 builder; the
  largest card. Depends on the core card (schema types) — true dependency.
- `tessera` bin: blueprint `scripts = [...]` key + loader wiring into `World::load`'s flow,
  hot-reload watcher extension, demo: the reference behavior. ~1 builder; depends on both.

### 9.1 Demo mapping (the M4 milestone goal)

**One behavior written entirely in script: a `Zapper` goblin variant that spends one charge to
zap the player for an extra point of damage on hit.** (The demo shows the full loop — define
part, spawn, event, mutate, veto.)

Gameplay: the Zapper is a goblin with a limited stock of charges. Each of its attacks spends
one charge to deliver a zap: one extra point of damage to the defender, logged in the event log.
When its charges run out, the attack is a plain hit with no zap. Starting from 3 charges, two
attacks therefore cost the player 2 (1 per hit) + 2 (1 per zap) = 4 Health and leave the Zapper
at 1 charge.

```toml
# data/base/blueprints/zapper.toml
[Zapper]
inherits = "Goblin"
parts.Charge = { charges = 3 }
scripts = ["scripts/zapper.luau"]
```

```lua
-- data/base/scripts/zapper.luau
-- tessera-api: 1
engine.define_part("Charge", {
  fields = { charges = { kind = "int", default = 1, min = 0, max = 99 } },
})

local ATTACK_HOOK = engine.LATE  -- runs after the native armor mutator, before Health clamp

engine.on("Attack", ATTACK_HOOK, function(ev)
  local charge = engine.get_part(ev.attacker, "Charge")
  if charge and charge.charges > 0 then
    engine.set_part(ev.attacker, "Charge", { charges = charge.charges - 1 })
    local dmg = engine.get_part(ev.defender, "Health")
    if dmg then
      dmg.hp = dmg.hp - 1                        -- via the whitelisted Health.hp path (§4.4)
      engine.set_part(ev.defender, "Health", dmg)
    end
    engine.log(("zapper zaps %d"):format(ev.defender))
  end
  return true
end)
```

Success criteria (mirrors architecture M4 line): spawn a `Zapper` (charges = 3) next to the
player, let the scheduler act it twice. Each action is one Attack: 1 damage from the base hit
plus 1 zap damage, so the player's `Health` drops by 2 × 2 = 4, and the Zapper's `Charge` ends
at 3 − 2 = 1; `--headless n` prints the event log with the script handler's evidence ("zapper
zaps …" twice). **Plus one scripted action for the player**: arrow keys already route through
`Move`; the demo adds a `Zap` key (E) that dispatches `Attack` toward the last faced direction —
the player's own scripted strike, mirroring the Zapper's — proving scripts and native parts
share one dispatch path. (If E-Attack is too large, the fallback demo is the script's veto
behavior: a scripted handler cancels `Move` when `Charge.charges` is 0 — the Zapper is out of
charges and cannot zap; pick the smaller one at implementation time; the card carries both.)

## 10. Test matrix

`tessera-script` (all headless; no windowing):

| # | Test | Verifies |
|---|---|---|
| 1 | sandbox blocks `os.time`, `io.*`, global writes outside thread | §3.1 read-only state |
| 2 | memory limit triggers `ScriptError::Memory` (allocate a huge table) | set_memory_limit |
| 3 | `while true do end` handler hits instruction budget and errors; sim continues with next handler, determinism intact | §3.2 infinite-loop stopper |
| 4 | budget carries across nested `engine.*` calls (script calls engine.spawn whose own script runs) | §3.2 carry rule |
| 5 | cross-platform build/parse smoke: load every script in `data/base` on all 3 OSes (CI test) | §8 |
| 6 | define_part + TOML spawn: valid payload → fields present; unknown/wrong-kind/out-of-range → spawn error | §4.1 validation |
| 7 | schema change after registration is rejected | §6.3 |
| 8 | `on("Move")` script handler vetoes: event Cancelled recorded, position unchanged, no energy spend | §5.3 + M3 §2.6 |
| 9 | priority interleave: native armor EARLY mutator + script LATE mutator order asserted via event log | §5.3 ordering |
| 10 | quarantine: handler errors 3× (throw in script) → unregistered, 4th dispatch has no script handler, event log says so | §6.2 |
| 11 | quarantine resets on success (2 failures, 1 success, 1 failure → still registered) | §6.2 |
| 12 | reload: change script on disk → new handlers take effect for subsequent spawns, old live `ScriptPart`s untouched, SimRng stream unchanged | §6.3 |
| 13 | api-version mismatch → Load error before execution | §5.1 |
| 14 | determinism: scripted 200-tick run twice, identical event log + positions incl. a scripted `math.random` consumer | §7.2 |
| 15 | `--headless n` smoke with Zapper demo in CI | §9.1 |

`tessera-core` (schema storage):

| # | Test | Verifies |
|---|---|---|
| 16 | `ScriptPart` fields serialize/deserialize round trip (the M7-save-shaped serializer works) | §4.1 closed value set |
| 17 | `register_script_part` + `schema()` lookups deterministic ordering (BTreeMap) | §4.2 |

## 11. Explicitly out of scope for M4

`require`/module loading (§3.1), per-entity dynamic handler attach, mod manifests and patching
(M5), worldgen/FOV hooks (M6), saves (M7), async/timers, network/ffi/userdata/custom events from
scripts, `windows-gnu` targets, JIT/codegen.

## §12 Risks (residual)

1. **Borrow shapeshifting at the API boundary** (§3.3): the pending-actions pattern must be
   proof-tested for the `spawn` path (scripts spawning entities during a dispatch); the depth
   cap (16) plus handler-snapshot isolation gives a second containment layer.
2. **Instruction-budget calibration**: 5M instructions may be too tight for heavy content or too
   loose for worst-case frame pacing — it is a single constant; make tuning a config file knob
   *disabled by default* (content should not depend on tuning).
3. **Sorting cost on hot paths**: marshalling tables by sorted field order per event costs
   allocations; acceptable at mod scale, revisit with a pooled `Value` arena if profiling shows
   it (post-M7 roadmap, per architecture over-engineering-fear §9).
4. **Luau up-stream breakage** on luau0-src bumps: pin and record (§8). Fallback documented (§8).