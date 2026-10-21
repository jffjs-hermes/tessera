# S0 spec: content schema — tuning, abilities/skills/statuses as blueprints, and the Shield Bash path

Status: research spec for S0 — Schema spike (author: researcher bot; pending Jeff's
approval). Grounded in [architecture.md](../architecture.md) §5 (blueprints, "mods are the
base game's format"), §6 (native/script boundary), §8 (S0 line), the roadmap S0 entry
([roadmap.md](../roadmap.md) § "Slice roadmap: S0–S11"), the shipped M2/M4/M5 code
(`crates/tessera-core/src/blueprints/`, `crates/tessera-core/src/ecs.rs`,
`crates/tessera-core/src/script_part.rs`, `crates/tessera-script/src/host.rs`,
`crates/tessera/src/main.rs` hot-reload wiring), the M2/M4/M5/M7 specs
([m2-entities.md](m2-entities.md), [m4-scripting.md](m4-scripting.md),
[m5-mods.md](m5-mods.md), [m7-saves-tooling.md](m7-saves-tooling.md)), and the design notes
`Specs/Content Schema Sketch.md`, `Specs/Slice Implementation Roadmap.md`,
`Specs/Slice Readiness.md`, `Design/Classes/Knight.md`, `Specs/Combat Sim Spec.md`.

**Scope boundary:** S0 settles the *content schema* — what a skill, ability, status, item or
quest is as data, how tuning tables are typed and read, and one end-to-end proof
(Shield Bash). S2 owns the combat pipeline, scenario/bot formats and band policy; where this
spec names an event that S2 will define in detail (`Hit`, `Blocked`, …) it only fixes the
*name, payload and dispatch position*, not the resolution rules.

---

## 1. Decisions at a glance (the four S0 open questions)

| # | Question (roadmap §7 review list, items 2–5) | Decision | One-line reason |
|---|---|---|---|
| Q1 | Content kinds as inherited/patchable blueprints vs new loader kinds | **Blueprints** — every listed kind (skills, abilities, statuses, items, quests, plus tuning) is an ordinary blueprint; no new loader kinds | The blueprint loader already gives inheritance, `replace`/`merge`/`remove` patches, sorted deterministic load and hot reload for free; a second loader kind duplicates all of it for no capability gain |
| Q2 | Declarative ability effects + scripts for exceptions, vs scripts-only | **Declarative parts first, scripts for exceptions** — data carries the numbers; a script runs only when the effect is beyond the part vocabulary | Scripts-only makes every numeric tweak a code change, defeating the tuning table and the sim sweeps; declarative data keeps the Shield Bash path testable without a script |
| Q3 | Additive/percent status modifiers vs an expression language | **Additive/percent fields** (`add`/`pct`, keyed by stat), no expression language | Two operators cover every designed status and gear modifier; an expression language is a second scripting system with its own determinism and validation burden |
| Q4 | Derived stats: eager recompute on change vs lazy reads | **Eager recompute at defined invalidation points, cached in a derived part** | Eager keeps reads trivially deterministic and inspectable (M7 inspector shows the live value); lazy makes every read a potential script call and hides invalidation bugs |

The rationale for each, plus the rejected alternatives, is in §5–§8. Everything else
(blabels, identity, tuning format, conventions, reload/save semantics) is decided here too;
§12 lists the small residual risks rather than open questions — **S0 must leave no open
schema questions** (roadmap: S0 is the decision gate for schema questions).

## 2. Proposal validation: are blueprints enough? (Q1, examined)

The task asks to *validate or refute*, not assume. The check:

- **Inheritance is needed and already works.** `Skill_Blade` inherits `Skill`;
  `Ability_ShieldBash` inherits `Ability`; `GoblinRaider` inherits `Humanoid`. The M2 loader
  resolves single-parent chains deterministically (parent→child, sorted ids). A separate
  loader would need to re-implement this for each new kind.
- **Patchability is needed and already works.** Mods tweak base abilities (`merge` one
  number in `Ability_ShieldBash`) and quest mods remove quest stages (`remove`). M5 patch
  ops operate on raw `toml::Value` before typed deserialization (m5 spec §4.2), so they work
  on *any* blueprint payload regardless of kind.
- **Identity/namespacing is the one real gap.** Blueprint ids today are bare strings
  (`"Goblin"`), global across the library. Content kinds multiply the collision surface
  (a mod adding `Skill_Blade` must not silently redefine base's). Resolution: keep one
  id space, but make ids **kind-prefixed by convention and validated against kind** —
  `Skill_*`, `Ability_*`, `Status_*`, `Item_*`, `Quest_*`, `Tuning_*`, plus the existing
  creature blueprint names. See §4.
- **The kind must be explicit data, not inferred from filename or prefix alone.** Each kind
  gets a small *kind part* (e.g. `parts.Ability = { … }`) registered in the existing
  `PartRegistry`; the loader then validates a kind-part's payload with the same
  `deny_unknown_fields` machinery every other part uses. Nothing new to register: kind parts
  are ordinary parts, and the convention is enforced by a **lightweight naming lint** in the
  loader (a blueprint that contains a kind part whose prefix does not match its id is a load
  error — `Ability_Foo` must carry `parts.Ability`, and `parts.Ability` requires an
  `Ability_`-prefixed id).

**Verdict: validated, with one addition** — namespacing conventions (§4) and kind parts.
No parallel loader kinds.

## 3. What a kind is: kind parts, not new loaders

Each content kind is defined by **one registered part** whose payload carries that kind's
typed data. The blueprint remains the unit of identity, inheritance and patching; the kind
part is the unit of validation and interpretation.

| Kind | Kind part | Carries |
|---|---|---|
| Tuning | `Tuning` | named integer fields (one table per area: `Tuning_Combat`, …) |
| Skill | `Skill` | name, family, hidden, `rank_cost` per rank, granted abilities per rank |
| Ability | `Ability` | name, kind (`active`/`passive`/`reactive`), target, tags, effects |
| Status | `Status` | name, duration, stacking policy, tags, modifier list |
| Item | `Item` | slot, weight, price; plus `Weapon`/`Armor` payload parts |
| Quest | `Quest` | stage list with event trigger expressions |

All ship as **native parts in `tessera-core`** (typed structs, serde, `deny_unknown_fields`):
they are schema, not rules, and S1–S3 read them natively. Effects *within* `Ability` /
`Status` are **data** (§7–§8); behavior beyond the effect vocabulary is a Luau script
referenced by the blueprint (`scripts = [...]`), which is the M4 mechanism unchanged.

Creatures (and the player) keep using plain parts (`Attributes`, `Skills`, `Equipment`,
…); they are consumers of these blueprints, not a new kind.

### 3.1 Declaration shape

One blueprint per content entity, in the campaign mod (or base for engine-level ones):

```toml
# data/mods/tessera-campaign/blueprints/abilities.toml
[Ability_ShieldBash]
inherits = "Ability"                       # abstract root, carries defaults + docs
parts.Ability = { name = "Shield Bash", kind = "active", target = "adjacent",
                  tags = ["melee", "stagger"] }
parts.Charges = { max = 3, recharge = 1200 }        # game-time units per charge
parts.Damage  = { lo = 3, hi = 6, dtype = "blunt", stat = "might", acc_bonus = 3 }
parts.Effect  = { applies = ["Status_Staggered"], script = "" }   # script "" = none
```

A blueprint with a kind part but no `inherits` from the kind's abstract root is allowed at
load (inheritance is optional) but flagged by the naming lint as a style warning; the lint
*errors* only on prefix/kind-part mismatch (§2).

### 3.2 Identity and namespacing

- Blueprint ids stay a **single global `BTreeMap` id space** (current loader). Rules:
  1. Ids are unique across all kinds (a `Skill_Blade` and `Ability_Blade` may coexist, but
     two `Skill_Blade` in the same mod set is the existing redefinition error).
  2. The campaign mod uses the prefixes above; base ships only engine-level content
     (`Actor`, `Humanoid`, the abstract roots `Skill`, `Ability`, `Status`, `Item`,
     `Quest`, `Tuning`, plus tiles/zones).
  3. Prefix/kind mismatch = load error (§2). No `:`-namespacing (`modid:thing`): the M5
     manifest already namespaces mods, and per-mod id prefixes would break patch ergonomics.
- Mod-defined content that collides with base ids *overrides* (last wins, M2 semantics) —
  that is the intended mod mechanism, and patches remain the gentler tool.

### 3.3 Inheritance and patch semantics

Unchanged from M2/M5, now applied to all kinds:

- Single-parent `inherits`, whole-part inheritance, deterministic resolution order.
- Kind parts inherit and merge like any part: a child `parts.Ability = { name = … }`
  overrides only the fields it sets (M5 `merge` semantics at resolution — the existing
  parent→child whole-part inheritance keeps child tables *replacing* parent tables per M2;
  kind parts that want field-level defaults must either repeat the field or the abstract
  root must keep defaults minimal. **Decision:** keep M2 whole-part replace semantics;
  abstract roots carry defaults for the *full* payload so children rarely need partial
  overrides).
- M5 patch ops (`replace`/`merge`/`remove`) work on every kind with no special cases, since
  they act before typed deserialization.
- A kind part's payload is validated at load time (kind parts are registered parts, so the
  loader's existing registry check plus serde covers them); scripts referenced by
  `scripts = [...]` are validated by the M4 header check.

## 4. Tuning tables (Q2 groundwork; the S1 contract)

A tuning table is a blueprint carrying `parts.Tuning` — typed, validated at load, readable
from Luau, overridable by sim sweeps without touching content.

```toml
# data/mods/tessera-campaign/blueprints/tuning-combat.toml
[Tuning_Combat]
parts.Tuning = { base_hit = 50, ac_factor = 2, ac_cap = 30, acc_per_agility = 2,
                 crit_threshold = 5, crit_percent = 150, speed_slope = 2,
                 speed_min = 50, speed_max = 140 }
```

- **Typed:** `parts.Tuning` is a registered native part whose payload is
  `BTreeMap<String, TuningValue>` where `TuningValue` is `int | pct (int, %) | intlist`.
  Unknown fields are a load error (serde); duplicate keys across *one* table are a load
  error; two tuning blueprints of the same id is the normal redefinition error.
- **Script access:** `engine.tuning("Tuning_Combat").base_hit` returns integers; missing
  key raises a script error (fail-closed, like `get_part` on an absent part).
- **Sweep/override layer:** overrides are *not* content. The sim harness (S2) and dev console
  (M7) pass `(Tuning_BlueprintId, key, value)` triples that are applied **after** load as a
  final validation-checked overlay: an override naming an unknown table/key is an error, an
  override with a wrong type is an error. Overriding never mutates the authored `data/`
  files and is fully recorded in run output (Combat Sim Spec §9: results include the
  override list so any number is reproducible).
- **No floats.** Tuning values are integers or percent-of-integer (applied with the existing
  integer arithmetic conventions; determinism rule: no floating point for gameplay
  outcomes).

## 5. Ability conventions (Q2)

An ability blueprint's *numbers* live in parts; its *behavior* is:

- **Declarative effects** for everything expressible by the effect vocabulary: a `Damage`
  part (a hit for `lo..hi` of `dtype`, scaled by `stat`, with `acc_bonus`), an `Effect`
  part (applies statuses, moves the target, spends a resource). The S0 vocabulary is the
  minimum that makes Shield Bash declarative end-to-end: `Damage`, `applies` statuses,
  and a `push` distance. S3 extends the vocabulary as Knight/Cleric/Wizard abilities
  demand — extending it is a schema change with a spec, not a silent one.
- **Scripts for exceptions:** any effect outside the vocabulary is a Luau script listed in
  `parts.Effect.script` or the blueprint's `scripts = [...]`, using the M4 surface
  (`engine.on`, `get_part`/`set_part`, `engine.rng(label)` forked streams). Scripts never
  *replace* the declarative parts; they *add* behavior, so the data stays the source of
  truth for costs, scaling and sweep knobs.

**Ownership split (respecting architecture §6):** native owns identity, resource accounting
(charges/cooldown decrement on use), status application and duration bookkeeping, and event
dispatch. Luau owns the *rules*: whether an ability may be used in context, unusual
per-hit behavior, and any effect the vocabulary lacks. A Shield Bash with no script must
resolve identically on every platform from data alone.

**Naming:** ability ids `Ability_*`; granted-by-rank wiring lives on the granting skill
(`parts.Ranks`), so abilities do not know their skills. `kind = active|passive|reactive` is
a flag in `parts.Ability` (Slice Readiness item 1: active slots out of slice, flag kept).
Scaling is `parts.Scaling = { per_rank = { damage = 2, recharge_pct = -8 } }` — integer
deltas/percent per rank above the granting rank, read by the ability rules script from the
owner's current rank (decided 2026-10-18 in the schema sketch; S3 implements the rank read).

## 6. Status conventions (Q3)

```toml
[Status_Staggered]
inherits = "Status"
parts.Status    = { name = "Staggered", duration = 100, stacking = "refresh",
                    tags = ["harmful", "control"] }
parts.Modifiers = { skip_turns = 1 }
```

- A status is a blueprint with `parts.Status` (identity, `duration` in game-time units,
  `stacking = "refresh" | "stack" | "independent"`, `tags`) and `parts.Modifiers` — the
  modifier list (Q3):
  - Each modifier is `{ stat = "<stat name>", add = <int>?, pct = <int>? }` (TOML shorthand
    `stat = <int>` means `add = <int>`; `stat_pct = <int>` means `pct`).
  - Stats are a fixed native enum of derived-stat names (S1 defines the list; S0 fixes the
    mechanism and reserves the names: `speed`, `accuracy`, `defense`, `damage_dealt`,
    `damage_taken`, `healing`, `block`, plus `skip_turns` as a control field, not a stat).
  - **Application order is fixed and total:** `add` in blueprint-defined order, then `pct`
    (integer math, truncation toward zero), then S1's clamps. There is no reordering rule
    to learn; the order is data-independent.
  - Duration: refreshed by reapplication of the same status id under `refresh`;
    `stack` adds duration to the existing instance; `independent` creates a separate
    instance (capped at a fixed 8 instances, first-expired-first-out — a deterministic cap,
    not an open knob).
- **No expression language.** Every designed modifier (Withered's heal/ward percentages,
  stagger, Chilled speed, block-chance changes) is `add`/`pct`. If a future status truly
  needs more, it takes a script (Q2's mechanism) — not a DSL.
- Statuses apply through a native `ApplyStatus` event; `StatusApplied`/`StatusExpired`
  fire at application/expiry (event names per the schema sketch's event list; payload and
  resolution are S3's).

## 7. Derived stats: eager recompute (Q4)

- Derived values (HP, Accuracy, Dodge, Defense, Speed — the S1 set) are computed by
  `rules/stats.luau` (Luau owns formulas, architecture boundary) **eagerly** at two
  invalidation points: (a) spawn from a blueprint, and (b) any event that the rules script
  declares dirtying (attribute change, equipment change, status applied/expired).
- The result is cached in a native dynamic part (`Derived`), written via the existing
  whitelisted `set_part` path. Reads (native or script) never recompute.
- Why eager: reads are pure data (deterministic, inspectable in the M7 inspector, cheap in
  hot paths); the dirtying event set is small and declared, so the cost is bounded; lazy
  would put a script call inside native hot loops — the exact thing architecture §6 forbids.
- S0 delivers the *mechanism* (a `Derived` part plus the invalidation hook and one trivial
  dirtying event) with Shield Bash as the consumer; the real stat set and formulas are S1.

## 8. Hot reload, determinism, saves, validation, compatibility

- **Hot reload.** The M2/M4/M5 watcher pattern extends to the new content directories:
  `tuning/` (if used as a directory), `blueprints/*.toml` (already watched — tuning and
  other kinds are blueprints, so *they reload for free*), plus patches. Statuses in flight
  on live entities keep their applied instance (duration/tags snapshotted at application);
  only *new* applications read the reloaded definition. Rebuilding the library on any error
  keeps the previous content wholesale (existing keep-old-on-error policy).
- **Determinism.** All kind parts are `BTreeMap` payloads; modifier application order is
  blueprint-declared (§6); tuning reads are pure lookups; status cap ordering is FIFO.
  No `HashMap` anywhere new (determinism.md rule 3; each sim crate carries the
  `clippy.toml` disallowed list).
- **Saves and versioning.** Applied statuses and charges are *persistent state* — they save
  through the existing `SaveHooks` (`StorageClass::Dynamic` parts serialize via
  postcard/TOML per M7) and must record the **content identity** they came from:
  `{ blueprint_id, kind, duration_remaining, charges_or_stacks, source_mod }`. The M7 save
  `SAVE_FORMAT_VERSION` bumps once when the first persistent kind part lands (S0 records
  the implication now per the roadmap: *save tags/version recorded before new persistent
  parts land*); the `Derived` part is `Rebuilt`-class — never saved, recomputed on load
  (its inputs save, the derived cache does not). Saves also record the mod list (M7
  already does) so a save references the content set that produced it; loading a save
  whose status blueprint ids are missing is a load error naming the id.
- **Validation and error behavior.** Fail-closed, consistent with M2/M5: unknown kind part
  = unknown part error; kind payload violations are serde errors with the file/id in the
  message; prefix/kind mismatch errors; tuning override errors (unknown key, wrong type)
  reject the run before simulation. Errors are collected per load pass and reported
  together where possible.
- **Compatibility.** No existing blueprint changes meaning: everything here is additive
  (new registered parts, new conventions). The only reserved word is the naming lint on
  `Tuning_/Skill_/Ability_/Status_/Item_/Quest_` prefixes — base data today uses none of
  them, so no migration. The `handlers` reserved key stays rejected (M3 owns its arrival).

## 9. Shield Bash end-to-end (the S0 proof)

**Goal:** one ability defined entirely in data runs in a headless test — the S0 acceptance
line in the roadmap — proving: kind parts load, tuning is script-readable, a declarative
effect resolves through events, a status applies, and a script-visible assertion holds.

Content (campaign mod, all data, no ability script):

```toml
# data/mods/tessera-campaign/blueprints/abilities.toml
[Ability_ShieldBash]
inherits = "Ability"
parts.Ability = { name = "Shield Bash", kind = "active", target = "adjacent",
                  tags = ["melee", "stagger"] }
parts.Charges = { max = 3, recharge = 1200 }
parts.Damage  = { lo = 3, hi = 6, dtype = "blunt", stat = "might", acc_bonus = 3 }
parts.Effect  = { applies = ["Status_Staggered"], push = 1, script = "" }
```

```toml
# data/mods/tessera-campaign/blueprints/statuses.toml
[Status_Staggered]
inherits = "Status"
parts.Status    = { name = "Staggered", duration = 100, stacking = "refresh",
                    tags = ["harmful", "control"] }
parts.Modifiers = { skip_turns = 1 }
```

Flow (all seams named):

1. **Load:** `mod.toml` (`tessera-campaign`, depends on `base`) → blueprints load sorted →
   kind parts validate against the registry → naming lint passes. *Seam: `BlueprintLibrary::load`
   with the campaign mod present (M5 wiring).*
2. **Spawn:** test spawns `Player` (with `Skill_ShieldCraft`-style ownership stub — for S0
   a bare `parts.Abilities = { list = ["Ability_ShieldBash"] }` part) and a `Goblin`
   adjacent to it. *Seam: `EntityBroker::spawn_with` from blueprint ids.*
3. **Use (native):** a test-triggered `UseAbility { ability, target }` event (S0's minimal
   ability event; S2/S3 build the targeting pipeline) hits the native resource path:
   charges 3→2. *Seam: event dispatch through the M3 bus; `Charges` decrement is native.*
4. **Resolve (declarative):** the `Damage` part draws `lo..hi` from
   `engine.rng("combat")` (forked stream — determinism rule), applies via the existing
   `Attack`→`TakeDamage` pipeline with the `blunt` dtype; the `Effect` part applies
   `Status_Staggered` (native `ApplyStatus`) and pushes 1 tile (native `Move`-path query).
   *Seams: `engine.rng` fork; M3 `TakeDamage`; native status store; `path` push query.*
5. **Assert (headless):** a Luau assertion script
   `data/mods/tessera-campaign/scripts/abilities/shield_bash_check.luau` registered on
   `ActorTurnStart` verifies: goblin HP dropped by 3–6 of blunt; goblin has
   `Status_Staggered`; attacker `Charges.charges == 2`; and a staggered actor skips its
   next turn (scheduler seam). *Seam: `engine.get_part`, `engine.on`, turn scheduler.*

**Expected demo/test mapping:**

| Artifact | What it proves |
|---|---|
| `data/mods/tessera-campaign/` (Shield Bash + Staggered + tuning table) | the content conventions load as a mod |
| `crates/tessera/tests/s0_shield_bash.rs` (headless integration test, seeded) | the full path: load → spawn → use → declarative resolve → status applied → charges spent → deterministic per seed |
| `--headless n` demo output | the event log shows `UseAbility`, `TakeDamage(blunt)`, `ApplyStatus(Status_Staggered)` in order |

**Acceptance criteria (all required):**
1. The campaign mod loads with the Shield Bash blueprint; a naming-lint violation
   (mismatched prefix) fails the load with a precise error.
2. `parts.Tuning` validates at load; `engine.tuning()` returns the value; an unknown-key
   override fails the run.
3. A seeded headless run exercises Shield Bash with **no ability script** and asserts the
   four conditions in step 5; the same seed reproduces identical output; the run passes on
   CI (Linux/macOS/Windows per M4's Luau requirement).
4. Applied status + charges survive a save/load round-trip with content identity recorded
   (the version-bump implication is *recorded* in this spec; the actual bump lands with S3's
   persistent status work if S0 keeps statuses ephemeral — decision: S0 persists nothing new;
   the save implication below is the recorded contract).

## 10. S0/S2 separation

S0 fixes: kinds, kind parts, tuning format and override mechanism, modifier mechanism,
`UseAbility` event name/payload, `Derived` mechanism, save-content-identity contract.
S2 owns: the hit pipeline, scenario/build/policy formats, bots, bands, sweeps driver UI.
Nothing in this spec decides combat resolution rules; where a seam is needed early
(`TakeDamage` payload fields), S0 only names the field and defers semantics to S2.

## 11. Builder decomposition (each slice ~300–500 changed lines, own tests/demo)

1. **Kind parts + naming lint** (~400 lines): register `Tuning`, `Skill`, `Ability`,
   `Status`, `Item`, `Quest` parts in `tessera-core`; prefix lint in the blueprint loader;
   error cases + loader unit tests.
2. **Tuning + override layer** (~350): `TuningValue` typing, `engine.tuning()` binding,
   override triples applied post-load with validation; unit + integration tests.
3. **Effects, status store, `Derived`, `UseAbility`** (~500): declarative effect dispatch
   (`Damage`/`applies`/`push`), native status store with stacking/duration, `Derived` part
   + invalidation hook, `UseAbility` resource path; unit tests per piece.
4. **Shield Bash vertical slice** (~350): campaign mod content, headless integration test
   (§9), `--headless` demo wiring, hot-reload extension test (edit a status duration while
   running → new applications use it).

Cards 1–2 are independent; 3 depends on 1; 4 depends on all.

## 12. Residual risks (not open questions)

- **Effect vocabulary growth.** S3 will need `cooldown`, wind-up, area vocabularies; the
  risk is vocabulary churn in `parts.Effect`. Mitigation: the vocabulary is versioned with
  the script API (M4's `TESSERA_API_VERSION`), each addition a small spec line.
- **Kind-part partial inheritance friction** (§3.3): children restating full payloads is
  mildly verbose but keeps M2 semantics untouched; revisit only if it visibly hurts.
- **Naming lint strictness.** The prefix/kind mismatch check is the only hard rule; mods
  with unusual ids must still work — the lint errors only on contradiction, never on style.
- **`skip_turns` vs speed scheduler interaction** is an S3/S2 detail (skip consumes the
  next scheduled action); the S0 contract is only that the modifier exists and is
  deterministic.

## 13. Governance

Bots never merge. This spec PR is authored as `jffjs-bots` on branch `wt/s0-schema`, review
requested from Jeff; merge and any overrides of the four decisions in §1 are Jeff's calls,
folded back into this file before builders start.
