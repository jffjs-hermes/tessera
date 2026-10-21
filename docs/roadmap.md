# Roadmap

Summary only; the full rationale is in [architecture.md](architecture.md). This is the technical execution order for the slice, not a copy of the design notes. Each slice entry is a bounded planning summary; a later breakdown card will turn its hint into reviewable PRs of roughly 300–500 changed lines.

Language: Rust. Rendering: wgpu instanced cells; glyph atlas now, sprites later via tile ids.
Scripting: Luau via mlua. Desktop only. Native code owns primitives, invariants, deterministic hot loops; Luau owns rules, policy, and tuning.

## Engine roadmap

- [x] **M0** Workspace, CI, licenses, VFS trait, seeded RNG, determinism rules
- [x] **M1** Atlas renderer, tile-id registry, movable `@`, tileset hot reload
- [x] **M2** Blueprints with inheritance, spawning from data, hot reload; ECS + dynamic parts
- [x] **M3** Event system, energy-based turn scheduler, headless sim tests
- [x] **M4** Luau scripting (sandboxed), minimal API, one behavior written in script
- [x] **M5** Mod loader: manifests, load order, patch ops (zip-able folders via VFS)
- [x] **M6** Zones/chunks, cave worldgen, pathfinding, basic AI
  - M6 includes the playable demo: a goblin that wanders, chases within 10 tiles, and respawns; the player respawns at the start tile.
- [x] **M7** Versioned saves, debug console, entity inspector
- [ ] **M8** Field of view: recursive shadowcasting line of sight, player and AI visibility, FOV-based detection (replaces straight-line distance for detection); render the visible-tile set for the player, not detection logic alone.

Execution order: M4, M6 (world slice and demo), M5 (mod loader), M7, M8. M8 may run in parallel with S1–S6; the slice starts after M8 in dependency order, while S1–S6 do not require waiting for FOV except where noted.

## Slice roadmap: S0–S11

### S0 — Schema spike

- **Gap and surface:** M5 makes the campaign mod possible, but the loader currently has no agreed conventions for tuning, skills, abilities, statuses, classes, items, or quests. Validate the proposal that all are inherited/patchable blueprints, with typed parts and one Shield Bash path from TOML through Luau to a headless assertion; do not create parallel loader kinds without evidence.
- **Subsystems and boundary:** extend `tessera-core` dynamic parts/events and `tessera-script` bindings, plus campaign mod directories and hot reload. Keep data declarations in TOML; expose typed tuning lookup, effect dispatch, and deterministic RNG handles to scripts. This is a schema experiment, not a full content import.
- **Dependencies and missing capability:** M5 is required; S0 must finish before S3. Typed tuning, ability/status reload, and content validation do not exist today and are explicit gaps. Record save tags/version implications before new persistent parts land.
- **Decomposition hint:** one schema/loader validation increment, one script API and Shield Bash vertical slice, and one headless fixture/test increment.

### S1 — Stats and derived values

- **Gap and surface:** add Attributes, Level, HP, Accuracy, Dodge, Defense, and Speed as typed parts/derived values. Add a tuning blueprint readable by Luau and overridable by sim sweeps; values must not be scattered constants. Extend spawn/change hooks and the entity inspector/console enough to inspect derived values.
- **Subsystems and boundary:** `tessera-core` parts and deterministic event hooks; `tessera-script` `rules/stats.luau`; UI only for read-only visibility. Native owns integer arithmetic, clamping, and invariant checks; scripts own formulas and tuning policy.
- **Dependencies and missing capability:** follows S0 and can run beside M8. The engine has no attribute model, derived-stat invalidation, or typed tuning table. Save schema must version persistent attributes/level while derived values can be rebuilt.
- **Harness and acceptance:** begin sim phase 1 with formula fixtures reproducing HP, hit chance, and speed examples; use seeded runs and band checks once scenarios exist.
- **Decomposition hint:** data/part model, stats rules/API, then deterministic headless formula tests and inspector coverage.

### S2 — Melee combat, gear, Knight (sim phase 1)

- **Gap and surface:** replace the minimal Attack/TakeDamage path with before-hit, hit/miss, block, crit, damage, resistance, threshold, and death events. Add inventory/equipment slots, weapons, armor, tier, weight, armor-load speed penalty, and the Knight/Goblin Raider blueprints.
- **Subsystems and boundary:** `tessera-core` event bus, scheduler, parts and save hooks; `tessera-script` combat rules and tuning; console/inspector for equipment. Native owns legal targeting, occupancy, roll ordering, integer damage, and invariant-safe health changes. Scripts own hit formulas, resistance policy, and item tuning.
- **Dependencies and missing capability:** follows S1; equipment/inventory, typed hit-resolution events, damage types, tier/quality, and save serialization do not exist. Add named combat RNG fork without changing unrelated streams and plan a save version bump.
- **Harness and acceptance:** headless scenarios, build data, naive/expert bots, seeded repeated runs, and doorway/open bands form phase 1; Knight versus three goblins must be reproducible and inside approved bands.
- **Human UI acceptance:** equipment is viewable and wieldable in the running build, not only inspector-visible.
- **Decomposition hint:** event pipeline, equipment/item parts and save support, then Knight content plus scenario runner/bots/tests.

### S3 — Abilities and statuses (Knight skills)

- **Gap and surface:** add skill-rank scaling, active/passive/reactive ability blueprints, charges/cooldowns, adjacent/line/area targeting, status duration/stacking/modifiers, and telegraphed wind-ups with interrupt/cancel/completion events. Implement Knight Blade, Shield Craft, Bulwark, and Chivalry content.
- **Subsystems and boundary:** extend events and energy scheduler for wind-up timing; add target queries and cell-property hooks in world; extend Luau bindings for declarative effects and special scripts. Native owns target legality, timing, status lifecycle, and deterministic ordering; scripts own ability rules, scaling, and policy.
- **Dependencies and missing capability:** S0 schema and S2 combat are required. The current engine cannot represent statuses, pending actions, area targets, terrain tags, charges, or interrupts. Save active statuses and pending wind-ups with a version bump; use a status RNG stream where rolls exist.
- **Harness and acceptance:** phase 2 adds status/ability metrics, Knight bands, and seeded wind-up cancellation tests.
- **Human UI acceptance:** ability targeting and status display are visible in the running build; headless band checks remain required.
- **Decomposition hint:** scheduler/wind-up seam, status model/events, then target/effect API and Knight ability content with tests.

### S4 — Skills and progression

- **Gap and surface:** add skill/rank blueprints, XP/levels, point costs, class discounts, teacher caps, tomes, class/race presets, and console-level character creation. Keep the slice's 15 skills bounded; rank and gear scaling remain data-driven.
- **Subsystems and boundary:** core persistence and event bus for XP/LevelUp/SkillLearned; Luau progression rules over `Tuning_Progression`; UI console/inspector and a teacher stub. Native owns points non-negativity and rank-cap invariants; scripts own discounts, unlock policy, and costs.
- **Dependencies and missing capability:** follows S3; current engine has no progression, character build, teacher, or XP state. Save schema must preserve learned ranks and XP, and replay output must include build decisions.
- **Harness and acceptance:** create a Knight, reach level 5, learn ranks II/III through a teacher stub, and verify deterministic budget totals; extend build fixtures for later classes.
- **Decomposition hint:** progression parts/rules, build/teacher console path, then save/replay and headless progression tests.

### S4b — Game UI shell (parallel with M8)

- **Gap and surface:** the slice needs a human-playable view and control layer, not only debug visibility. Add a HUD for HP, statuses, charges/cooldowns, and a message log; an ability bar with adjacent/line/area targeting mode; equipment/inventory screens; a real character-creation screen for S4's Knight/class/race flow; and new-game/continue menus over the M7 save system.
- **Subsystems and boundary:** game UI is rendered NATIVELY by the existing world renderer (`wgpu` instanced-cell / glyph-atlas path) — NOT by `egui` (which stays reserved for the M7 dev console and inspector). Inventory, character sheet and dialog screens open as overlay/dialog panels drawn over the rendered game world, Qud-style; a persistent message log is always visible beside the world (placement to be confirmed later). The UI is a view/control layer over the same event bus the headless sim uses: it never owns simulation state, feeds the RNG, or decides invariants; player input enters as plain events. Native owns the cell renderer, atlas, overlay layout primitives and input translation; the sim and scripts retain state, rules, policy, and tuning.
- **Dependencies and missing capability:** runs in the M8 parallel band and consumes the event, ability, equipment, progression, and save capabilities as they land. The current console/inspector path is not a human entry point, and no usable HUD, menus, inventory surface, character creation, targeting mode, or action log exists.
- **Harness and acceptance:** exercise every surface in a desktop build: start or continue a game, create a Knight, inspect and wield gear, select an ability and target adjacent/line/area cells, observe HP/status/charge changes and messages, and save/continue without changing seeded simulation outcomes. Keep headless UI-independent scenarios and event assertions.
- **Decomposition hint:** 2–3 builder cards, each 300–500 changed lines: (1) HUD, message log, and input plumbing; (2) menus, character creation, and equipment/inventory screens; (3) targeting mode and ability bar.

### S5 — Cleric

- **Gap and surface:** use the S3 framework for Restoration, Aegis, Radiance, and Devotion: heals, wards, holy damage, and Withering. Add class content and the status/effect combinations that exercise support play without inventing a class-specific engine.
- **Subsystems and boundary:** campaign blueprints/scripts, event effects, scheduler charges/cooldowns, and sim metrics; reuse native target/status invariants. Scripts own healing/ward/holy rules and tuning; native owns application order and legality.
- **Dependencies and missing capability:** S4 is required; no additional engine kind should be assumed until S0 proves the blueprint model. Save active charges/statuses and use deterministic status/combat streams.
- **Harness and acceptance:** phase 2 Cleric builds and doorway/open scenarios must meet the approved bands, with ward/heal/status metrics and seeded regression fixtures.
- **Decomposition hint:** Cleric data, healing/ward effect scripts, then sim builds/scenarios and regression coverage.

### S6 — Wizard

- **Gap and surface:** add Pyromancy, Cryomancy, Spellcraft, and Arcane Study with areas, fire/ice effects, Ice Wall, burning ground, and kiting hooks. The current map has no terrain-tag/property layer or temporary terrain effects.
- **Subsystems and boundary:** extend world cell storage and renderer-readable tile state, scheduler/effects, scripting API, and campaign data. Native owns area bounds, terrain mutation, occupancy, and deterministic spread; scripts own spell policy, damage/tuning, and AI decisions.
- **Dependencies and missing capability:** S4 is required; M8 can run in parallel. Explicit gaps are terrain tags, temporary cell properties, area effects, deterministic fire spread, and kiting movement hooks. Persist terrain effects if saves can occur mid-encounter; version accordingly.
- **Harness and acceptance:** phase 3 adds terrain/fire/ice scenarios, self-damage metrics, naive/expert bots, seeded spread tests, and Wizard bands.
- **Decomposition hint:** cell-property/terrain API, spell effects and content, then kiting AI and terrain sim fixtures.

### S7 — Rogue and stealth

- **Gap and surface:** add alertness states, noise and notice rolls, perception/light policy, Sneak Attack, Pocket Sand, and Hit and Run. M8 FOV is a hard dependency: straight-line distance is not sufficient for stealth.
- **Subsystems and boundary:** world FOV/visibility, event bus (`Noticed`, `AlertChanged`), scheduler, and Luau AI policy; UI can expose alertness for debugging. Native owns LOS, noise propagation primitives, and deterministic notice ordering; scripts own alert policy and stealth tuning.
- **Dependencies and missing capability:** requires S4 and M8. Alertness/noise/notice, legal-information bot inputs, and stealth state persistence do not exist. Add deterministic AI/stealth RNG forks and save alertness only where the save model requires it.
- **Harness and acceptance:** phase 4 Rogue scenarios compare unaware ambushes with alerted mixed groups; run seeded naive/expert policies and verify the 15-point ambush advantage target.
- **Decomposition hint:** FOV/noise integration, alertness/status rules, then Rogue content and stealth harness scenarios.

### S8 — Creatures, AI archetypes, and loot

- **Gap and surface:** add creature Equipment/Loot parts, drop rules, tiers/quality, hidden skills, morale, Captain and the roster; implement brute, skirmisher, archer, caster, swarm, and guardian policies plus cult abilities. Loot is rolled at spawn/generation, not nondeterministically at death/open.
- **Subsystems and boundary:** campaign blueprints and Luau AI/loot, event hooks for death/drop/morale, scheduler and save serialization. Native owns spawn/despawn, inventory containment, RNG stream isolation, and item invariants; scripts own archetype policy, morale tuning, and loot table rules.
- **Dependencies and missing capability:** needs S3 and S5; current AI has only wander/chase/bump combat and no morale, ranged/area tactics, inventory drops, or loot tables. Add a named `loot` RNG stream and save generated item instances with a schema bump.
- **Harness and acceptance:** ruins scenarios and phase-5 metrics cover archetypes, morale, drops, and seeded runs; approved ruins bands must pass.
- **Decomposition hint:** item/loot persistence, archetype and morale policies, then roster/cult content with ruins sim fixtures.

### S9 — Sites, worldgen, and loot

- **Gap and surface:** add loot tables, containers, site loot slots, tier/quality rolls, guarantees, plains/forest biomes, village, graveyard, cave, and ruins stamps with spawn tables and role-based story casting.
- **Subsystems and boundary:** extend `tessera-world` zone/chunk generation, VFS/mod content, scripts, renderer cell properties, and save/load. Native owns stamp placement, walkability, seed derivation, and deterministic generation; scripts own table selection, level scaling, and site policy.
- **Dependencies and missing capability:** requires M6 and S8. Zone stamps, biome layering, spawn tables, containers, and generated loot are absent. Fork `loot` from worldgen so loot changes do not perturb layout; preserve generated sites/items in saves and bump schema. Account for the known RenderTile save issue before relying on visual round trips.
- **Harness and acceptance:** same seed must generate village, graveyard, cave, and ruins identically; validate loot guarantees and site draw cells, not just logs.
- **Decomposition hint:** worldgen stamps/biomes, loot/container generation, then site content/spawn tables and deterministic generation tests.

### S10 — Quests, NPCs, shops, and rewards

- **Gap and surface:** add quest state/stages, journal, branching dialogue topics, reputation, teachers, shops/stock tables, essential-NPC handling, and slice rewards. Quest and dialogue should be patchable blueprints driven by events rather than a new hard-coded content registry.
- **Subsystems and boundary:** core event/state persistence, VFS/mod data, Luau quest/shop rules, save schema, and UI journal/dialogue/shop surfaces as native overlays built on the S4b shell. Native owns event delivery, state serialization, and essential-NPC invariants; scripts own stage predicates, dialogue policy, stock/reward tuning.
- **Dependencies and missing capability:** needs S4, S9, and M7. The engine has no quest state, journal/dialogue UI, reputation, shop, or event-driven objective model. Save quest progress, reputation, stock, and item identity with a version bump; preserve deterministic reward rolls.
- **Harness and acceptance:** headless quest fixtures must advance by `Entered`, `Found`, and `Obtained` events; playable acceptance completes the graveyard/ruins/cellar chain and both side quests.
- **Decomposition hint:** quest/event state and persistence, UI plus NPC/shop interactions, then content/rewards and end-to-end quest fixtures.

### S11 — Slice integration

- **Gap and surface:** wire the campaign mod, four classes, village through graveyard/cave/ruins, teachers, combat, stealth, loot, quests, saves, and the final story piece into one playable path. This is integration and tuning, not a new general-purpose subsystem.
- **Subsystems and boundary:** all crates and VFS/mod loading; final UI/readability and save/replay checks. Keep native/script ownership from earlier entries; resolve only integration seams, missing invariants, and content wiring.
- **Dependencies and missing capability:** all prior slice work is required. Explicitly audit that capabilities are shipped data and used by the running build (not merely tested in isolation), and that saves restore visible render tiles and gameplay state.
- **Harness and acceptance:** complete an explicit full HUMAN playthrough per class, plus phase-5 gauntlets/sweeps and band checks, then record known issues; use deterministic replay and draw-cell assertions for visibility.
- **Decomposition hint:** campaign wiring, cross-system playthrough/regression fixes, then balance sweep and release-readiness documentation.

## Order, parallelism, and open questions

The Knight proves combat and the ability framework before Cleric and Wizard exercise support, charges, areas, and terrain; Rogue waits for FOV; creatures follow three classes so ruins encounters can be measured; worldgen and quests come last because they depend on stable combat and content. M8 can run alongside S1–S6, while S0 must settle before S3. The headless harness grows from phase 1 combat scenarios to statuses, terrain, stealth, then gauntlets/sweeps and CI band checks.

These choices remain explicit review questions rather than silent architecture decisions: (1) retain S0–S11 labels or continue engine numbering after M8; (2) confirm all content kinds are blueprints versus adding loader kinds; (3) choose declarative ability effects plus scripts for exceptions versus scripts only; (4) choose additive/percent status modifiers versus an expression language; (5) choose eager derived-stat recomputation versus lazy reads; (6) choose TOML rule-list versus Luau bot policies and hand-authored arenas versus generated scenario maps; (7) decide the CI subset and band widths. S0 is the decision gate for schema questions; the sim phases are the decision gates for bot and CI questions.

## Status notes

M5 is complete on the board and here. M8 remains planned and deferred from M6. M7 save/load render-tile preservation was addressed separately; any remaining save/render regression must be verified before S9–S11 rely on visual round trips.
