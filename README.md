# Tessera

[![CI](https://github.com/jffjs-hermes/tessera/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/jffjs-hermes/tessera/actions/workflows/ci.yml)
![Platforms](https://img.shields.io/badge/platforms-linux%20%7C%20windows%20%7C%20macos-blue)
![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-green)

A data-driven, scriptable, mod-friendly tile-based roguelike engine in Rust,
inspired by Caves of Qud. Desktop only (Linux/macOS/Windows; CI builds and tests all three).

**Status:** M0 (scaffold), M1 (tile renderer), M2 (data-driven entities), M3 (typed event
bus + energy scheduler wired into the demo), M4 (Luau scripting) and M6 (world slice
and demo) complete. M5 (mod loader) is next, with field of view planned as M8 after M7.

## Run

```sh
cargo run -p tessera            # arrows / WASD / hjklyubn / numpad to move, `.`/Space/numpad 5 to wait, `e` zap, Esc to quit
cargo run -p tessera -- --headless 12   # run 12 scripted actions, print final map + event log, exit 0
cargo run -p tessera -- --help
```

Movement goes through the sim's typed event bus and energy scheduler: arrows,
WASD, `hjklyubn`, and the numpad (all eight directions — `7 8 9` / `4 6` /
`1 2 3`) submit a `Move`; `.`, Space, and numpad `5` submit a `Wait`; `e` is a
`Zap` attack toward the last faced direction. Moving into a live enemy attacks
it instead of moving (bump rule). A wall-blocked move is cancelled (costs no
energy). `--headless <n>` is the display-free smoke
path CI can run — it prints the final map then the one-event-per-line log.
`--zone <id>` selects a named zone (`data/base/zones/*.toml`); the zone defaults
to the single zone in the pack. `--mods <dir>` discovers local directory and zip
mods (default `data/mods`); the shipped `sample-mod` adds a Bogling creature,
patches the base Goblin's tile and strength, and registers an Attack handler
that appears in the event log.

### Saves and the debug console (headless)

Saves use the M7 versioned, compressed (`postcard` + `zstd`) format with atomic
writes and a `.bak` rotation — see `docs/specs/m7-saves-tooling.md`. They are
pack-specific: a save can only load into the same zone that wrote it
(unknown-version files from a newer build are rejected loudly).

```sh
# Save target directory (default ./saves), and load a named save at startup:
cargo run -p tessera -- --headless 2 --save-dir saves --exec "save demo"
cargo run -p tessera -- --headless 2 --save-dir saves --load demo --exec "entities"

# The typed console runs headlessly too, before the scripted actions:
cargo run -p tessera -- --headless 0 --exec "spawn Goblin 5 5" --exec "entities"
```

`--exec "<line>"` is repeatable; each line runs **between** turns, before the
scripted `--headless` actions, and prints its outcome to stdout. Every executed
command appends a deterministic `CMD <verb> ... tick=..` line to the event log,
so a headless run is a complete record of how its final state was reached.

Console grammar (whitespace-tokenized; `#` starts a comment; malformed input
prints a usage error, never panics):

| Command | Effect |
|---|---|
| `help` | list commands |
| `list` | list blueprint ids in the loaded library |
| `entities` | list live entities: `<id> <blueprint> <pos> <hp>` |
| `spawn <blueprint> [x y]` | spawn (default = zone start tile) |
| `kill <entity-id>` / `kill all <blueprint>` | despawn one / all matching |
| `print <entity-id>` | dump an entity's parts |
| `sethp <entity-id> <n>` | write the `Health` part |
| `tp <entity-id> <x> <y>` | teleport (writes `Position`) |
| `save [name]` | write a save (default name `quicksave`) |
| `load <name>` | replace live state from a save |
| `clear` | clear the transcript |

The windowed egui console panel and entity inspector are planned for M7's
windowed devtools (`--features devtools`); the headless parser + executor above
already cover the same grammar without a window.


## The demo cave

The default demo loads a cave generated from `data/base/zones/start.toml`
(`--zone start`, editing the TOML requires a restart — zone hot reload is out of
scope). `@` is the player, `g` is a goblin. Both come from blueprints: the
player at the zone's start tile, the goblin at a seeded random walkable tile.

The goblin's AI and death/respawn policy live in `data/base/scripts/goblin_ai.luau`:

- keep your distance and the `g` wanders aimlessly;
- approach within 10 tiles (straight-line distance) and it chases you, biting for
  damage when adjacent — watch `hp=` in the title bar;
- `E` (Zap) the `@` into the goblin to kill it; after 5 of *your* turns a fresh
  `g` reappears somewhere walkable;
- if the goblin kills you, you respawn at the zone's start tile.

`--headless <n>` exercises the same sim and prints the goblin's event log too.
No field of view: the whole cave is visible by design.

While it runs, edit `data/base/tiles/ascii.toml` (e.g. change the `creature.player`
glyph or color) and the window updates immediately. A malformed edit logs a warning
and keeps the previous content.

Needs a Vulkan/Metal/DX12-capable GPU driver (wgpu). On Linux headless boxes,
Mesa's llvmpipe works.

## Layout

| Crate | Purpose |
|---|---|
| `tessera-core` | Windowless engine data: VFS, seeded RNG, tile registry, map |
| `tessera-render` | wgpu instanced-quad cell renderer + glyph atlas |
| `tessera` | Binary: window, input, hot reload |

Content lives in `data/base/`, which is meant to use the same format as mods.

## Design rules

- **Tile ids, not glyphs.** Entities reference ids like `creature.player`; tilesets map
  ids to appearance. Sprites can later replace glyphs per id with no entity changes.
  Unknown ids draw as a magenta `?`.
- **Determinism.** See [docs/determinism.md](docs/determinism.md).
- **All content loads through `Vfs`** (directory now; zip/in-memory for mods and tests).

See [docs/architecture.md](docs/architecture.md) for the full plan and [docs/roadmap.md](docs/roadmap.md) for milestone status.

## License

Dual-licensed under MIT or Apache-2.0, at your option.
