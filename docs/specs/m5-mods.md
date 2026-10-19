# M5 spec: mod loader — manifests, load order, patches, zip VFS

Status: research spec for M5 (author: researcher bot; pending Jeff's approval). Grounded in
[architecture.md](../architecture.md) §4 (planned `tessera-mods` crate), §5 ("Mods are the
base game's format", VFS note), §6 (mod and script safety), §8 (M5 scope: *manifests, load
order, patch operations, zip-able folders via the VFS, sample mod that changes a blueprint
and adds a script*), [determinism.md](../determinism.md), the shipped M0–M6 code
(`crates/tessera-core/src/{vfs,blueprints,ecs}.rs`), and the M2/M4 specs
([m2-entities.md](m2-entities.md), [m4-scripting.md](m4-scripting.md)).

---

## 1. Decisions at a glance

| Question | Decision |
|---|---|
| Manifest format | `mod.toml` at mod root; strict `deny_unknown_fields`-style validation |
| Load order | Kahn topological sort on declared dependencies; stable tie-break = lexicographic mod id |
| Patch semantics | `replace`, `merge`, `remove` as explicit, per-key TOML ops in `patches/*.toml` |
| Zip handling | `zip` crate (pure Rust `ZipArchive` backend, `deflate` feature only) behind `Vfs` |
| Security | Reject absolute paths, `..`, drive letters, symlinks and hardlink entries in zips |
| Base data | `data/base` loads first as an implicit built-in mod; mods patch it; base files never mutated in place |

## 2. `mod.toml` manifest schema

A mod is a directory (or a zip whose root is a single directory) containing a `mod.toml` at
its root, plus content subdirectories (`blueprints/`, `scripts/`, `tiles/`, `patches/`).
An empty or missing manifest is a hard load error.

```toml
# mod.toml
id      = "more-monsters"        # required, unique, [a-z][a-z0-9_-]*, max 64
version = "0.3.1"                # required, semver (X.Y.Z with optional -pre+build)
api     = 1                      # required integer; must match TESSERA_API_VERSION
title   = "More Monsters"        # optional display string (max 80)
authors = ["jeff"]               # optional list of strings

[dependencies]
base = "0.1.0"                   # mod id -> semver requirement ("0.1.0" = ^0.1.0)
qud-forks = { version = "0.2", optional = true }

[load-order]
after  = ["qud-forks"]           # soft hints: edges added only if the mod is present
before = []
```

Rules:

- `id` and `version` are required; the `(id, version)` pair is the mod identity.
- `api` is required and checked against `tessera_core::TESSERA_API_VERSION`; a mismatch is a
  load error *before* any content file is read (matches the M4 script-header check).
- `dependencies` values are semver *requirements* (tolerant: exact, caret, tilde). A missing
  dependency entry or a version that does not satisfy the requirement is a load error.
- `optional = true` dependencies are skipped when the target mod is absent, but enforced
  (version + existence) when present.
- `load-order.after` / `load-order.before` add *soft* edges: they participate in ordering only
  when both mods are present. Soft edges can create cycles too (they are real edges when
  active) and must be diagnosed the same way.
- Unknown keys are a load error (same tight-schema policy as M2/M4).
- **Base data is an implicit mod** with fixed `id = "base"` that cannot be named in any
  mod's `dependencies` (it is always present and always first). Mods that reference content
  from base declare `base = "0.1.0"` as a dependency to be explicit.

## 3. Deterministic load order

### 3.1 Algorithm

Kahn's topological sort over a `BTreeMap<ModId, BTreeSet<ModId>>` edge set (determinism rule 3:
no `HashMap`, no `HashSet`, no unordered iteration anywhere in the loader):

1. **Discover:** scan the content root. For each mod source (directory or zip), read its
   `mod.toml`. Parse failures abort with the source path in the message. Discovery order is
   the *sorted* set of mod ids, not filesystem order (avoids platform differences in
   directory enumeration).
2. **Build edges:** for each mod, add hard edges from every present hard dependency, plus
   active soft edges from `load-order.after` / inverted edges from `load-order.before`.
3. **Topological sort:** repeatedly take the ready set (all prerequisites placed), pick the
   **lexicographically smallest mod id**, place it, remove it. This gives one canonical order
   for any given mod set, independent of discovery or platform enumeration.
4. **Base** is placed first by construction (no incoming edges; enforced by the check in §2).

### 3.2 Error taxonomy and diagnostics

```rust
pub enum LoadOrderError {
    Cycle { path: Vec<ModId> },                    // "mod cycle: a -> b -> c -> a"
    MissingDependency { mod_id, dep: ModId },
    VersionConflict { mod_id, dep, required: String, found: Option<String> },
    DuplicateId { id, first_path, second_path },   // two sources claim the same id
    InvalidManifest { path, msg },
}
```

- Diagnostics carry mod ids and source paths (matching the M2 `file + id` convention). All
  errors in a pass are collected and reported together where possible, then the load is
  rejected wholesale (fail-closed; see §7).
- **Cycles** report the full cycle path, not just "cycle detected". Soft edges that close a
  cycle are named in the message (e.g. `... -> more-monsters -> qud-forks -> more-monsters
  (soft edge from load-order.after)`).

### 3.3 Where it lives

`crates/tessera-mods/` (new crate per architecture §4): discovery + manifest parsing +
ordering. Depends on `tessera-core` (for `Vfs`, `TESSERA_API_VERSION`) and `toml`/`serde`/
`thiserror`/`tracing`. No GPU or windowing deps (determinism rule 6). Its `clippy.toml`
carries the same `disallowed-types` list as `tessera-core`.

## 4. Blueprint patch semantics

Mods edit existing blueprints via TOML patch files in `patches/*.toml`. Patch files load in
sorted path order *within* each mod, and mods apply their whole patch set after the mod's own
blueprint files (§2). A patch file targets one or more blueprint ids:

```toml
# patches/goblin.toml
["Goblin"]
replace = { parts.Render = { tile = "creature.goblin.blue" } }
merge   = { parts.Stats.strength = 20 }
remove  = ["parts.Throwable"]
```

### 4.1 Operations

| Op | Semantics |
|---|---|
| `replace` | Whole-value replacement at the dotted key path. Replaces the value entirely (table or scalar). A missing target path is an **error** (`ReplaceTargetMissing`). |
| `merge`   | Deep merge: tables merge recursively; scalars and lists replace. Used to tweak one field of a part payload without restating it. A missing intermediate table is an **error** (`MergeTargetMissing`) — patches never *create* structure. |
| `remove`  | List of dotted key paths. Removes the value at each path. A missing target path is an **error** (`RemoveTargetMissing`). Removing a whole part (`parts.X`) is allowed; removing a *required* engine component (e.g. `parts.Render` on a renderable entity) is allowed too — the entity simply fails to render and this is not caught here (consistency with the M2 model: render fallback is the magenta `?` tile). |

All three ops are **all-or-nothing per patch file**: the ops are applied to a copy of the
target blueprint; if any single op fails, the whole patch file is discarded, the blueprint
keeps its pre-patch state from earlier mods, and the load fails with the op, path and file in
the message. **Base data immutability on errors:** the base `data/base/blueprints/*.toml` files
on disk are never modified — patches apply to in-memory copies. If a patch errors, base content
on disk is untouched; only the in-memory load rejects and (during hot reload) the previous
library stays active (same keep-old-on-error policy as M2/M4).

### 4.2 Part-table behavior

`parts.<Name>` payload tables are values owned by their `PartRegistry` entry (M2/M4). Patches
operate on the raw `toml::Value` before typed deserialization, so:

- `replace`/`merge`/`remove` act on the raw value. Validation (schema check for `ScriptPart`,
  serde `deny_unknown_fields` for native parts) happens at *spawn* time on the post-patch value,
  not at patch time. A patch that produces an invalid part payload surfaces as the same
  `PartError::Schema`/serde error a hand-written bad blueprint would produce — same class of
  error, same fail-closed behavior.
- Deep merge of a part payload is defined as: for each key in the patch value, if the target
  value at that key is a table *and* the patch value is a table, recurse; otherwise replace.
  No type coercion (a table patch value over a scalar target is a replace, not an error —
  the spawn-time schema check catches invalid resulting payloads).
- Part names are not reserved or filtered: a patch may add a part to a blueprint if that part's
  name exists in the registry (the registry is checked at spawn). A patch adding a completely
  unknown part name fails at spawn with the standard M2 "unknown part" error, not at patch time.

### 4.3 Ordering with blueprints

For each mod in load order (§3): load its `blueprints/*.toml` (adding new ids, redefining
existing ones — last wins, per M2), then apply its `patches/*.toml` in sorted path order. Two
mods patching the same blueprint apply sequentially in load order; within one mod, patch file
sort order decides. A patch targeting a blueprint id that does not exist after the blueprint
pass of *this* mod (and all earlier mods, including base) is an error (`PatchTargetMissing`).

## 5. Zip VFS

### 5.1 Dependency choice

`zip = "6"` (pure Rust, well-maintained, no C toolchain — matters because vendored Luau already
brings C++ into the M4 build; a second C/C++ dependency would compound the §8 build risk from
the M4 spec). Features: `deflate` only — no `aes-crypto`, no `bzip2`, no `zstd`, no `time`.
The `time` feature in particular is avoided: it pulls chrono and produces nondeterministic
timestamps we do not need (content files are read through `Vfs::read`, not stat'd).

Rejected: `tar` (not a container format for mods — no random access, different tooling on
Windows), `zip-rs` 0.6 legacy line (unmaintained), rolling our own DEFLATE reader.

### 5.2 `ZipVfs` implementation

A new `Vfs` implementation in `tessera-core::vfs` alongside `DirVfs`/`MemVfs`:

- Constructed from a `ZipArchive<R>` over any `Read + Seek` (file on disk or `Cursor<Vec<u8>>`
  in tests). Reads a whole file entry into `Vec<u8>` on `read` (no streaming partial reads —
  mod content files are small).
- `list(dir)` returns the same sorted, `/`-separated, recursive relative path list as
  `DirVfs` (determinism rule 3 / M2 sorted-load contract) — sorted from the archive's
  `file_names()`, deduplicated (zip allows duplicate entry names; **duplicates are an
  error at construction**, matching the manifest duplicate-id rule).
- **Path security, applied at construction and on every `read`:**
  - Reject absolute paths (`/x`, `\x`), Windows drive prefixes (`C:\`, `C:/`), and any `..`
    component (mirrors the existing `DirVfs::resolve` escape check in `vfs.rs:37-45`).
  - Reject entries with symlink or other non-regular-file attributes (the `zip` crate exposes
    `unix_mode()` / `is_dir()`; a non-file, non-directory entry is an error).
  - Normalize `\` to `/` on entry names (Windows-authored zips) **before** the checks above,
    so a `\..\` traversal cannot slip past a `/`-only filter.
- The root is either the archive root itself (if it contains `mod.toml` directly) or the
  single top-level directory (if the archive wraps the mod folder — the common
  "download zip from GitHub" shape). Both are supported; ambiguous archives (two top-level
  dirs, no `mod.toml` anywhere) are an error at load with the archive's entry list in the
  message.

## 6. Sample mod layout

```
my-mod/
  mod.toml
  blueprints/
    extra-creatures.toml
  patches/
    goblin.toml
  scripts/
    zapper-tweak.luau
  tiles/
    recolors.toml
```

- `blueprints/`, `patches/`, `scripts/`, `tiles/` are conventional subdirectory names the
  loader recognizes; unknown top-level entries are ignored (a mod may carry docs or assets).
- A mod with no `blueprints/` or no `patches/` is valid (a pure tileset or pure script mod).

### 6.1 Demo mapping (the M5 milestone goal)

A sample mod `data/mods/sample-mod/` ships in the repo and is loaded by the existing
`main.rs` alongside base:

```toml
# data/mods/sample-mod/mod.toml
id      = "sample-mod"
version = "0.1.0"
api     = 1

[dependencies]
base = "0.1.0"
```

```toml
# data/mods/sample-mod/blueprints/bogling.toml
[Bogling]
inherits = "Goblin"
parts.Charge = { charges = 5 }
scripts = ["scripts/bogling-tweak.luau"]
```

```toml
# data/mods/sample-mod/patches/goblin.toml
["Goblin"]
replace = { parts.Render = { tile = "creature.goblin.angry" } }
merge   = { parts.Stats.strength = 22 }
```

```lua
-- data/mods/sample-mod/scripts/bogling-tweak.luau
-- tessera-api: 1
engine.on("Attack", engine.LATE, function(ev) return true end)
```

Success criteria (mirrors architecture M5 line): the demo spawns a `Bogling` (a new blueprint
that only exists because the mod added it, with the mod's `Charge` part) and a regular `Goblin`
whose tile is the mod's `creature.goblin.angry` (patched from base) and whose `strength` is 22
(merged from base's 18). A `--headless n` run prints the event log showing the Bogling's
scripted handler firing. A zip built from the same folder loads identically through `ZipVfs`
(the same mod, packed and unpacked, produces the same run — the "zip-able folders via the VFS"
acceptance from architecture §8).

## 7. Determinism constraints honored

From [determinism.md](../determinism.md), applied here:

1. All mod discovery, ordering and patch application use `BTreeMap`/`BTreeSet` and sorted
   `Vec`s. The loader never branches on filesystem enumeration order, zip entry order, or
   archive metadata timestamps (the `zip` `time` feature is not enabled — §5.1).
2. Load order is a pure function of the *set* of mods and their manifests, not of discovery
   order (§3.1 canonical tie-break). Two machines with the same mod set produce the same
   load order, and therefore the same blueprint content and the same `SimRng`-seeded run.
3. No `HashMap`/`HashSet` in `tessera-mods`; the crate carries its own `clippy.toml` with the
   same disallowed list (determinism.md explicitly calls this out for new sim crates).
4. Single-threaded load; `notify`'s watcher thread only triggers a rebuild and hands the
   result over (same pattern as M2/M4 reload).
5. Patch application is pure data transformation on `toml::Value`; no RNG, no clock, no
   platform-specific paths.

## 8. Module boundaries (for builder cards)

**In scope (M5):**
- `tessera-core::vfs`: `ZipVfs` (§5.2), path-security checks shared with `DirVfs` (factor the
  `resolve` validation into a helper both use).
- `tessera-mods` (new crate, §4): `ModManifest` parsing + validation, discovery (dir + zip),
  `LoadOrderError` + topological sort, `ModRegistry`.
- `tessera-core::blueprints` extension: patch application (`replace`/`merge`/`remove` on
  `toml::Value`) hooked into the existing load pass, with the §4.1 error cases.
- `tessera` bin: wire mod loading into `main.rs`'s content load, ship `data/mods/sample-mod/`,
  extend the hot-reload watcher to mod dirs.
- Tests: `MemVfs` + `ZipVfs` unit tests, load-order tests (cycle, missing dep, tie-break),
  patch-op tests (each op, each error case, base-immutability), the §6.1 demo acceptance test.

**Out of scope (explicitly):**
- Mod *distribution* (download sites, signatures, checksums) — only local folders and zips.
- Optional-dependency version *resolution* beyond the §2 simple requirement check (no SAT
  solver, no diamond-dependency negotiation — dependencies form a tree in practice; a real
  conflict is reported, not resolved).
- Runtime mod enable/disable UI, per-save mod sets (M7 saves will need to record the mod list;
  the schema in §2 is save-friendly but storage is M7's problem).
- Native plugin mods (architecture §6 defers them).

**Builder card sketch:** (1) `ZipVfs` + shared path-security helper + tests; (2) manifest +
discovery + load order + tests; (3) patch ops + blueprint-loader integration + tests;
(4) `tessera` bin wiring + sample mod + demo acceptance + zip round-trip test. Cards 1–2 are
independent; 3 depends on 2's load-order output; 4 depends on all three.

## 9. Open decisions and risks

- **Optional dependencies' interaction with load-order hints:** a mod may declare
  `load-order.after = ["x"]` for a mod that is only an optional dependency. The soft edge is
  active only when `x` is present; if the optional dependency is absent, the hint is silently
  inert. This is the recommended reading of the schema but is called out here in case Jeff
  prefers soft edges to be a hard error when the target is absent.
- **Patch-time vs spawn-time validation for parts** (§4.2): patches operate on raw
  `toml::Value` and validation is deferred to spawn. The alternative (validate the patched
  payload at patch time against the registry) gives earlier errors but needs the registry at
  patch time and duplicates M2's spawn-time checks. The spec recommends defer-to-spawn; Jeff
  may prefer eager validation for a tighter fail-fast loop during mod authoring.
- **Risk: zip duplicate entries.** The `zip` crate tolerates duplicate names; we error at
  construction (§5.2). This is a deliberate fail-closed choice — a duplicate-name archive is
  either corrupt or an attack.
- **Risk: soft-edge cycles from `load-order`.** Two mods each declaring `after = [other]`
  create a cycle that only exists because of hints. The error message names the soft edge
  (§3.2) so the fix is obvious to a mod author.
