# M7 spec: saves and developer tooling

Status: research spec for M7 (author: researcher bot; pending Jeff's approval). Grounded in
[architecture.md](../architecture.md) §3 (`Save/load: serde + postcard/bincode, versioned
schemas, zstd`; `Dev UI: egui — Inspectors and a console (M7)`; `Logging/profiling: tracing`),
§4 (planned `tessera-ui` crate), §5 ("The simulation is headless and deterministic, so it runs
in tests and a CLI"), §8 (M7 scope: *versioned saves, debug console, egui entity inspector*),
[determinism.md](../determinism.md), and the shipped M0–M6 code
(`crates/tessera-core/src/{ecs,events,turns,rng,zone}.rs`, `crates/tessera/src/{sim,main}.rs`).

External version evidence (checked via crates.io API on 2026-10-19; re-verify at implementation):

| Crate | Version | Evidence |
|---|---|---|
| `egui` | 0.37.0 (2026-10-15) | `rust_version = "1.95"`, edition 2024 |
| `egui-wgpu` | 0.37.0 | depends on `wgpu ^30.0` (non-optional) and `winit ^0.30.13` behind the optional `winit` feature; `rust_version = "1.95"` |
| `egui-wgpu` | 0.36.1 | same deps (`wgpu ^30.0`, `winit ^0.30.13`); `rust_version = "1.95"` |
| `egui-wgpu` | 0.35.0 | targets `wgpu ^29.0` — incompatible with our wgpu 30 |
| `postcard` | 1.1.3 (2025-07) | serde ^1.0.100, edition 2021, actively maintained (31M recent downloads) |
| `bincode` | 3.0.0 (2025-12) | **dropped the pure-serde encode/decode API** (own traits/derive); bincode 2.x keeps a `serde` compat feature |
| `zstd` | 0.14.3 (2026-10) | C zstd bindings via `zstd-sys`; `vendored` feature builds C in-tree; `rust_version = 1.64` |

Repo facts: workspace `rust-version = 1.85` (crates `tessera`, `tessera-core`, `tessera-render`,
`tessera-script`, `tessera-mods`); `wgpu = "30.0.1"`, `winit = "0.30.13"`; `serde = 1.0.229`
with derive already in `tessera-core`. `hecs = 0.11`.

---

## 1. Decisions at a glance

| Question | Decision | Confidence |
|---|---|---|
| Serialization format | **postcard 1.1.3** (`use-std`), zstd level 3 wrapper | high |
| Format versioning | 12-byte plaintext header (magic + u32 version) *before* the zstd frame; unknown version = hard error, no silent migration | high |
| Save payload strategy | Whitelist part-snapshot registry (native parts snapshot explicitly; script parts already Serialize); unknown part on save = fail-closed error | high |
| Atomic writes | temp file in the save dir → `sync_all` → backup old via rename → rename over → startup sweep of stale temp files; Windows sharing-violation retry | high |
| egui integration | `egui-wgpu` 0.37 + `egui-winit` 0.37 wired directly onto the existing winit/wgpu objects (not eframe) | high |
| Release exclusion | cargo feature `devtools` (non-default); release/dist builds omit it | high |
| **Rust MSRV** | egui 0.36/0.37 require `rust_version = 1.95`; workspace pins 1.85 → **bump to 1.95 needs owner approval** | **BLOCKER** |
| Console | line grammar (`spawn`, `list`, `entities`, `print`, `sethp`, `tp`, `save`, `load`, `help`); parsed to a typed `ConsoleCommand` enum, executed between turns | medium |
| Inspector | read-only egui window; per-frame snapshot taken under the broker read lock; entity list in deterministic id order | medium |
| Where the code lives | save module in `tessera-core`; console/inspector as a `devtools` module inside the `tessera` binary (new `tessera-ui` crate deferred) | medium |

## 2. Serialization: postcard + zstd

### 2.1 Choice: postcard 1.1.3 over bincode

Rationale:

1. **The workspace already speaks serde everywhere** (blueprints, tilesets, manifests, script
   parts). Postcard is a plain serde format — `#[derive(Serialize, Deserialize)]` is the only
   requirement. Bincode 3.0 (current, 2025-12) abandoned its serde frontend for its own
   `encode`/`decode` traits + derive, which would fork the codebase's serialization story; using
   bincode 2.x instead means depending on a "previous major with a compat feature".
2. **Stability of the dependency.** Bincode churned majors (1 → 2 → 3 in ~2 years, each breaking).
   Postcard has been 1.x since 2021 with the same wire format.
3. **Size.** Postcard's varint + discriminator encoding is consistently smaller than
   fixed-width bincode; after zstd the absolute difference is small, but small inputs (the demo
   save is a few KB) benefit from the shorter representation and it costs nothing.
4. Architecture §3 lists both; this spec resolves the ambiguity.

Crate config:

```toml
# crates/tessera-core/Cargo.toml
postcard = { version = "1.1.3", default-features = false, features = ["use-std"] }
zstd = { version = "0.14.3", default-features = false, features = ["arrays", "zdict_builder"] }
```

`use-std` enables `alloc`/`std` Serde impls. zstd pulls in the C library (`zstd-sys`); CI builds
Linux/macOS/Windows already for Luau's vendored C++, so a compiled C dependency is an accepted
pattern in this repo. Add the `vendored` feature only if a CI runner lacks a system zstd —
verify at implementation; **blocker B2** below records the CI check.

Compression level: **3** (zstd's default). Saves are small; higher levels waste startup time.

### 2.2 File layout and format version

```
offset 0..4   magic  b"TESS"            (rejects wrong/foreign files before any decompress)
offset 4..8   u32 LE format version  (1)
offset 8..12  u32 LE postcard payload length (pre-compression; sanity check)
offset 12..   zstd frame of the postcard-encoded SaveData
```

- The header is **plaintext, before compression**, so `load` can reject an unknown version
  without touching the decompressor and the error can name both numbers.
- `SAVE_FORMAT_VERSION = 1` is a `pub const` in `tessera-core::save`.
- Loading a version `> SAVE_FORMAT_VERSION` or any unknown value returns
  `SaveError::UnsupportedVersion { found, supported: SAVE_FORMAT_VERSION }` — a **hard error**,
  never a best-effort parse. Forward migration (a v2 reader upgrading a v1 file) is explicitly
  deferred to when a v2 exists; until then the error message tells the user the file was written
  by a newer build. This is the "explicit format version and unknown-version migration error"
  the milestone asks for, with migration parked until there is a second version to migrate.
- Load order of defenses: magic mismatch → `SaveError::BadMagic`; version mismatch →
  `UnsupportedVersion`; zstd frame error → `SaveError::Corrupt`; postcard deserialization
  error → `SaveError::Corrupt` (with the postcard message as cause). Never panic on bad input.

### 2.3 `SaveData` payload and the part-snapshot problem

`Box<dyn Part>` is type-erased, so entities cannot serialize generically. Two options were
weighed:

- **(A) Serialize everything generically** — would require `Part: Serialize` as a trait bound.
  Rejected: it drags serde into every future part's signature and forces it on mod-defined
  script parts that are already handled separately.
- **(B) Whitelist snapshot registry (chosen).** A `SaveHooks` map from `PartTypeId` to a
  `(fn(&dyn Part) -> Result<toml::Value, PartError>, fn(&toml::Value) -> Result<Box<dyn Part>, PartError>)`
  pair, registered alongside the existing `PartRegistry` (the parts already round-trip through
  `toml::Value` configs at blueprint-load time, so the snapshot direction is symmetric).

```rust
pub struct SaveData {
    pub version: u32,                  // == SAVE_FORMAT_VERSION, also in the header
    pub zone_id: String,               // zone reloads from data (deterministic); id is a check
    pub next_entity_bits: Vec<u64>,    // hecs entity ids via Entity::to_bits/from_bits
    pub entities: Vec<SavedEntity>,    // id + Vec<(part name, toml::Value)>
    pub player: Option<u64>,
    pub clock: u64,                    // Scheduler::ticks()
    pub actors: Vec<(u64, i64, u32)>,  // entity id, energy, speed (scheduler state of record)
    pub rng: SimRngState,              // ChaCha8 stream state, see below
    pub player_turns: u64,
    pub facing: (i32, i32),
}
pub struct SavedEntity { pub bits: u64, pub parts: Vec<(String, toml::Value)> }
```

- **Zone is not serialized.** It is a pure function of mod data (determinism.md), so the loader
  rebuilds it from the same pack and the save records only `zone_id`, which must match — a
  mismatch is `SaveError::ZoneMismatch` (saves are pack-specific; fail closed rather than
  teleport entities into a different map).
- **Script parts** already implement `Serialize`/`Deserialize` over their data tables
  (`script_part.rs`); their hook serializes via the existing TOML value.
- **RNG state.** `SimRng(ChaCha8Rng)` must round-trip or determinism-after-load dies. `rand_chacha`
  exposes serde support behind its `serde1` feature — enable it in `tessera-core` and implement
  `Serialize`/`Deserialize` for `SimRng` by delegation. Verified requirement: the
  `known_values_are_stable` test (determinism.md pins this) extends to
  "loaded stream produces the same sequence as the unsaved one" (§8 test list).
- **Event bus handlers are not saved.** They are re-registered on load from blueprints/scripts
  exactly as at zone start; a save captures *state*, not wiring.
- Unknown part type on **save**: `SaveError::UnserializablePart(name)` — fail closed with the
  name so the gap is obvious. Unknown part name on **load**: the hook registry rejects it the
  same way (a save from a build with more parts than the loader is not silently truncated).

## 3. Crash-safe atomic save writes

Goal: a process kill (or power loss) at any instant leaves either the previous save or the new
save intact and loadable — never a half-written file.

### 3.1 Write sequence (`tessera_core::save::write_atomic`)

```text
1. tmp = save_path.with_extension("tmp")          // same directory ⇒ same volume
   (if a previous tmp exists it is overwritten via File::create — see sweep note)
2. write header + zstd frame into tmp; flush; tmp.sync_all()
3. if save_path exists: rename save_path → save_path.with_extension("bak")   // best effort
4. rename tmp → save_path
5. sync parent directory (Unix only; #[cfg(unix)] — no portable equivalent on Windows)
```

- **Same-directory temp file** is required for rename atomicity (cross-device rename fails;
  POSIX rename within a directory is atomic w.r.t. concurrent readers).
- **Windows semantics.** `std::fs::rename` on Windows uses `MoveFileExW` with
  `MOVEFILE_REPLACE_EXISTING`, so step 4 does replace an existing destination — but it can fail
  with a sharing violation (`ERROR_SHARING_VIOLATION`) when another handle has the destination
  open, which antivirus/indexer scans cause in practice. Mitigation: retry steps 3–4 up to 3
  times with a short sleep between attempts; on final failure return
  `SaveError::WriteFailed(path, cause)` and **leave the previous save and `.bak` untouched**
  (the tmp file stays and is swept on next startup). This is also why step 3 (rotate old →
  `.bak`) happens *before* the final rename: after it succeeds there is a valid older copy even
  if step 4 then fails.
- `.bak` rotation is best-effort: a failure to create the backup is logged and the save
  proceeds (the tmp→final rename is still atomic; only the extra recovery copy is lost).
- **Temp cleanup.** On game startup, before any load, delete files matching
  `*.tmp` / `*.tmp.*`-pattern siblings in the saves directory. The `.bak` file is never swept.
- **Load fallback.** If `save_path` fails to load with `Corrupt`/`BadMagic` and a `.bak` exists,
  surface both facts to the caller (the console/UI decides; M7's CLI path prints a warning and
  loads the `.bak` only with `--load <path>` pointed at it explicitly — no silent fallback,
  matching the repo's fail-closed style).
- fsync of the file (step 2) is what makes rename-after-crash safe on ext4/APFS/NTFS; skipping
  it risks the rename landing while the data blocks have not. The parent-dir fsync (step 5) is
  the POSIX guarantee that the *rename* itself survives power loss; it is best-effort and
  Windows builds skip it.

### 3.2 Default save location

`--save-dir <dir>` CLI flag, defaulting to `./saves` (relative to cwd, matching how `--data`/
`--mods` already behave). A platform data-dir crate (`dirs`) is deliberately *not* added in M7 —
flag for the future when the game stops being a developer-run demo.

## 4. egui integration and release exclusion

### 4.1 Versions and integration path

- Repo pins: `wgpu 30.0.1`, `winit 0.30.13` (Cargo.toml + Cargo.lock).
- `egui-wgpu 0.37.0` (and 0.36.1) depend on exactly `wgpu ^30.0` and, behind its optional
  `winit` feature, `winit ^0.30.13` — **first-class match, no pinning or patching needed**.
  0.35 and older target wgpu 29 and below, so the version choice is forced to ≥ 0.36.
- **Do not use `eframe`.** eframe owns the event loop and window; the demo already has a working
  winit 0.30 `ApplicationHandler` (`crates/tessera/src/main.rs`) and a hand-rolled wgpu surface.
  The integration is:
  - `egui_winit::State::new(...)` fed `WindowEvent`s from the existing handler
    (`on_window_event` returns whether egui consumed the input — game keys are suppressed when
    it does, so typing `s` in the console does not move the player).
  - `egui_wgpu::Renderer` created from the *existing* `Device`/`Queue`/surface format; each
    frame: `egui::Context::run` builds the UI → `renderer.update_buffers` →
    `renderer.render` into the same render pass after the cell renderer.
  - Feature set: `egui-wgpu = { version = "0.37", default-features = false, features = ["winit"] }`,
    and `egui-winit = { version = "0.37", default-features = false, features = ["clipboard", "wayland", "x11", "accesskit"] }`
    trimmed to what CI platforms need (verify exact feature list at implementation — macOS
    needs none of the Linux clipboard backends).

### 4.2 BLOCKER B1 — MSRV conflict (needs owner decision)

Every egui release that targets wgpu 30 (`0.36.1`, `0.37.0`) declares
`rust_version = "1.95"`; the workspace pins `rust-version = 1.85`. There is **no egui version
compatible with both wgpu 30 and Rust 1.85**. Options:

1. **(Recommended) Bump workspace `rust-version` to `1.95`** when adopting egui 0.37. Cheap
   (stable toolchain is well past this), but it is a workspace-wide contract change → needs
   Jeff's approval per the task policy on architecture decisions.
2. Ship M7 dev tools without egui (a plain-key console + text dump inspector) and defer egui to
   an MSRV bump later. Weaker demo, but zero MSRV movement.
3. Vendor/patch egui with a lowered MSRV — rejected; maintenance poison.

**Do not start egui work until B1 is answered.** Everything else in M7 (saves, console engine,
headless paths) is MSRV-neutral and can proceed.

### 4.3 Release exclusion: feature flag (not dev profile)

Cargo profiles cannot gate dependencies or code; dev-dependencies are never built into the
binary. So the mechanism is a feature:

```toml
# crates/tessera/Cargo.toml
[features]
devtools = ["dep:egui", "dep:egui-winit", "dep:egui-wgpu"]

[dependencies]
egui     = { version = "0.37", optional = true, default-features = false }
egui-winit = { version = "0.37", optional = true, default-features = false, features = ["clipboard"] }
egui-wgpu  = { version = "0.37", optional = true, default-features = false, features = ["winit"] }
```

- All devtool modules live under `#[cfg(feature = "devtools")]`; a release build without the
  feature compiles none of egui (no binary-size or dependency cost, CI keeps building the
  default feature set clean).
- **Dev workflow:** `cargo run -p tessera --features devtools`. CI adds one matrix job (or an
  extra command) building `--features devtools` so the gated code cannot rot — a
  `devtools`-only CI job is part of the staged plan.
- Console commands also work headless (§5), and the headless path is compiled unconditionally
  (it has no egui dependency) — only the *windowed* console UI is gated.
- `--frames <n>` smoke runs keep working with and without the feature.

## 5. Debug console

### 5.1 Grammar

Line-oriented, whitespace-tokenized (no shell quoting in M7; quoted args arrive when a command
needs them). `#` starts a comment. Numbers are `i32`/`u64` as appropriate; parse errors print
usage, never panic.

```text
help                              — list commands
list                              — list blueprint ids in the loaded library
entities                          — list live entities: <id> <blueprint> <pos> <hp>
spawn <blueprint> [x y]           — spawn; default = zone start tile
kill <entity-id> | kill all <blueprint>
print <entity-id>                 — dump an entity's parts (the inspector text path)
sethp <entity-id> <n>             — write the Health part
tp <entity-id> <x> <y>            — teleport (writes Position)
save [name]                       — write a save (default name "quicksave")
load <name>                       — replace live state from a save
clear                             — clear the transcript
```

### 5.2 API shape (`crates/tessera/src/devtools/console.rs`, feature-gated UI, ungated engine)

```rust
pub enum ConsoleCommand { Spawn { blueprint: String, at: Option<(i32, i32)> }, /* … */ }
pub fn parse(line: &str) -> Result<ConsoleCommand, String>;
pub enum ConsoleOutcome { Ok(String), Err(String), ReloadWorld /* load */ }
pub fn execute(sim: &mut Sim, cmd: ConsoleCommand) -> ConsoleOutcome;
```

- **Parsing is pure and unit-tested without a sim.** Execution takes `&mut Sim` and goes through
  the same public surface the player actions use (`Sim::submit_action` for turn-affecting
  commands; direct broker access only for `print`/`entities`, which are reads).
- **Determinism rule:** commands execute *between* player turns, never during event dispatch.
  Every executed command appends a line to the sim's event log
  (`CMD spawn Goblin tick=123 at=10,4`), which keeps the headless replay log a complete record
  of how a state was reached — consistent with `run_headless`'s log contract.
- Console output mapping: windowed mode renders the transcript in an egui console panel
  (input line at the bottom, ` (backtick) toggles the panel); headless `--headless` runs
  accept `--exec <line>` (repeatable) executed before the scripted actions, printing outcomes to
  stdout — so console behavior is CI-testable without a window.

## 6. Entity inspector (windowed, feature-gated)

Scope for M7 — **read-only**:

- An egui window (toggle with F3) listing live entities in deterministic entity-id order:
  `#<id> <blueprint> (x, y) hp=<n>`.
- Selecting an entity shows each of its parts: the `PartTypeId` name plus the part's `Debug`
  formatting (`Part: Debug` is already required by the broker). No editing in M7 — the console
  (`sethp`/`tp`) is the mutation path, and it logs; live inspector editing would bypass the
  event bus and break the determinism story.
- **Data flow** preserves the "renderer is a strict reader" rule: each frame (or each turn,
  whichever is cheaper — per-frame behind a dirty flag) the devtools module builds a
  `InspectorSnapshot { entities: Vec<EntityRow> }` by locking `Arc<RwLock<EntityBroker>>` for
  read, then drops the lock before egui draws. egui never touches the broker directly.
- Script-owned state (e.g. goblin AI internals) is not exposed in M7; only native parts.
  Listed as a non-goal with a note that a scripted `describe()` hook is the eventual path.

## 7. Module boundaries

```text
crates/tessera-core/src/save.rs     # SaveData, postcard+zstd codec, format version,
                                    # write_atomic/load, SaveError — no winit/wgpu deps
crates/tessera-core/src/rng.rs      # + Serialize/Deserialize for SimRng (serde1 feature)
crates/tessera/src/devtools/mod.rs      # feature-gated: egui wiring, panel toggles
crates/tessera/src/devtools/console.rs  # parser + executor (parser ungated)
crates/tessera/src/devtools/inspector.rs# snapshot builder + egui window
crates/tessera/src/main.rs          # --save-dir/--load/--exec args; egui frame hooks behind cfg
```

- The save codec lives in `tessera-core` because the *state it captures* is core state and the
  future CLI/replay tooling will need it without a window. File I/O in core is acceptable (the
  VFS already does I/O there); `write_atomic` takes plain paths, not the content `Vfs`.
- No new crate in M7. The planned `tessera-ui` crate (architecture §4) stays deferred until the
  devtools module grows a second consumer — consistent with "crates are split when a second
  consumer or a dependency boundary justifies it".
- **Hotspot warning:** `crates/tessera/src/main.rs` is touched by nearly every milestone card
  (args, event-loop, wiring). The egui integration should be isolated in `devtools/mod.rs` with
  two small `#[cfg]` call sites in `main.rs` (input hook + paint hook) to keep the diff small.

## 8. Demo controls / output mapping, README changes

| Input | Windowed (devtools) | Headless |
|---|---|---|
| `` ` `` (backtick) | toggle console panel | — |
| F3 | toggle inspector window | — |
| `--exec "<cmd>"` | executed at startup, transcript shows result | same, printed to stdout |
| `--save-dir <dir>` | save target | same |
| `--load <name>` | load at startup | same |
| `--headless <n>` | unchanged | unchanged; log now includes `CMD` lines |

README updates: the Run section gains `--features devtools`, the console/inspector keys, the new
CLI flags, and a "Saves" paragraph (format version, `.bak` behavior, unknown-version error
text). All five M7 architecture items map to visible behavior: a save written → file in
`--save-dir`; console commands → transcript/stdout; inspector → F3 window.

## 9. Tests

1. **Round-trip:** build a populated sim (player + goblin + script parts), save to memory
   (bytes API exposed for this), load, compare `SaveData` equality and continue both worlds
   with the same action script — identical event logs after reload.
2. **RNG continuity:** save mid-stream, load, assert the next `next_u32()` values match the
   uninterrupted stream (pins the ChaCha8 state round-trip).
3. **Version rejection:** header with version 99 / bad magic / truncated file → the three
   distinct error variants, no panic.
4. **Atomic write:** happy path leaves save + no tmp; failure-injected rename (unwritable dir)
   leaves the previous save intact; `.bak` rotation produces a loadable backup.
5. **Temp sweep:** plant stale `*.tmp` files, run the sweep, assert they are gone and `*.bak`
   survives.
6. **Console parser:** every command's happy path + malformed variants (`spawn` with no arg,
   `tp` with non-numeric y) return errors, not panics.
7. **Console via headless:** `--exec "spawn Goblin 5 5"` in `--headless` mode shows the spawn
   `CMD` line and the entity in the final map.
8. **Save/load determinism:** the M0-pinned `known_values_are_stable` gains a
   save→load→same-sequence sibling.
9. **devtools-off build:** `cargo build -p tessera` (default features) compiles with no egui in
   the lock graph — asserted in CI by the existing job plus the new feature job.

## 10. Staged implementation plan (small builder cards)

1. **Card A — codec core:** `save.rs` types + postcard/zstd round-trip + version/magic errors +
   `SimRng` serde. Tests 1–3. (No file I/O yet.) *Depends on B1 only if… no — MSRV-neutral.*
2. **Card B — atomic file layer:** `write_atomic`/`load` + tmp sweep + `.bak` rotation.
   Tests 4–5.
3. **Card C — snapshot hooks:** whitelist registry for `Position`/`Health`/`ActorEnergy`/script
   parts; wire save/load into `Sim` (new `Sim::save`/`Sim::load`), zone-id check.
4. **Card D — CLI + console engine:** `--save-dir/--load/--exec` args, parser + executor +
   `CMD` log lines. Tests 6–8. README flag/keys section.
5. **Card E — egui wiring (gated; needs B1 resolved):** feature + deps, `egui_winit` input hook,
   `egui_wgpu` render pass, backtick/F3 toggles, console panel UI, CI feature job. Test 9.
6. **Card F — inspector window:** snapshot builder + read-only entity list/detail pane; README
   inspector paragraph.

Cards A–D are one PR-sized batch each and unblock the milestone's headless half; E and F carry
the egui risk and wait on the B1 decision.

## 11. Blockers and open questions

- **B1 (owner decision, blocks Card E/F):** egui 0.36/0.37 require `rust_version = 1.95`;
  workspace pins 1.85 (§4.2). Recommend bumping to 1.95; needs approval.
- **B2 (implementation-time check, blocks Card B):** confirm CI runners can build the C zstd
  (`zstd-sys`) on all three platforms; if not, add the `vendored` feature. The Luau C++ build
  already proves C++ toolchains exist on the runners, so risk is low.
- **B3 (minor, blocks Card C):** `rand_chacha`'s `serde1` feature must expose the exact ChaCha8
  state used by `rand_core 0.10` — verify the feature still exists at that version; fallback is
  serializing the 32-byte seed + stream counter manually (both are public API).
- Open question for the owner (non-blocking): should `load` support cross-zone saves later, or
  is pack-specific locking (current `ZoneMismatch` proposal) the permanent rule?
