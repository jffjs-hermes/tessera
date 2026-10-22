# Engine design diagrams

These diagrams are the executable architecture map for the current engine. Node names intentionally match Rust modules and types; a node labelled **planned** is not part of the current workspace.

- [System boundaries](system.md) — crate responsibilities and data flow
- [Runtime turn loop](runtime.md) — one scheduled action through the simulation
- [Entity broker and persistence](ecs.md) — blueprints, parts, spawn, and saves
- [Typed event bus](events.md) — handlers, ordering, cancellation, and effects
- [Script bridge](script-bridge.md) — Luau/`mlua` and the native simulation boundary
- [World and zones](world.md) — terrain, generation, occupancy, and pathfinding

Each page has a short legend below its Mermaid diagram. The diagrams describe the implementation on `main`, not a proposed replacement architecture.
