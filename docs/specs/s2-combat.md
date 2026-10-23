# S2 spec: melee combat, gear, Knight, and sim phase 1

Status: research spec for S2 — Melee combat, gear, Knight (sim phase 1) (author:
researcher bot; pending Jeff's approval). Grounded in [architecture.md](../architecture.md)
§6 (native/script boundary), the roadmap S2 entry ([roadmap.md](../roadmap.md) § "Slice
roadmap: S0–S11"), the shipped S0/S1 specs and code ([s0-schema.md](s0-schema.md) §§4/6/7,
[s1-stats.md](s1-stats.md)), and the current `crates/tessera-core` (`events.rs` —
`Attack`/`TakeDamage`/`Death` and the `install_minimal_combat` path it replaces,
`effects.rs` — declarative `Damage`/`Effect`/charges and the `Derived` cache,
`stats.rs` — the `DerivedStat` enum and reserved names, `save.rs` — whitelist +
`SAVE_FORMAT_VERSION`, `turns.rs` — the energy scheduler, `rng.rs` — `SimRng::fork`),
`crates/tessera/src/sim.rs` (headless runner, default handlers, the native
`TakeDamage`→`Health` mutator) and `crates/tessera/src/scripting.rs` (the whitelisted
`set_part` surface). Design sources, read directly from the vault: `Specs/Combat
Formulas.md`, `Specs/Combat Sim Spec.md`, `Specs/Slice Numbers.md`,
`Specs/Balance Summary.md`, `Specs/Slice Implementation Roadmap.md`,
`Design/Combat and Abilities.md`, `Design/Items and Gear.md`,
`Design/Creatures and Enemies.md` (Goblin Raider), `Design/Classes/Knight.md`, and **both
prototypes read in full**: `Specs/prototype_formula_sim.py` (integer calibration:
`hp`, `mod`, `hit_chance`, `attack`, `cost_gt`, the `P` dict) and
`Specs/prototype_class_sim.py` v3 (the shared `hp_formula`, the `K` constants dict, the
full `T` tuning dict, `strike`/`weapon_attack`/`on_attacked`/`cost_gt`, and the per-class
bot rule lists). **No combat number or sim band in this spec is invented**: every constant
cites the prototype line or design doc it comes from, and the two prototypes are
cross-checked in §8.

**Scope boundary:** S2 delivers the melee attack pipeline (hit/miss/block/crit/damage/
resistance/threshold/death), equipment and inventory with weight and the armor-load speed
penalty, the named combat RNG stream, the Knight and Goblin Raider blueprints, and the
headless sim harness phase 1 with the approved doorway/open bands. S3 owns abilities,
charges-as-mechanics, statuses and wind-ups (the Knight's skill kit); S2's sim runs basic
attacks only. S5/S6 own healing/wards and spells. Ranged weapons, cover, flanking and
off-hand rules are out of S2 (recorded, §11).

---

## 1. Decisions at a glance (the eight S2 open questions)

| # | Question | Decision | One-line reason |
|---|---|---|---|
| Q1 | Where the resolution arithmetic lives (hit%, crit, block roll, damage chain) | **Native computes, scripts pre-compute**: every tunable input (accuracy, defense, block chance, resistance, threshold) is a script-written cached value under the S1 recompute pipeline; the native `Attack` resolver does only fixed-order integer arithmetic over those caches + `Tuning_Combat` | Resolution runs per attack in native hot paths — a Luau call per roll is exactly what architecture §6 forbids; script ownership of *policy* is preserved because every policy number is script-computed and cached (the S1 pattern, s1 §5.4) |
| Q2 | Where per-type resistance and threshold values live | **One new native cache part `Combat`** (block chance + per-`DamageType` resistance/threshold), written by the *same* `RecomputeDerived` handler in `rules/stats.luau`, never saved, rebuilt on load | The `Derived` part stays exactly S1's five computed stats (its tests and validation untouched); `Combat` gets its own typed payload and the same invalidation set, so one recompute event fills both |
| Q3 | Equipment model | **Native `Equipment` (fixed slot map) + `Inventory` (multi-list) parts; items are blueprint ids** — `Item_*` blueprints carry `Item`/`Weapon`/`Armor` parts; gear bonuses are *computed by the rules script from `Equipment`* and added on top of the authored `Bonuses` part, never written into it | M2 whole-part semantics make an item-instance part map heavy; blueprint ids keep saves small and patchable, and separating authored (innate/skill) bonuses from gear-derived bonuses avoids a double-count as soon as both exist |
| Q4 | Combat RNG stream | **One persistent named stream per `SimContext`, created lazily from the parent via `fork` and kept in a `BTreeMap`; every combat draw (hit, block, damage) pulls from it; it is serialized with the context** | The current per-roll `ctx.rng.fork(COMBAT_STREAM_LABEL)` in `effects.rs` consumes a draw from the *parent* stream on every roll — it both perturbs the parent and makes adding a roll shift combat; a persistent named stream is the fix the design asks for ("one forked SimRng per system") |
| Q5 | What is usable in the RUNNING build (roadmap human-UI acceptance) | **Debug-console equipment surface**: `inventory`, `equip <item> [slot]`, `unequip <slot>`, `give <item>` commands plus inspector equipment rows; the player spawns with the Knight kit equipped and can wield it | S4b owns the real HUD/inventory screens; the console is the M7-sanctioned input surface and satisfies "viewable and wieldable, not only inspector-visible" without inventing a UI layer early |
| Q6 | Save treatment | **`Equipment` and `Inventory` become whitelisted persistent parts; `SAVE_FORMAT_VERSION` bumps 2 → 3** in this milestone (S1 pattern: one bump per milestone adding whitelisted parts); `Combat` cache is never saved | Same contract s1 §6 recorded; gear must survive save/load as build state |
| Q7 | Sim phase-1 bot policies | **Luau policy scripts** (one per policy id, registered like content), not TOML rule lists | Scripts own policy per architecture §6; phase 1 needs ordered rule lists with condition predicates that TOML cannot express yet (target selection, HP thresholds); the S3/S7 abilities policies will need those predicates anyway (Combat Sim Spec §13 leaves this open — decided here) |
| Q8 | Enemy AI in the sim | **The real M6 AI path** (`GoblinBrain` policy script), extended to the new combat pipeline | Combat Sim Spec §13 prefers real AI "so results transfer"; the doorway queue falls out of real occupancy rules for free |

Carried forward from S0/S1 **without reopening**: typed tuning + validated overrides
(s0 §4); modifier mechanism `add` then `pct`, truncation toward zero (s0 §6); eager
`Derived` recompute with the declared dirtying set and the deferred-dispatch flush
(s1 §5.2, implemented in `events.rs::pending_recompute`); native owns integer arithmetic,
clamps and invariants; scripts own formulas and tuning policy (architecture §6); bots
never merge.

## 2. Damage types and the tuning additions

### 2.1 `DamageType` (native enum)

`TakeDamage.dtype` is a free `String` today. S2 fixes the slice's types as a **native
enum** with validation at the `TakeDamage` boundary:

```rust
pub enum DamageType { Slash, Pierce, Blunt, Fire, Cold, Lightning, Poison, Holy, Shadow }
// as_str: "slash", "pierce", "blunt", "fire", "cold", "lightning", "poison", "holy", "shadow"
```

`TakeDamage::validate` rejects an unknown dtype (fail-closed, same style as the negative-
amount check already in `events.rs`). The existing `"generic"` tag stays accepted and
behaves as zero resistance / zero threshold (the minimal path must not silently change).
The `"shadow"` type is named now (the Bonecaller's Shadow Bolt is S8 content) so the enum
does not churn later; S2 content uses slash/pierce/blunt only.

### 2.2 Tuning keys added to `Tuning_Combat`

Base `data/base/blueprints/tuning-combat.toml` and the campaign restatement both gain
(every value from the prototypes or design docs, cited):

| Key | Value | Source / use |
|---|---|---|
| `crit_threshold` | 5 | `K["crit_thr"]` = `P` inline `min(5,hit)` — crit on the best 5% of the d100 |
| `crit_percent` | 150 | `K["crit_pct"]` / `attack`: `dm*150//100` |
| `armor_speed_per_weight` | 2 | Combat Formulas §Tuning knobs `combat.armor.speed_per_weight` (2% speed per point over the armor threshold) |
| `armor_threshold_per_might` | 2 | Items and Gear: armor threshold = 2 × Might |
| `armor_resist_cap` | 75 | Combat Formulas: "Armor resist cap: 75% physical" |
| `res_floor` / `res_ceiling` | −100 / 90 | Combat Formulas: resistance range −100..90 (100 = immunity, handled as its own case) |
| `min_damage` | 1 | `attack`: `max(1, dm)` |

The S1 keys (`base_hit`, `hit_min/max`, `ac_factor`, `ac_cap`, `untrained_penalty`, …) are
already shipped and are consumed as-is. The `100` in `cost_gt` and the speed base are
literals per s1 §4.2's recorded convention.

## 3. The attack pipeline

### 3.1 Event set and exact ordering

One attack dispatches, in this fixed order (priorities follow the M3 bus; mutators before
observers, `(priority, seq)` order):

| Step | Event | Kind | Owner | Notes |
|---|---|---|---|---|
| 1 | `Attack { attacker, defender }` | dispatched by the caller (bump move, AI, bot policy) | — | unchanged struct; the *weapon* is read from the attacker's `Equipment`, not carried in the payload |
| 2 | native legality (EARLY mutator) | — | native | attacker and defender alive, defender carries `Health` with `hp > 0`, attacker carries a wielded melee weapon, defender within the weapon's reach (reach 1 = Chebyshev distance 1; the only reach S2 ships). Failure cancels — no events fire |
| 3 | `BeforeHit { attacker, defender, hit_pct }` | mutator, EARLY legality guards, then the combat-rules script's hooks at NORMAL | native dispatch; script may cancel | `hit_pct` is the native-computed chance (§3.2) carried so handlers can read it; a cancel vetoes the attack (future: dodge abilities). No S2 handler cancels |
| 4 | hit roll + crit + block, native, one at a time from the combat stream | — | native | order fixed in §3.2 |
| 5a | `Missed { attacker, defender, roll, hit_pct }` | observer LATE | native fires | exactly one of 5a/5b/5c fires |
| 5b | `Blocked { attacker, defender, roll, block_pct }` | observer LATE | native fires | the hit is negated; the S3 Riposte/Counterweight hooks attach here |
| 5c | `Hit { attacker, defender, roll, hit_pct, crit: bool }` | observer LATE | native fires | `crit = roll <= min(crit_threshold, hit_pct)` |
| 6 | `TakeDamage { target, amount, source: Attack, dtype }` | mutator chain | native computes `amount`, native `sim.rs` mutator applies `Health` | the full damage chain of §3.3 runs *before* dispatch; the existing native `TakeDamage` handler (sim.rs, NORMAL) decrements `Health` and fires `Death` at zero — unchanged |
| 7 | `Death { target }` | native, once (existing `was_alive` guard) | native | scheduler removal + AI corpse policy stay as shipped; S2 adds equipment drop = none (S8 owns loot) |

Resistance and threshold are **not separate events**: they are native steps inside the
`TakeDamage` amount computation (§3.3), which is what "damage, resistance, threshold" in
the roadmap means — separate *steps*, each individually logged, not separate dispatches.
A future status that modifies resistance attaches as a `TakeDamage` mutator (S3), which is
why the chain lives there.

Roll ownership (Q4): steps 4 draws exactly **two d100s maximum per attack in this order**:
(1) the hit roll, (2) the block roll *only if* the hit landed, the defender has
`block > 0`, and the damage type is physical (slash/pierce/blunt — magic is never
blocked, per Combat Formulas §Hit resolution and the prototypes' `can_block`). Damage
draws (3) one uniform integer in `[lo, hi]`. No other draws; `Missed`/`Blocked` consume
nothing further. This fixes the draw order so replay and band results are stable.

### 3.2 The resolution arithmetic (exact, integer-only)

All inputs come from the cached `Derived` (s1) and the new cached `Combat` part (Q2),
plus `Tuning_Combat`. Division is floor (`i64` /), matching the prototypes' `//`:

```
hit_pct  = clamp(T.base_hit + attacker.Derived.accuracy − defender.Derived.defense,
                 T.hit_min, T.hit_max)
roll     = d100 (combat stream)          # hits when roll <= hit_pct
crit     = roll <= min(T.crit_threshold, hit_pct)
blocked  = defender.Combat.block > 0 and dtype is physical
           and d100 (combat stream) <= defender.Combat.block
base     = uniform_integer(lo, hi)        # weapon's [min, max], combat stream
damage   = base + mod(stat) + flat        # mod(x) = (x−10)//2; stat from the Weapon part
         (ability multiplier: ×mult/100 here — S3; S2 has no multipliers)
if crit:    damage = damage * T.crit_percent // 100
res      = clamp(defender.Combat.resist[dtype], T.res_floor, T.res_ceiling)
if res == 100: damage = 0                  # immunity; skips threshold and min
else:      damage = damage * (100 − res) // 100 − defender.Combat.thr[dtype]
           damage = max(T.min_damage, damage)
```

Negative-amount and invariant handling stay native: the `TakeDamage` payload validator
already rejects `amount < 0`; the native `Health` decrement clamps at 0 (`hp` never goes
negative) and `Death` fires exactly once. `TakeDamage` with `amount == 0` (immunity) is
delivered and logged but changes nothing.

**Where `lo/hi/stat/dtype` come from:** the wielded weapon blueprint's `parts.Weapon`
(§4.2). Flat bonuses are `Bonuses.damage_flat` (authored) — the gear/skill split of
§4.4. `flat` in the chain is that value plus zero in S2 (skill passives are S3).

### 3.3 The `Combat` cache part (Q2)

```rust
pub const PART_COMBAT: &str = "Combat";
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Combat {
    pub block: i64,                          // block chance percent, 0 = no shield
    pub resist: BTreeMap<DamageType, i64>,   // summed per type, clamped to [res_floor, res_ceiling] by the writer
    pub threshold: BTreeMap<DamageType, i64>,// flat per type, >= 0
}
```

- Written only by the rules script through the whitelisted `set_part` path (the
  `Derived` write path in `scripting.rs::apply_set_part` gains a `Combat` sibling with the
  same fail-closed validation: non-integer fields and unknown damage types are errors).
- Never serialized (`save.rs::NON_PERSISTENT_PARTS` gains `Combat` beside `Derived`);
  rebuilt on load by the same load-path recompute that rebuilds `Derived` (s1 §6).
- Entity-id-ordered, idempotent, consumes no RNG — the s1 §5.2 guarantees carry over.
- Native reads are cache reads (`derived_or_energy_speed`'s pattern): no script call on
  any read path.

### 3.4 What the rules script computes (`rules/stats.luau` extended, same handler)

The `RecomputeDerived` handler (registered once by the engine, s1 §5.1) now also writes
`Combat` when the entity carries `Attributes`:

- **block** = shield block base (the equipped off-hand shield's `Armor.block`) +
  `(Might − 10)` (Combat Formulas: "+1 per Might above 10"; prototype Knight block
  `19 = 15 + (14 − 10)`). Raise Shield doubling is S3 (a status modifier), not here.
- **resist[type]** = sum over equipped armor pieces of `piece_weight_share × material
  profile[type]`, where the slot weights are the design's fixed table (head 30, arms 25,
  legs 35, body 100, out of 190) — full set = material value, partial sets pro-rated;
  plus innate sources (`Bonuses` gains per-type fields, §4.4). Armor-sourced physical
  resistance is then clamped to `armor_resist_cap` (75). Elemental/holy/shadow resistance
  ignores material profiles in S2 (armor contributes only via explicit affixes, of which
  the slice has none) — recorded so the rule is not rediscovered.
- **threshold[type]** = sum of equipped pieces' `Armor.threshold[type]` (slice: 0
  everywhere; the field exists because Bulwark III and creature thresholds need it).

The full material profile table (Padded/Leather/Brigandine/Mail/Plate × slash/pierce/
blunt) ships as tuning-adjacent **content on the armor blueprints** (`Armor.resist`), not
as a global table — a mod can add a material without touching base. The slot weights
(30/25/35/100) are `Tuning_Combat` keys `slot_weight_head/arms/legs/body`.

### 3.5 Worked values (acceptance fixtures, from Combat Formulas §Worked examples)

Each row is a headless test vector: build the two entities with the listed cached inputs
and assert the exact hit%/damage set. The first block reproduces the doc's examples
bit-for-bit; the second pins the blueprint-level values that differ (§8 explains why).

| Attack | Cached inputs | Expected |
|---|---|---|
| Knight → goblin | acc 4, def 12; longsword 4-8, Might 14 (mod 2), flat +1 | hit **42**; damage **7–11**; crit **10–16** (`7*150//100=10` … `11*150//100=16`) |
| Goblin → Knight (mail, round shield) | goblin acc 8; Knight dodge 1, AC 10 ⇒ def 21; block 19; mail slash res 30 | hit **37**; block roll ≤19; damage 3-6 slash → **2–4 after mail** (3→2, 4→2, 5→3, 6→4) |
| Goblin → Wizard (robes) | def 5, no resist/threshold | hit **53**; damage **3–6** |
| Rogue → goblin | acc 16, def 12; shortsword 3-6, Agility 15 (mod 2) | hit **54**; damage **5–8** |
| Mend (recorded, S5) | Will 13 | `40 * 109 // 100` = **43**% — pinned here so the heal formula's integer shape is fixed before S5 |
| Speed/armor load | Agility 9 Knight (base 98), worn 33 vs threshold 28 | penalty `2*(33−28)` = 10% ⇒ speed `98*90//100` = **88** |

## 4. Equipment and inventory

### 4.1 Parts (native, registered in `kinds.rs`/`stats.rs` style)

```rust
pub const PART_EQUIPMENT: &str = "Equipment";   // slot -> item blueprint id
pub const PART_INVENTORY: &str = "Inventory";   // ordered lists per slot-group

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Equipment {
    pub main_hand: String,      // "" = empty
    pub off_hand: String,       // shield, or a second blade later
    pub head: String,
    pub body: String,
    pub arms: String,
    pub legs: String,           // boots included (Items and Gear: "Boots are part of legs")
    // ring x2 / amulet / cloak: reserved names, absent in S2 payloads (deny_unknown_fields
    // keeps mods honest; the slots arrive with affix items)
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inventory {
    pub items: Vec<String>,     // blueprint ids in pickup order (deterministic)
}
```

- Fixed slot names are a native enum-equivalent (`EquipmentSlot` with `as_str`/`from_str`)
  validating the map keys — same reasoning as the attribute enum (s1 §2.1).
- Two-handed weapons: the `Weapon.hands` field (`1`/`2`); a two-handed `main_hand` rejects
  a non-empty `off_hand` at equip time (native invariant, `EquipError::HandsBusy`).
- Weight: each equipped item's `Item.weight`; **armor load** = worn armor + shield weight
  vs threshold `2 × Might` (Q: `armor_threshold_per_might`); **pack load** = inventory
  weight vs `10 × Might` — over capacity the `give`/pickup path refuses (no Burdened
  status in S2; recorded, §11).

### 4.2 Item blueprints (campaign mod, `Item_*` naming per s0 §3.2)

```toml
[Item_Longsword]
inherits = "Item"
parts.Item   = { name = "Longsword", slot = "main_hand", weight = 3, tier = "common", price = 40 }
parts.Weapon = { lo = 4, hi = 8, dtype = "slash", stat = "might", hands = 1, reach = 1,
                 cost = 100, accuracy_bonus = 0 }

[Item_RoundShield]
inherits = "Item"
parts.Item  = { name = "Round shield", slot = "off_hand", weight = 6, tier = "common", price = 25 }
parts.Armor = { ac = 2, block = 15, resist = { slash = 0, pierce = 0, blunt = 0 }, threshold = {} }

[Item_MailHauberk]   # body piece of the mail set
parts.Item  = { name = "Mail hauberk", slot = "body", weight = 14, tier = "common", price = 90 }
parts.Armor = { ac = 4, block = 0, resist = { slash = 30, pierce = 20, blunt = 0 }, threshold = {} }
```

- `Weapon`/`Armor` are native typed parts (serde, `deny_unknown_fields`) registered beside
  the s0 kind parts; `resist`/`threshold` keys validate against `DamageType`.
- `Weapon.cost` is the attack's energy cost (Items and Gear table); the attack action
  charges `cost_gt(cost, speed) = (cost*100 + speed − 1) // speed` GT on the scheduler —
  the prototypes' `cost_gt`, now native.
- Tiers (`common/fine/masterwork/rare`) are a string field in S2 with **no affix
  resolution** (affixes are S8/loot); `tier` exists so saves and loot rolls keep the axis.
- The slice ships exactly the kits the design tables name: Knight (longsword, round
  shield, mail set: hauberk/open helm/boots-in-legs), Rogue (shortsword + dagger, leather
  set), Cleric (mace, brigandine set), Wizard (quarterstaff, robes) — plus the goblin's
  short blade. All are data; only the Knight + goblin are *required* by the acceptance.

### 4.3 Speed: the armor-load extension (declared in s1 §10, landed here)

The `speed_base` formula is unchanged; the recompute pipeline appends the design's
post-base step (Combat Formulas §Time and the scheduler, Items and Gear):

```
worn    = sum(equipped Item.weight for armor slots + off-hand shield)
over    = max(0, worn − armor_threshold_per_might * Might)
penalty = armor_speed_per_weight * over          # percent
speed   = speed_base * (100 − penalty) // 100     # before status modifiers
```

then the S0 modifier pipeline and the invariant clamps (floor 20) as in s1 §4.5. The
Knight kit (33 vs 28 → 10%) is the §3.5 fixture. **Recording the calibration delta:** the
prototype class sim does *not* model the overload penalty, so engine band results may sit
a few points below the prototype's; §9's acceptance runs with the penalty ON (the design
rule is authoritative) and §9.3 says what to do if that pushes a band.

### 4.4 `Bonuses` semantics under gear (the double-count fix)

S1 authored `Bonuses` as the stand-in for gear (Knight `accuracy = 3`, goblin `ac = 2`).
With real equipment landing, the rule is: **the authored `Bonuses` part is innate/skill
bonus; the rules script reads `Equipment` and adds the equipped items' contributions on
top inside the formulas** (`accuracy`, `dodge`, `ac`, `damage_flat`, `block`,
per-type `resist`). Nothing writes into `Bonuses` from gear, so no equipped item is ever
counted twice. `Bonuses` gains per-type resistance/threshold fields for innate sources
(race, future skills):

```rust
pub struct Bonuses { accuracy: i64, dodge: i64, ac: i64, damage_flat: i64, block: i64,
                     resist: BTreeMap<DamageType, i64>, threshold: BTreeMap<DamageType, i64> }
```

(serde default per field; existing S1 payloads — plain `accuracy`/`ac` — load unchanged.)

**Equipment change is the s1-declared dirtying event:** the console `equip`/`unequip`
path and the script `set_part` write to `Equipment` both dispatch the existing
`RecomputeDerived` (queued via the same pending-recompute flush; the wiring mirrors the
`Attributes`/`Level` write path in `scripting.rs`).

### 4.5 Inspector and console (Q5)

- Inspector: the entity row summary gains an equipment segment
  (`wield: Longsword  off: Round shield  mail set  load 33/28 (+10%)`), read from the
  live `Equipment` part; the generic part pane shows `Inventory`/`Equipment` snapshots
  through the existing save-hook path.
- Console (all wired through the same event paths the sim uses, so headless and running
  builds behave identically): `inventory` (list), `give <Item_id>` (into `Inventory`),
  `equip <Item_id> [slot]` (moves inventory→slot, dispatches recompute),
  `unequip <slot>` (slot→inventory). Errors (`unknown item`, `slot occupied`,
  `hands busy`, `over capacity`) are returned as command errors, never silent.
- Wielding: an equipped `main_hand` weapon is what `Attack` resolves (§3.1 step 2) —
  a bare-handed actor cancels its attacks (S2 keeps no unarmed rule; recorded).

### 4.6 Saves (Q6)

- `Equipment` and `Inventory` join `build_save_hooks` as persistent whitelisted parts
  (canonical TOML text, restore fails closed — the s1 §6 mechanism verbatim).
- `SAVE_FORMAT_VERSION` 2 → 3. A v2 save loads as
  `UnsupportedVersion { found: 2, supported: 3 }` (accepted pre-release, named decision —
  no shipped saves exist).
- `Combat` joins `NON_PERSISTENT_PARTS`; load-path recompute rebuilds it in entity-id
  order together with `Derived`.
- Item **blueprint ids** persist; a save referencing a missing `Item_*` id is a load
  error naming the id (s0 §8's content-identity rule, applied to items).

## 5. The named combat RNG stream (Q4)

### 5.1 The bug being fixed

`effects.rs::resolve_use_ability` forks a fresh stream from the parent per roll
(`ctx.rng.fork(COMBAT_STREAM_LABEL)`). `SimRng::fork` consumes one draw from the parent,
so (a) every roll perturbs the parent stream (worldgen/AI streams shift when combat code
changes) and (b) the "combat" stream identity is not stable. The design requires the
opposite: "one forked SimRng stream per system, so adding a roll elsewhere does not shift
combat" (Combat Formulas §Conventions, determinism.md).

### 5.2 The fix

`SimContext` gains a named-stream registry:

```rust
streams: BTreeMap<&'static str, SimRng>,   // created lazily, deterministic

pub fn stream(&mut self, label: &'static str) -> &mut SimRng {
    // first call: self.rng.fork(hash(label)) — one parent draw, ever;
    // later calls: the cached stream, zero parent draws
}
```

- `label = "combat"` is the only S2 stream; the existing per-entity AI walk stream
  (`sim.rs`'s `fork(entity_to_id(actor))`) migrates to `stream("<entity-id>")`-style named
  entries in the same change so the parent stream is drawn exactly once per label for the
  whole context (the migration is behavior-neutral today because that fork is already
  once per action — the spec pins the target shape, not the interim).
- `SaveData` carries the stream map (each `SimRng` already round-trips its full ChaCha8
  state, `rng.rs`); the save version bump (Q6) covers the format extension. A save/load
  mid-fight therefore resumes the combat stream exactly — replay determinism holds.
- Why combat draws cannot disturb existing streams: the parent `ctx.rng` is drawn once
  per label for stream *creation*; after that, combat draws touch only the `"combat"`
  entry. Worldgen, AI and any future system fork their own labels; adding a combat roll
  consumes only combat-stream state. The per-run sim fork (§7.4) is taken from the master
  seed *before* the context exists, so scenario ordering cannot cross-contaminate either.
- The script surface keeps `engine.rng("combat")`-style access: `HostRng::fork(label)`
  (already shipped in `tessera-script`) is wired so a *script* fork with label `"combat"`
  resolves to the same persistent native stream (read-only draw access for scripted
  effects, e.g. S3's scripted abilities), rather than an independent host-side stream.
  One label, one stream, whichever side draws.

## 6. Content: Knight and Goblin Raider

### 6.1 The mapping table (prototype ↔ design doc ↔ blueprint)

The two sources disagree in two places. The prototypes are the **authoritative
calibration** (the task brief and Balance Summary both treat `prototype_class_sim.py` as
the current numbers); the deltas are recorded, not hidden:

| Value | Design doc | `prototype_class_sim.py` | S2 blueprint uses | Note |
|---|---|---|---|---|
| Knight AC | mail set + round shield = 8 + 2 = **10** (Items and Gear + Combat Formulas example) | `mk_knight`: `ac = 12` | **12** | the class sim's bands were produced at 12; adopting 10 would move every Knight result. Owner-review item (§11): either update the gear tables to sum to 12 or retune the sim — the blueprint pins 12 so the engine reproduces the bands either way |
| Goblin damage | "Short blade 2-5 slash" (Creatures and Enemies) | `T["gob_lo"]=2, T["gob_hi"]=5` | **2–5** | sources agree |
| Goblin HP | L2, T9 ⇒ 34 | `gob_hp_T = 9` | **34** | agree |
| Knight block | round 15 + (Might−10) | `block = 19` | **19** | agree |
| Knight resists | mail: slash 30 / pierce 20 | `res = dict(slash=30, pierce=20)` | **30/20** | agree |
| Knight acc/dmg bonus | Blade I Swordsmanship +acc/+dmg | `acc_bonus=3, dmg_bonus=1` | **+3/+1** | skill passives stand in as authored `Bonuses` until S3 |
| Goblin accuracy | "L2 +6" shorthand in the worked example | `accuracy` from L2, A13: `2 + 2*3 = 8` | **8** | the S1 fixture already pins 8 |

### 6.2 Blueprints (campaign mod, `data/mods/tessera-campaign/blueprints/`)

```toml
# --- classes.toml
[Knight]                     # level-3 build, the sim's reference player
inherits = "Humanoid"
parts.Health     = { hp = 50 }        # hp_formula(T13, L3) = 10+26+2*(3+4) — matches the sim
parts.Attributes = { might = 14, agility = 9, toughness = 13, will = 8, intellect = 11 }
parts.Level      = { level = 3 }
parts.Bonuses    = { accuracy = 3, damage_flat = 1 }   # Blade I stand-in (S3 owns the skill)
parts.Equipment  = { main_hand = "Item_Longsword", off_hand = "Item_RoundShield",
                     body = "Item_MailHauberk", head = "Item_OpenHelm", legs = "Item_SimpleBoots" }
scripts = ["scripts/policies/knight_expert.luau"]
```

The Knight's **AC 12** and **block 19** are script-computed from gear + Might (§3.4).
Since the shipped armor pieces sum to AC 10 + block 15+4 = 19, the kit carries one
authored `Bonuses.ac = 2` increment (a "gambeson under hauberk" line item) so the
computed total is the prototype's 12 without inventing a new item — the mapping table
records this explicitly as the concrete reconciliation and as the thing to clean up when
the AC table is retuned.

```toml
# --- creatures.toml (Goblin Raider, from the authoritative T dict + ENEMY table)
[GoblinRaider]
inherits = "Humanoid"
parts.Render     = { tile = "creature.goblin" }
parts.GoblinBrain = { detect = 10 }              # real M6 AI (Q8)
parts.Health     = { hp = 34 }
parts.Attributes = { might = 9, agility = 13, toughness = 9, will = 6, intellect = 7 }
parts.Level      = { level = 2 }
parts.Equipment  = { main_hand = "Item_ShortBlade" }   # 2-5 slash, cost 100, stat agility
scripts = ["scripts/goblin_ai.luau"]             # the shipped AI policy, retargeted to Attack
```

- `T["gob_hp_T"]/["gob_lo"]/["gob_hi"]` exist so sweeps can move the goblin without
  touching the blueprint; the blueprints ship at those values and the tuning keys remain
  the sweep surface (the blueprint is the default, the key is the knob — same shape as
  S1's campaign table).
- **Dirty Trick is S3 content and is absent from S2** (no statuses in S2). Phase-1 band
  runs are therefore slightly goblin-weaker than the prototype's final numbers; §9.3
  handles the consequence. Unarmed goblins never occur (the short blade is part of the
  blueprint).
- The existing base `[Goblin]` demo blueprint is unchanged (M6 demo compatibility); the
  Raider is the campaign's creature. The demo goblin's `Bonuses.ac = 2` stays authored
  (it stands for its leather, per the S1 fixture note).

### 6.3 The Knight is playable in the running build

`--headless` and the windowed demo spawn the player as `Knight` (the `Player` blueprint
gains the kit through inheritance for the demo world): the player bumps a goblin and the
full pipeline (hit/miss/block/crit/resist messages) is visible in the event log, satisfying
the roadmap's human-UI acceptance together with Q5's console commands.

## 7. Headless sim harness, phase 1

### 7.1 Interface (from Combat Sim Spec §3, phase-1 subset only)

```
tessera --sim scenarios/doorway_3_goblins.toml --build builds/knight_human_lv3.toml \
        --runs 1000 --seed 12345 --out results/knight_doorway.json
tessera --sim ... --set combat.crit_percent=150      # validated tuning override (s0 §4)
```

`--sim-suite`, `--sweep`, `--sim-compare` and `--check` are later phases (S5+); phase 1
ships the single-scenario runner, the JSON output, and the band check **inside the
acceptance test** (not yet a CLI flag — the roadmap acceptance is a test, and a CLI
band-flag added before suites exist would be untestable surface).

### 7.2 Scenario and build data (TOML, campaign content)

```toml
# data/mods/tessera-campaign/sim/scenarios/doorway_3_goblins.toml
[scenario]
id = "doorway_3_goblins"
map = "zone.sim_doorway"          # a fixed blueprint zone: room, one-tile doorway, guard room
turn_cap = 30000                  # game-time units (the prototype's stalemate cap in GT)
player_spawn = [3, 8]

[[enemies]]
blueprint = "GoblinRaider"
count = 3
spawn = "far_side"                # fixed cells, declared in the zone blueprint
start_alertness = "alerted"       # morale/alertness layers are S7/S8; phase 1 = always alerted

[win]
kills_all = true
[lose]
player_dead = true

[band]                            # approved 2026-10-18 (Combat Sim Spec §8) — verbatim
metric = "win_rate"
expert = [90, 100]                # doorway: Knight 90%+
```

```toml
# data/mods/tessera-campaign/sim/builds/knight_human_lv3.toml
[build]
id = "knight_human_lv3"
blueprint = "Knight"              # the §6.2 blueprint IS the build for phase 1
policy = "policy.knight_expert"
```

The **open** scenario is the same file with `map = "zone.sim_open"` (a clear room, no
doorway) and band `[55, 85]`. Both zones are small fixed blueprint zones (about 20×14)
shipped in the campaign mod; no generated arenas in phase 1 (Combat Sim Spec §13's map
question: hand-made arenas for phase 1, generated later).

### 7.3 Bots (Q7)

- `data/mods/tessera-campaign/scripts/policies/knight_expert.luau`: the ordered rule list
  transcribed from `knight_act` in `prototype_class_sim.py`, phase-1 subset (no
  abilities exist yet): if no enemy is adjacent, move toward the nearest enemy along the
  native path (this is the prototype's implicit approach logic, made explicit: goblins
  start at distance 8 and walk in); otherwise attack the lowest-HP adjacent enemy
  (`tkey = (−prio, hp)` with all priorities 0 in phase 1 ⇒ lowest HP, ties by entity id).
- `policies/knight_naive.luau`: attack if adjacent, else move toward the nearest enemy;
  no HP awareness, no tactics. This is the Combat Sim Spec §6 "naive" tier and it is what
  the *phase-1 acceptance* uses for the band rows — the approved bands are expert-bot
  numbers, so the expert policy carries the band check and the naive run is reported for
  the bracket (recorded in output, not asserted).
- Enemy behavior is the real goblin AI policy (Q8) driven through the same `Attack`/
  `Move` events; the doorway queue is emergent from occupancy (one actor per tile,
  `TileEntry`), matching the prototype's `prev.dist+1` single-file queue.

### 7.4 Runs, seeds, output

- Per-run RNG: the master seed `--seed 12345` seeds the context; run *i* plays under the
  named stream `"sim:<scenario>:<build>:<i>"` forked once from the master (Combat Sim Spec
  §3's derivation, verbatim). Run *i* is reproducible alone; adding scenarios shifts
  nothing.
- Output: one JSON line per run
  `{"run": i, "win": true, "turns": 31.4, "hp_left_pct": 0.42, "damage_dealt": 118,
    "damage_taken": 41, "kills": 3}` — the §7 metrics subset phase 1 can actually
  measure (outcome, turns, HP remaining on wins, damage dealt/taken, kills). A summary
  line (win rate, mean/median turns, mean HP lost on wins) is appended. No confidence
  intervals in phase 1 (S5+); the summary states the policy and seed.
- 1000 runs per scenario in the report; the CI acceptance test runs 500 (Combat Sim Spec
  §9's floor) at a fixed seed.

### 7.5 Determinism

Same seed + scenario + build + content ⇒ byte-identical output on every platform: no
floats in outcomes (all metrics are integers or integer-derived ratios), no hash
iteration (the harness is `BTreeMap`-keyed), fixed entity processing order (scheduler
order, then entity-id ties), the combat stream of §5. The existing headless determinism
test pattern (`sim.rs::headless_run_is_deterministic_and_covers_all_paths`) extends to
the sim runner.

## 8. Prototype cross-check (both `.py` files, read in full)

| Concept | `prototype_formula_sim.py` | `prototype_class_sim.py` | S2 adopts |
|---|---|---|---|
| `hp_formula` | `hp(T,L) = 10+2*T+(L-1)*(3+T//3)` | identical | s1 formula, unchanged |
| `mod` | `(x-10)//2` | identical | §3.2 |
| hit chance | `clamp(P["base"]+a.acc()-d.defense(),5,95)` | `K["base"]`, same clamp | §3.2 via cached acc/def |
| roll semantics | `randint(1,100)`; hit if `roll <= hit` | identical | d100 inclusive, `roll <= hit_pct` |
| block | `randint(1,100) <= d.block` | `<= d.block*(2 if raised)`; S2 drops the doubling (S3) | §3.2 |
| crit | `roll <= min(5,hit)`; `dm*150//100` | `K["crit_thr"]=5`, `K["crit_pct"]=150` | §3.2 |
| damage chain | roll + mod + bonus, crit, resist `(100-res)//100`, `−thr`, `max(1,…)` | `strike`: same chain, plus mult/flat/exploit | §3.2 (S2 has no mult/exploit) |
| resistance clamp | none (fixture res hard-coded) | `clamp(res,−100,90)` | §3.2 with tuning keys |
| `cost_gt` | `(c*100+speed-1)//speed` | identical | §4.2 |
| threshold | fixture `thr=0` | `d.thr` | cached, slice = 0 |
| K dict | — | all keys | mapped to `Tuning_Combat` (§2.2; `status_per_pt`, `heal_will_pct` are S3/S5 keys, named in s1 §4.7, **not added in S2**) |
| T dict | — | goblin/knight keys | §6 mapping; class keys for Cleric/Wizard/Rogue abilities are S3/S5/S7 content and are **not** added to tuning now |
| bands | calibration snapshot table (97/46 doorway/open, basic attacks) | Balance Summary results | the approved Combat Sim Spec §8 bands are the acceptance; the prototype's own win rates are context, not targets |

Two prototype details recorded, not adopted: the class sim's `E.speed` has no armor-load
term (§4.3 runs with the rule ON), and its goblin `Dirty Trick` is an S3 ability (§6.2).

## 9. Acceptance: tests and the band check

| Artifact | What it proves |
|---|---|
| `crates/tessera-core/tests/…` unit tests | `DamageType`/`Equipment`/`Inventory`/`Weapon`/`Armor`/`Combat` serde fail-closed; hands/occupancy invariants; `SimContext::stream` persistence (parent drawn once) |
| `crates/tessera/tests/combat_pipeline.rs` (headless, seeded) | §3.5's worked values exactly; event order `BeforeHit → Hit/Missed/Blocked → TakeDamage → (Death)`; block negates; crit math; immunity ⇒ 0; the draw-order lock (a blocked attack's damage roll never happens) |
| `crates/tessera/tests/gear_recompute.rs` | equip/unequip dirties recompute; the §3.5 armor-load speed fixture; AC/block/resist computed from gear; authored-`Bonuses` + gear addition has no double count |
| `crates/tessera/tests/gear_save_roundtrip.rs` | save/load ⇒ identical `Equipment`/`Inventory`; `Combat` absent from the snapshot and rebuilt equal; v2 save rejected as `UnsupportedVersion` |
| `crates/tessera/tests/sim_phase1.rs` | **the acceptance**: run the engine sim, doorway and open, 500 runs, seed 12345, expert Knight policy ⇒ doorway win rate ≥ 90 and open within [55, 85]; the same seed reproduces byte-identical output; a second seed lands in the same bands |
| `--sim` CLI demo | the running binary executes both scenarios and prints the summary; the console `equip`/`inventory` commands equip and wield the longsword in the running build (human acceptance) |

**Band policy (§9.3, the pre-agreed response to drift):** the bands are approved design
targets (Combat Sim Spec §8), the prototypes are directional. If the engine's first run
lands outside a band, the *tuning knobs* (`T["gob_*"]`, `armor_speed_per_weight`, the
Knight AC mapping) move via sweep — the scenario band fields and the fixture vectors stay
fixed; a band change is a design decision recorded in the card, never a silent test
 loosening. Phase-1 runs exclude Dirty Trick (§6.2); if the Knight's open rate lands
above 85 for that reason alone, the recorded expectation is "re-measure in S3 with
statuses", and the acceptance test for that run pins the measured value with a comment,
not a widened band.

## 10. Builder decomposition (each ~300–500 changed lines, own tests/demo)

1. **Combat pipeline** (~450): `DamageType`, `BeforeHit`/`Hit`/`Missed`/`Blocked`,
   native resolution replacing `install_minimal_combat`, the `Combat` part + script
   writes, the named-stream registry + save extension; unit + pipeline tests.
2. **Gear** (~450): `Equipment`/`Inventory`/`Item`/`Weapon`/`Armor` parts, item
   blueprints, rules-script gear terms + armor-load speed, dirtying wiring, console
   commands, save hooks + version 3; recompute/save/inspector tests.
3. **Content + sim harness** (~500): Knight/GoblinRaider/item content, scenario/build
   TOMLs, zones, the two policies, `--sim` runner + JSON output, the phase-1 acceptance
   test with bands; headless demo wiring.

Cards 1 and 2 are independent; 3 depends on both.

## 11. Recorded decisions, residual risks, out of scope

- **Knight AC 12 vs the gear tables' 10** (§6.1): reconciled by an authored +2 line item;
  the owner decides whether the item tables or the sim total moves. This is the one
  number in the spec with a live discrepancy, recorded here and in the card comment.
- **Armor-load penalty vs prototype**: the rule ships ON (design-authoritative); band
  drift, if any, is handled by §9.3's policy.
- **Phase 1 excludes Dirty Trick, Riposte/Counterweight triggers, Raise Shield, statuses,
  abilities, ranged weapons, cover/flanking, off-hand, ammo, identification, loadout
  swapping** (the two-weapon-set swap is S4b-adjacent UI over S3 resources). Each has a
  named owner milestone in the roadmap.
- **Untrained weapons**: `untrained_penalty` is applied by the rules script when the
  wielder lacks the weapon's training — S2 blueprints are all trained or deliberately
  untrained (the Wizard demo kit), so the term is implemented with the formula and
  exercised by the Wizard fixture, but skill-based training checks themselves are S4.
- **Repository guard**: no milestone/card/spec labels or task identifiers appear in
  implementation code, comments or filenames (the guard extended to filenames and
  lowercase labels in the current main); this spec file is documentation and follows the
  existing `docs/specs/` naming.

## 12. Governance

Bots never merge. This spec PR is authored as `jffjs-bots` on branch `wt/s2-combat`,
review requested from Jeff; merge and any overrides of the eight decisions in §1 are
Jeff's calls, folded back into this file before builders start.
