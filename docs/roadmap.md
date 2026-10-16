# Roadmap

Summary only; the full plan and rationale are in [architecture.md](architecture.md).

Language: Rust. Rendering: wgpu instanced cells; glyph atlas now, sprites later via tile ids.
Scripting: Luau via mlua. Desktop only.

- [x] **M0** Workspace, CI, licenses, VFS trait, seeded RNG, determinism rules
- [x] **M1** Atlas renderer, tile-id registry, movable `@`, tileset hot reload
- [ ] **M2** Blueprints with inheritance, spawning from data, hot reload; ECS + dynamic parts
- [ ] **M3** Event system, energy-based turn scheduler, headless sim tests
- [ ] **M4** Luau scripting (sandboxed), minimal API, one behavior written in script
- [ ] **M5** Mod loader: manifests, load order, patch ops (zip-able folders via VFS)
- [ ] **M6** Zones/chunks, cave worldgen, FOV, pathfinding, basic AI
- [ ] **M7** Versioned saves, debug console, entity inspector

Known M1 limits: 8x8 bitmap font only (ASCII, Latin, box, block); single hardcoded map;
no tile layering beyond terrain + one entity.
