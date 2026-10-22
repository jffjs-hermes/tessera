# S1 spec: stats and derived values

Status: research spec for S1 — Stats and derived values (author: researcher bot; pending
Jeff's approval). Grounded in [architecture.md](../architecture.md) §6 (native/script
boundary, "Core owns the primitives and invariants; scripts own the rules, policy and
tuning"), the roadmap S1 entry ([roadmap.md](../roadmap.md) § "Slice roadmap: S0–S11"),
the shipped S0 code and spec ([s0-schema.md](s0-schema.md) §4 tuning contract, §6 modifier
mechanism and reserved stat names, §7 eager `Derived` recompute, §8 saves/versioning), the
M2/M3/M4/M7 specs ([m2-entities.md](m2-entities.md), [m3-events-turns.md](m3-events-turns.md),
[m4-scripting.md](m4-scripting.md), [m7-saves-tooling.md](m7-saves-tooling.md)), and the
design notes `Design/Attributes and Levels.md`, `Design/Combat and Abilities.md`,
`Design/Character Progression.md`, `Specs/Combat Formulas.md`, `Specs/Slice Numbers.md`,
`Specs/Balance Summary.md`, `Specs/Slice Implementation Roadmap.md`, and **both design-side
prototypes read directly** — `Specs/prototype_formula_sim.py` (the integer-only formula
calibration: `hp`, `clamp`, `mod`, `hit_chance`, `attack`, `cost_gt`, and the `P` tuning
dict `base=50, ac_factor=2, ac_cap=30, speed_slope=2`) and
`Specs/prototype_class_sim.py` (the newer class-balance sim v3: the shared `hp_formula`,
the `K` formula-constants dict, and the `T` slice-tuning dict for all classes).

**Scope boundary:** S1 delivers the attribute/level model and the five derived stats
(Max HP, Accuracy, Dodge, Defense, Speed) with their exact formulas, eager recompute
pipeline, save treatment, and inspector visibility. S2 owns the hit/damage pipeline that
*consumes* Accuracy/Defense (hit roll, crit, block, resistance, threshold); where this spec
names a value S2 will read, it only fixes the cached number and its invalidation, not the
resolution rules. S4 owns XP/level-up *flow*; S1 fixes only the `Level` part as persistent
state. Equipment terms in the formulas are read from optional parts and are 0 until S2
lands gear.

---

## 1. Decisions at a glance (the five S1 questions)

| # | Question | Decision | One-line reason |
|---|---|---|---|
| Q1 | Five attributes + Level: native fixed parts vs blueprint/script parts | **Native fixed parts** — one `Attributes` part (five fixed fields) plus one `Level` part; attribute *names* are a fixed native enum, not data | Attributes are schema every formula depends on; a typo'd attribute name must be a load error, and native clamps/invariants need a concrete type — the same reasoning that made the S0 content kinds native parts (s0 §3) |
| Q2 | Formula set location and semantics | **`rules/stats.luau` implements the formulas verbatim from `Specs/Combat Formulas.md` over `Tuning_Combat`**; integer-only, Luau floor division `//`, clamps exactly where the design clamps | Luau owns rules/policy per architecture §6; the tuning table (s0 §4) is the number source; the acceptance examples must reproduce bit-for-bit, so the spec pins the arithmetic, not just the intent |
| Q3 | Save schema for stats | **`Attributes` and `Level` are whitelisted persistent parts; `Derived` is never saved and is rebuilt on load; `SAVE_FORMAT_VERSION` bumps 1 → 2 in this milestone** | s0 §8 records the contract ("save tags/version recorded before new persistent parts land"; Derived is Rebuilt-class); the S1 bump is the *first* bump and also covers removing the legacy `Stats` part from the whitelist |
| Q4 | Inspector/console visibility | **Read-only: the egui inspector row gains Attributes/Level/Derived in the summary line and the generic native-part pane keeps showing live `Derived` values; the console gains a read-only `stats <id>` command; no write path for Derived is added** | The M7 inspector already renders every native dynamic part via the save-hook snapshot; Derived is native, so live values appear for free — the work is surfacing the summary and asserting it in tests |
| Q5 | Where `stats.luau` runs | **A single rules script registered once by the engine (not per-blueprint), invoked eagerly on spawn-after-stamp and on a declared dirtying-event set; every recompute writes the cached `Derived` part, and native hot paths read only the cache** | Per-blueprint registration would recompute N times per dirtying event and silently miss entities; a script call inside a native hot loop is exactly what architecture §6 forbids (s0 §7) |

Carried forward from S0 **without reopening**: typed tuning blueprint (`int | pct |
intlist`, no floats) with `engine.tuning` access and validated overrides (s0 §4); modifier
mechanism `add` then `pct` with fixed order (s0 §6); fixed native derived-stat enum with
the reserved names (s0 §6); eager `Derived` recompute at spawn and declared dirtying
events, cached in the native `Derived` part, reads never recompute (s0 §7); native owns
integer arithmetic, clamping and invariants, scripts own formulas and tuning policy
(architecture §6).

## 2. Attributes and Level as typed parts (Q1)

### 2.1 The parts

Two new **native** dynamic parts in `tessera-core` (serde, `deny_unknown_fields`, registered
via `kinds::register_all` like every S0 part):

```toml
# data/base/blueprints/creatures.toml (Player example)
[Player]
inherits = "Humanoid"
parts.Attributes = { might = 14, agility = 9, toughness = 13, will = 8, intellect = 11 }
parts.Level      = { level = 1 }
```

```rust
// crates/tessera-core/src/stats.rs (new module)
pub const PART_ATTRIBUTES: &str = "Attributes";
pub const PART_LEVEL: &str = "Level";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attributes {
    pub might: i64,       // clamped to [1, 30] at deserialize
    pub agility: i64,
    pub toughness: i64,
    pub will: i64,
    pub intellect: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Level {
    pub level: i64,       // clamped to [1, 20] at deserialize
}
```

- **Field names are the fixed attribute enum.** `AttributeName { Might, Agility, Toughness,
  Will, Intellect }` is a native enum with `as_str`/`from_str`; it validates `Bonuses` keys
  (§4.3) and any future per-attribute data. A misspelled attribute field is already a serde
  load error via `deny_unknown_fields`; the enum exists for code that iterates attributes
  deterministically (BTreeMap order: Agility, Intellect, Might, Toughness, Will).
- **Bounds are load-time invariants** (native owns invariants): attributes deserialize only
  in `[1, 30]` (design hard cap 30, `Attributes and Levels.md` §Levels), `level` only in
  `[1, 20]` (slice cap; the *design* cap is 20 and S4's XP flow enforces the same bound).
  Out-of-range values are serde load errors with file/id in the message, same failure
  style as every S0 part.
- **Why two parts, not one:** M2 inheritance/patch semantics are whole-part replace
  (s0 §3.3). A separate `Level` part lets a mod or a future S4 XP event change level
  without restating five attributes, and lets S4 extend level bookkeeping (XP) without
  touching the attribute payload. Two small parts cost nothing and keep patch ergonomics.
- **Why not blueprint parts or script parts:** the attributes are consumed by every
  formula and by native invariant checks; a script part would make native reads depend on
  script-registered schemas, and five separate one-field blueprint parts would multiply the
  naming-lint surface for zero capability. This mirrors s0 §3's verdict for content kinds:
  schema lives in native typed parts.

### 2.2 The legacy `Stats` part is removed (compatibility decision)

`events.rs` today ships `parts.Stats = { strength = 18 }` on the base Goblin, whitelisted
in `save.rs::build_save_hooks`. Keeping it beside `Attributes` would create two sources of
truth for the same idea. **Decision: `Stats` is deleted in S1** — removed from
`events.rs`, from `data/base/blueprints/creatures.toml`, from the save whitelist, and from
the sample-mod patch that merges `parts.Stats.strength`. Nothing in the slice reads
`strength` (the M6 goblin AI does not use it; `GoblinBrain.detect` is the AI input). This
is the change that forces the Q3 version bump; it is called out here so it is not a silent
removal. The generic `stats` word stays reserved for future engine use but no part carries
it after S1.

### 2.3 Who carries Attributes

`Actor` (the abstract root, s0 §3.2) gains `parts.Attributes` and `parts.Level` defaults
in base data (`data/base/blueprints/creatures.toml`), so every creature inherits them;
blueprints override per creature/class. Entities **without** `Attributes` (none after this
change, but mods may spawn bare parts) keep the legacy behavior exactly: no `Derived`
part, scheduler reads `ActorEnergy.speed` as today (§5.4).

## 3. The derived-stat enum and the Derived part (carried from S0, fixed here)

s0 §6 reserved the derived-stat names; S1 fixes the full list as a **native enum**:

```rust
pub enum DerivedStat { MaxHp, Accuracy, Dodge, Ac, Defense, Speed,
                       DamageDealt, DamageTaken, Healing, Block }
// as_str: "max_hp", "accuracy", "dodge", "ac", "defense", "speed",
//         "damage_dealt", "damage_taken", "healing", "block"
```

- `max_hp`, `accuracy`, `dodge`, `ac`, `defense`, `speed` are S1's computed set (§4).
  `damage_dealt`, `damage_taken`, `healing`, `block` stay reserved-and-uncomputed until
  S2/S3/S5 populate them; the enum accepts them as modifier targets today (s0 §6 already
  promised that) and `Derived` may not carry them until their owners land.
- The `Derived` part payload stays `BTreeMap<String, i64>` (scripts write through the
  generic whitelisted `set_part` path), but **deserialization now validates every key
  against `DerivedStat`** — an unknown stat key is a load/restore error, fail-closed. This
  keeps the script write path unchanged while making the cache typed at the boundary.
- `skip_turns` remains a control field in `Modifiers`, never a `Derived` key (s0 §6).

## 4. The formula set (Q2)

`rules/stats.luau` (shipped at `data/base/scripts/rules/stats.luau`, §5.1) implements
exactly these functions. Every constant below is a `Tuning_Combat` key (s0 §4: values must
not be scattered constants); every operation is integer; division is Luau floor division
`//` (floor for negatives, per `Combat Formulas.md` §Conventions: "multiplication then
integer division (floor)… floor also for negatives"). **No floats anywhere.**

### 4.1 Tuning table (base ships it)

`data/base/blueprints/tuning-combat.toml` moves into **base** (the stats rules are
engine-level content; base content is a mod, architecture §5). The campaign mod's existing
`tuning-combat.toml` is updated to the same full key set (M2 whole-part replace means a
partial override would *drop* base keys, s0 §3.3 — the campaign file restates every key
plus its own `goblin_hp`/`bash_lo`/`bash_hi`). Keys and authored defaults:

| Key | Value | Formula use |
|---|---|---|
| `hp_base` | 10 | Max HP constant term |
| `hp_per_toughness` | 2 | Max HP Toughness term |
| `hp_level_base` | 3 | Max HP per-level base |
| `hp_toughness_divisor` | 3 | Max HP per-level Toughness share |
| `acc_per_agility` | 2 | Accuracy and Dodge per-Agility slope |
| `level_bonus` | 1 | Accuracy/Dodge per-Level coefficient |
| `base_hit` | 50 | Hit chance base (S2 consumes; cached here for completeness) |
| `hit_min` / `hit_max` | 5 / 95 | Hit chance clamp (S2) |
| `ac_factor` | 2 | Defense AC term |
| `ac_cap` | 30 | Defense AC cap |
| `speed_slope` | 2 | Speed per-Agility slope |
| `speed_min` / `speed_max` | 50 / 140 | Speed base clamp |
| `untrained_penalty` | 15 | (reserved, S2 weapon handling) |

Missing key ⇒ the script errors (fail-closed, s0 §4 `engine.tuning` contract); an
unknown-key override is already a `TuningError` (s0 §4).

### 4.2 The formulas (verbatim from `Specs/Combat Formulas.md` §Derived stats)

```lua
-- rules/stats.luau — the five derived stats. Integer-only, floor division.
local T = engine.tuning("Tuning_Combat")

local function mod(x)                 -- attribute modifier, floor for negatives
  return (x - 10) // 2
end

local function max_hp(attrs, level)
  local toughness = attrs.toughness
  return T.hp_base + T.hp_per_toughness * toughness
       + (level - 1) * (T.hp_level_base + toughness // T.hp_toughness_divisor)
end

local function accuracy(attrs, level, bonuses)   -- bonuses = parts.Bonuses (§4.3)
  return T.level_bonus * level
       + T.acc_per_agility * (attrs.agility - 10)
       + bonuses.accuracy                                   -- weapon/skill/gear; 0 until S2
end

local function dodge(attrs, level, bonuses)
  return T.level_bonus * level
       + T.acc_per_agility * (attrs.agility - 10)
       + bonuses.dodge                                      -- Evasion/gear; 0 until S2
end

local function ac(bonuses)                            -- armor + shield AC; 0 until S2
  return bonuses.ac
end

local function defense(dodge_v, ac_v)
  return dodge_v + math.min(ac_v * T.ac_factor, T.ac_cap)
end

local function speed_base(attrs)
  local raw = 100 + T.speed_slope * (attrs.agility - 10)
  if raw < T.speed_min then return T.speed_min end
  if raw > T.speed_max then return T.speed_max end
  return raw
end
```

- The `100` in `speed_base` is the design's fixed base (`Combat Formulas.md` §Decisions:
  "`Speed = 100 + 2 x (Agility - 10)`"); it is a literal in the formula, not a tuning key,
  matching the design text. (If Jeff prefers it keyed, it is a one-line change.)
- **Attribute modifier `mod(x)`** stays native-free: it is used by *damage* (S2), not by
  the five S1 stats; it is defined here because it is the formula doc's shared convention.
- **Healing/Will formulas (`Mend`, wards) are out of S1 scope** — `healing` stays a
  reserved, uncomputed stat; S5 lands the heal/ward rules with the Cleric.

### 4.3 The `Bonuses` part (the S1 hook for S2/S3 terms)

Accuracy/Dodge/AC include "weapon accuracy + skill and gear bonuses" and "Evasion and gear
bonuses" (`Combat Formulas.md` §Derived stats). Those sources do not exist until S2/S3,
but the acceptance examples need them *now* (the Knight's `+3` is Blade I, the goblin's
AC 2 is leather armor). S1 introduces one additive native part that S2 gear and S3 skills
will populate:

```toml
parts.Bonuses = { accuracy = 3 }        # e.g. Blade I on the Knight fixture
parts.Bonuses = { ac = 2 }              # e.g. goblin leather, standing in for S2 armor
```

```rust
pub const PART_BONUSES: &str = "Bonuses";
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bonuses {
    pub accuracy: i64, pub dodge: i64, pub ac: i64,
    pub damage_flat: i64, pub block: i64,     // consumed by S2/S3; carried here so the
                                              // modifier vocabulary is stable from S1
}
```

Why a flat-bonus part and not statuses: statuses are *temporary, named, save-relevant*
state (s0 §6); weapon/skill/gear contributions are *persistent build state* that changes
only on equip/learn. One part keyed by the same stat names keeps S2 from inventing a
second mechanism. Status *penalties* on these stats (design: "− status penalties") flow
through the S0 `Modifiers` mechanism instead (§4.5).

### 4.4 Integer and clamp semantics (exact)

- All arithmetic is `i64`-equivalent integers; Luau `//` (floor) is the only division.
  Percent steps *inside the formulas*: none in S1 (the pct steps that exist — crit,
  resistance, overload — are S2). The S0 modifier `pct` step keeps its own S0-defined
  truncation-toward-zero arithmetic (s0 §6, not reopened); the formulas' own floor
  division matches `Combat Formulas.md`. The only place the two conventions could meet is
  a negative base under a `pct` modifier; S1 clamps prevent that for the five stats
  (speed ≥ `speed_min` before modifiers, HP ≥ 0), and §4.5 records the boundary.
- **Clamps live exactly where the design puts them:** the speed *base* clamps to
  `[speed_min, speed_max]` (§4.2); hit chance clamps to `[5, 95]` inside S2's resolution
  (the value `base_hit` feeds). Post-modifier, the **invariant clamps** (this is what
  "then S1's clamps" means concretely, s0 §6): `max_hp ≥ 1`, `speed ≥ 20`, `accuracy ≥ 0`,
  `dodge ≥ 0`, `defense ≥ 0`, `ac ≥ 0`. The speed floor is **20**, matching
  `prototype_class_sim.py`'s post-status `max(sp, 20)` — the newer calibration's floor is
  the number to match, and 1 would let a degenerate modifier stack produce a scheduler
  stall. A status that would push speed to 140+ (haste) is *not* re-clamped to the
  Agility band — the design applies statuses after the base clamp (`Combat Formulas.md`
  §Time and the scheduler), and haste/slow are the scheduler's tactical currency
  (`Combat and Abilities.md` §Implications).

### 4.5 Modifier pipeline (the S0 order, made concrete)

For each derived stat, the final cached value is:

1. **Formula base** (§4.2) — Luau, from `Attributes`/`Level`/`Bonuses` + `Tuning_Combat`.
2. **Applied statuses' `Modifiers` for that stat**, in the S0 fixed order: every `add`
   (sorted-key order across live instances), then every `pct` (native `Modifiers::apply`,
   s0 §6). Statuses carry snapshot modifiers per instance (S0 `StatusInstance`).
3. **Invariant clamps** (§4.4).

`Speed` example: Agility 20 → base 120; a Chilled stack (`speed_pct = 90`) → 108; two
stacks → 97. Frozen ("cannot act") is a control concern for S3, not a speed value.

### 4.6 Acceptance examples (must reproduce exactly)

These are the headless test vectors (from `Combat Formulas.md` §Worked examples and
`Attributes and Levels.md` §Levels HP table). `Bonuses` stands in for the not-yet-existing
gear/skill sources, exactly as flagged in §4.3:

| Example | Inputs | Expected |
|---|---|---|
| HP table, Knight | T 13, L 1 / 10 / 20 | **36 / 99 / 169** |
| HP table, Cleric | T 12, L 1 / 10 / 20 | **34 / 97 / 167** |
| HP table, Rogue | T 9, L 1 / 10 / 20 | **28 / 82 / 142** |
| HP table, Wizard | T 10, L 1 / 10 / 20 | **30 / 84 / 144** |
| Knight accuracy | L 3, Agility 9, `Bonuses.accuracy = 3` | `3 + 2*(9−10) + 3` = **4** |
| Goblin defense | L 2, Agility 13, `Bonuses.ac = 2` | Dodge `2 + 2*(13−10)` = 8; Defense `8 + min(2*2, 30)` = **12** |
| Hit% Knight→goblin | acc 4 vs def 12 | `clamp(50 + 4 − 12, 5, 95)` = **42** |
| Goblin accuracy | L 2, Agility 13 (+6 from the example's shorthand) | **8** |
| Knight defense | L 3, Agility 9 (`3 − 2` = 1), `Bonuses.ac = 10` | `1 + min(20, 30)` = **21** |
| Hit% goblin→Knight | acc 8 vs def 21 | `clamp(50 + 8 − 21, 5, 95)` = **37** |
| Wizard defense | L 3, Agility 11 | Dodge `3 + 2*(11−10)` = 5; AC 0 ⇒ Defense **5** |
| Hit% goblin→Wizard | acc 8 vs def 5 | **53** |
| Rogue accuracy | L 3, Agility 15, `Bonuses.accuracy = 3` | `3 + 2*(15−10) + 3` = **16** ⇒ hit% **54** |
| Speed table | Agility 8 / 10 / 15 / 20 / 25 | **96 / 100 / 110 / 120 / 130** (slope 2, clamp 50–140) |
| Speed + status | Agility 20 base 120, `speed_pct = 90` (Chilled) | **108** |

The hit% rows compute through the cached `accuracy`/`defense` values with the S0-scope
`base_hit`/`hit_min`/`hit_max` keys; the d100 roll itself is S2 and is *not* part of the
S1 acceptance (the roadmap S1 harness line is "formula fixtures reproducing HP, hit chance,
and speed examples" — the *chance*, not the roll).

### 4.7 Prototype cross-check (both `.py` sources, read directly)

Every S1 constant maps onto **both** prototypes; the two agree on all shared values, so a
single tuning table serves both:

| `Tuning_Combat` key | `prototype_formula_sim.py` | `prototype_class_sim.py` |
|---|---|---|
| `hp_base` / `hp_per_toughness` / `hp_level_base` / `hp_toughness_divisor` | `hp(T,L) = 10+2*T+(L-1)*(3+T//3)` | `hp_formula(T,L)` — **identical expression** |
| `acc_per_agility` | (inline `2*(A-10)`) | `K["acc_per_agi"] = 2` |
| `level_bonus` | (inline `s.L + …`) | (inline `s.L + …`) — both add Level with coefficient 1 |
| `base_hit` | `P["base"] = 50` | `K["base"] = 50` |
| `ac_factor` / `ac_cap` | `P` = 2 / 30 | `K` = 2 / 30 |
| `speed_slope` / `speed_min` / `speed_max` | `P["speed_slope"]=2`, clamp 50/140 | `K["speed_slope"]=2`, `clamp(100+K*…,50,140)` |
| `untrained_penalty` | (inline −15 in the Wizard fixture) | `K["untrained"] = 15` |
| (S2) `crit_threshold` / `crit_percent` | (inline `min(5,hit)`, `*150//100`) | `K["crit_thr"]=5`, `K["crit_pct"]=150` |
| (S5) `heal_will_percent` | (inline `100 + 3*(Will-10)`) | `K["heal_will_pct"] = 3` |

Two prototype details S1 records but does not adopt (they belong to later slices):
`prototype_class_sim.py` hardcodes the Dodge slope as `2*(s.A-10)` rather than reading
`acc_per_agi` (same value today; S1 reads the key for both, so a future retune cannot
split accuracy from dodge silently), and its magic accuracy uses Will
(`s.W if magic else s.A`) — a spell-resolution concern for S3/S5, noted so it is not
rediscovered. The `mod(x) = (x-10)//2` floor-division convention is identical in both
files and is the §4.2 `mod` definition.

## 5. The recompute pipeline (Q5)

### 5.1 The rules script and its registration

- `data/base/scripts/rules/stats.luau` is **base content** (the formulas are game rules;
  a total-conversion mod may ship its own by blueprint override of the script path — same
  mechanism as any content override).
- It is registered **once by the engine**, not per blueprint: at sim install time
  (beside `install_native_effects`, `crates/tessera/src/sim.rs`), the native installer
  looks up the well-known script path `rules/stats.luau` in the loaded library (sorted
  load order, first pack providing it wins — M5 semantics) and registers it as the handler
  for the `RecomputeDerived` event (§5.2). This is the same shape as `install_native_effects`
  owning event wiring natively; the *handler* is Luau because formulas are rules
  (architecture §6).
- **Fail-closed spawn:** spawning a blueprint that carries `parts.Attributes` when no
  stats rules script is registered is a spawn error (a stat-ful entity without a formula
  owner is an unrepresentable state; silent fallback would hide a broken content set).
  Entities without `Attributes` spawn fine and keep legacy behavior (§2.3).

### 5.2 Events and ordering

One new native event:

```rust
pub struct RecomputeDerived { pub entity: Entity }   // NORMAL priority
```

**Declared dirtying-event set for S1** (each fires `RecomputeDerived` for the affected
entity *after* its own handler completes, so the recomputation sees settled inputs):

| Dirtying event | Wired where |
|---|---|
| Spawn completion (post-`stamp_blueprint`) for an entity with `Attributes` | `World::spawn_blueprint` / `spawn_blueprint_at` success path |
| `StatusApplied` / `StatusExpired` | the existing S0 observers (`effects.rs::install_native_effects`) — extended: they currently only `invalidate_derived`; they now also dispatch `RecomputeDerived` |
| Write to `Attributes` or `Level` — native or via the script `set_part` pending-action path | the pending-action applier (`ScriptHost` action application) and the console's part-write path dispatch `RecomputeDerived` when the written part name is `Attributes`/`Level` |
| Content reload of a `Tuning_*` table | the hot-reload rebuild path dispatches `RecomputeDerived` for every live entity carrying `Attributes` (entity-id order, deterministic) |

Equipment change is S2's addition to this table (declared here so S2 extends, not
reinvents). **Ordering within one dispatch cycle** is fixed:

1. The dirtying event's own handlers run (e.g. `ApplyStatus` resolves, `StatusApplied`
   fires as its final step).
2. `RecomputeDerived` dispatches immediately after the dirtying dispatch returns
   `Continue` (cancelled dirtying events — e.g. an out-of-charges `UseAbility` — dispatch
   nothing).
3. The `stats.luau` handler reads the snapshot (`Attributes`, `Level`, `Bonuses`,
   `StatusEffects` skip-turn/modifier surface, `Tuning_Combat`), computes the five values,
   and writes them via `engine.set_part(entity, "Derived", {...})`.
4. The pending `set_part` applies when the `RecomputeDerived` dispatch returns — i.e. the
   cache is consistent **before control returns to the scheduler/next event**, which is
   what makes native reads safe (§5.4).
5. The native `invalidate_derived` hook (s0 §7) runs *first* in step 2's handler (clear
   then recompute), so a failed recompute leaves an absent cache rather than a stale one —
   absent is detectable, stale is a lie.

Repeat dirtying within one cycle (two statuses applied in one ability resolution)
recomputes twice; recomputation is pure and idempotent, and the cost is bounded by the
declared event set (s0 §7's argument, unchanged). No RNG is consumed by any of this.

### 5.3 HP interaction with the existing `Health` part

`Health { hp }` stays the **current-HP** store (S2's damage pipeline decrements it; the
console `sethp` debug command keeps working). `Derived.max_hp` is the cap. The native
recompute seam enforces one invariant after the script writes `Derived`: `Health.hp` is
clamped to `min(hp, max_hp)` (a shrink in max — a level-drain or tuning sweep — cannot
leave hp above max). Native owns the invariant; the script never touches `Health`. A
future S4 level-up heal policy composes on top.

### 5.4 Native hot paths read the cache, never call scripts

- The scheduler reads speed through one native helper:
  `derived_or_energy_speed(ctx, entity)` → `Derived.speed` when the entity carries
  `Derived`, else `ActorEnergy.speed` (legacy entities, §2.3). `ActorEnergy.speed` is no
  longer written by anything in S1; for stat-ful entities `Derived.speed` is the single
  truth and the `ActorEnergy` field is dead weight that S3's scheduler work may drop.
- S2's hit resolution will read `Derived.accuracy` / `Derived.defense` the same way.
- **No native code calls into Luau on a read path.** The only script invocation in the
  stats subsystem is the `RecomputeDerived` handler at the declared invalidation points.

## 6. Saves and versioning (Q3)

- **Whitelisted, persistent:** `Attributes`, `Level`, `Bonuses` gain `SaveHooks`
  snapshot/restore pairs in `save.rs::build_save_hooks` (canonical TOML text per the
  existing `(name, text)` `SavedEntity` format; restore fails closed on malformed
  payloads like every hook).
- **`Derived` is never serialized** (s0 §8: Rebuilt-class). Two mechanisms, both used:
  (a) `Derived` gets **no save hook**, and `snapshot_entities` gains an explicit
  skip-list containing `Derived` so a present cache is silently excluded rather than
  failing the save with `UnserializablePart` (the cache is legitimately not state);
  (b) `restore_broker` never produces a `Derived` part, and the load path dispatches
  `RecomputeDerived` for every restored entity carrying `Attributes`, in entity-id order,
  after the broker is rebuilt — so a loaded world has live derived values before the next
  scheduled action, and a loaded-but-unrecomputable entity (rules script missing) is a
  **load error**, not a silently slow goblin.
- **`SAVE_FORMAT_VERSION` bumps 1 → 2.** Per s0 §8 the bump lands "when the first
  persistent kind part lands" — that is this milestone (Attributes/Level/Bonuses are the
  first new persistent parts, and §2.2 removes `Stats` from the whitelist, so a v1 save
  would fail closed on the removed part anyway). Policy recorded for future bumps: one
  bump per milestone that adds or removes whitelisted parts, in the milestone that lands
  them; a v1 save now loads as `UnsupportedVersion { found: 1, supported: 2 }` — accepted
  pre-release (no shipped saves exist), named here so the behavior is a decision, not an
  accident.
- Applied statuses remain non-persistent in S1 (S3 owns status persistence per s0 §8);
  their effect on `Derived` is therefore not save-relevant yet.

## 7. Inspector and console visibility (Q4)

- **Inspector (egui, M7):** the row summary line gains `Might/Agility/Tough/Will/Intel`,
  `Lv`, and the five derived values (`HP 36/99  Acc 4  Dodge 8  Def 21  Spd 106` style),
  read from the live `Attributes`/`Level`/`Derived` parts. The detail pane's generic
  native-part listing already renders `Derived` through the save-hook snapshot path
  (`inspector.rs::native_part_debug`); a test asserts the snapshot contains the live
  computed values after a status application (proving "live native Derived", not a stale
  copy). **No write path is exposed** — the inspector remains read-only by construction
  (it snapshots; it never holds a broker borrow).
- **Console:** one read-only command, `stats <entity-id|player>`, printing the same five
  values plus the contributing terms (attribute, level, bonuses, each live status's
  modifier contribution) so a tuning session can see *why* a number is what it is. No
  `setderived` command: the cache has exactly one writer (the rules script) and the
  console must not become a second one (invariant ownership, §5.3).

## 8. Hot reload, determinism, compatibility

- **Hot reload:** `Tuning_Combat` and `rules/stats.luau` are ordinary content — the M2/M4/M5
  watchers already cover them (s0 §8). A tuning-table reload dirties every stat-ful entity
  (§5.2); a *script* reload re-registers the handler and recomputes all stat-ful entities
  in entity-id order. Recompute-on-reload keeps inspector values honest without a save/load
  round-trip.
- **Determinism:** no new containers beyond `BTreeMap`s; recompute consumes no RNG; the
  declared dirtying set is fixed; iteration orders are entity-id then sorted-key
  (determinism.md rule 3). The formulas are pure functions of cached inputs, so the same
  inputs always yield the same `Derived` — a replay that diverges in derived stats is a
  bug, and the fixture tests assert exact equality, not bands.
- **Compatibility:** additive parts (`Attributes`, `Level`, `Bonuses`), one removed part
  (`Stats`, §2.2 — the only removal, called out in the PR description), one moved tuning
  table (campaign → base, with the campaign file updated to restate the full key set), one
  version bump. Existing behavior is unchanged for entities without `Attributes`; the base
  creatures gain attributes in the same PR, so the demo goblin/player visibly run the new
  pipeline. No naming-lint interaction (no new `Kind_`-prefixed ids; parts are plain part
  names like `Charges`).

## 9. Tests and demo mapping (acceptance)

| Artifact | What it proves |
|---|---|
| `crates/tessera-core/tests/…` unit tests: part bounds, enum validation, `Bonuses` defaults | Q1/Q3 load-time invariants; unknown attribute/derived key fails closed |
| `crates/tessera/tests/s1_stats_formulas.rs` (headless, seeded) | §4.6 table reproduced exactly: 16 HP values, 4 accuracy/defense/hit% chains, 5 speed points, one status-modified speed; same seed ⇒ byte-identical `Derived` |
| `crates/tessera/tests/s1_stats_pipeline.rs` | recompute fires on spawn, `StatusApplied`/`StatusExpired`, attribute write via script `set_part`, tuning hot-reload; HP clamp invariant (§5.3); fail-closed spawn without the rules script; cache absent-after-invalidate (s0 §7 behavior preserved) |
| `crates/tessera/tests/s1_save_roundtrip.rs` | save → load ⇒ identical `Attributes`/`Level`/`Bonuses`, no `Derived` in the snapshot, derived values rebuilt and equal pre-save; v1 save rejected as `UnsupportedVersion` |
| `crates/tessera/src/devtools` tests | inspector snapshot shows live `Derived` after a dirtying event (§7) |
| `--headless` demo run | the event log shows `RecomputeDerived` after spawn and after the Shield Bash `Status_Staggered` application; the goblin's speed reflects its Agility (no longer a flat 100) |

**Acceptance criteria (all required):**
1. The §4.6 vectors reproduce **exactly** in headless tests on all three CI platforms
   (Luau per M4).
2. A seeded headless run's `Derived` values are identical across two runs of the same
   seed, and a save/load round-trip restores identical derived values.
3. The inspector shows live derived values for the player and a goblin in the running
   build; the console `stats` command prints the contributing terms.
4. `SAVE_FORMAT_VERSION` is 2; a v1 save fails with the named version error.
5. No native read path calls Luau (asserted by the pipeline test: scheduler ticks with the
   script handler unregistered still read the cached speed).

## 10. S1/S2 separation

S1 fixes: the stat parts, the derived enum, the five formulas and their tuning keys, the
recompute pipeline and its declared dirtying set, save treatment, inspector surface.
S2 owns: the hit/damage pipeline consuming `accuracy`/`defense`/`damage_dealt`/…,
equipment parts that *populate* `Bonuses`, the armor-overload speed term (it extends the
speed formula's post-base steps — declared here so S2 amends §4.2 in a spec line, not
silently), and the scenario/bot harness.

## 11. Builder decomposition (each slice ~300–500 changed lines, own tests/demo)

1. **Stat parts + save/version** (~350): `Attributes`/`Level`/`Bonuses` parts + enum,
   bounds, registry; save hooks; `Stats` removal from base data and whitelist;
   `SAVE_FORMAT_VERSION` 2; unit tests.
2. **Recompute pipeline + rules script** (~450): `RecomputeDerived` event, dirtying-event
   wiring, `Derived` key validation, `rules/stats.luau` + `Tuning_Combat` base table,
   scheduler speed helper, HP clamp invariant; unit + integration tests.
3. **Fixtures, inspector, demo** (~350): the §4.6 formula fixtures, save round-trip test,
   inspector summary + console `stats` command, demo/headless wiring, campaign tuning-file
   update.

Cards 1 and 2 are independent; 3 depends on both.

## 12. Residual risks (not open questions)

- **Floor vs truncation boundary.** The formulas use Luau floor division; the S0 modifier
  mechanism truncates toward zero. They meet only on a negative base under a `pct`
  modifier, which the invariant clamps (§4.4) make unreachable for the five S1 stats; if
  S2/S3 add a stat where that can occur, the owner decides then — recorded, not open.
- **`ActorEnergy.speed` dead weight.** Stat-ful entities stop using it; removing the field
  entirely touches saves and the scheduler, so S1 leaves it as the legacy fallback and S3's
  scheduler work owns the cleanup.
- **Whole-part replace vs the tuning table.** The campaign's tuning file must restate all
  base keys (§4.1); a future partial-merge convenience is M5 territory, not S1's.
- **Formula drift.** The design docs mark all constants placeholder; the fixture tests pin
  *today's* numbers, and a tuning change that breaks a fixture is a deliberate, reviewed
  change (that is the point of the fixtures).

## 13. Governance

Bots never merge. This spec PR is authored as `jffjs-bots` on branch `wt/s1-stats`, review
requested from Jeff; merge and any overrides of the five decisions in §1 are Jeff's calls,
folded back into this file before builders start.
