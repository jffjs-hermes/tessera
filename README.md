# Tessera

[![CI](https://github.com/jffjs-hermes/tessera/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/jffjs-hermes/tessera/actions/workflows/ci.yml)
![Platforms](https://img.shields.io/badge/platforms-linux%20%7C%20windows%20%7C%20macos-blue)
![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-green)

A data-driven, scriptable, mod-friendly tile-based roguelike engine in Rust,
inspired by Caves of Qud. Desktop only (Linux/macOS/Windows; CI builds and tests all three).

**Status:** M0 (scaffold), M1 (tile renderer), M2 (data-driven entities), M3 (typed event
bus + energy scheduler wired into the demo) and M4 (Luau scripting) complete. M6 (world slice
and demo) is next.

## Run

```sh
cargo run -p tessera            # arrow keys / hjklyubn / wasd to move, `.`/Space to wait, Esc to quit
cargo run -p tessera -- --headless 12   # run 12 scripted actions, print final map + event log, exit 0
cargo run -p tessera -- --help
```

Movement goes through the sim's typed event bus and energy scheduler: arrow
keys and WASD submit a `Move`, `.` or Space submits a `Wait`, and a wall-blocked
move is cancelled (costs no energy). `--headless <n>` is the display-free smoke
path CI can run — it prints the final map then the one-event-per-line log.

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
