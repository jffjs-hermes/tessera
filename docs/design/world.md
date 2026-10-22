# World, zones, generation, and spatial queries

The current world is a deterministic map of data-loaded zones. A `Zone` owns terrain; entity occupancy is queried from `EntityBroker` positions; pathfinding composes both.

```mermaid
flowchart TD
    Vfs["Vfs::list / read"] --> Loader["zone loader"]
    Loader --> WorldMap["WorldMap\nBTreeMap<id, Zone>"]
    WorldMap --> Zone["Zone\nrow-major Vec terrain ids\nwidth, height, start"]
    Zone --> Gen{generator type}
    Gen -->|ascii| Cells["hand-authored cells"]
    Gen -->|caves| Worldgen["worldgen.rs\nseeded CaveParams"]
    Cells --> Zone
    Worldgen --> Zone
    Zone --> Queries["Zone::in_bounds\nZone::is_walkable"]
    Broker["EntityBroker\nPosition parts"] --> Occupied["occupied(x,y)\nspatial entity query"]
    Queries --> MoveTarget["walkable && !occupied"]
    Occupied --> MoveTarget
    MoveTarget --> Events["Move / Attack event\nSimContext"]
    Zone --> Path["path.rs\nA* / random_walkable_empty"]
    Occupied --> Path
    Path --> Native["native one-call navigation\nused by bridge and sim"]
    Zone --> Render["Sim::map / renderables\nread-only presentation"]
    WorldMap -.-> Planned["multi-zone traversal / portals — planned"]
```

**Legend:** terrain is separate from occupancy: a cell can be walkable yet occupied by an entity. `WorldMap` already supports multiple named zones, while the demo simulates one zone. The dashed portal/traversal node is explicitly planned rather than a current connection.
