# Entity broker and persistence

Blueprint loading produces resolved part data; spawning stamps those parts into the dynamic broker. Saves contain state and scheduler state, while wiring is rebuilt on restore.

```mermaid
flowchart LR
    Files["blueprint TOML / patches"] --> Loader["blueprints::BlueprintLibrary\ninheritance + patch ops"]
    Loader --> World["World\nspawn_blueprint"]
    World --> Spawn["spawn / spawn_blueprint_at"]
    Spawn --> Broker["EntityBroker"]
    Broker --> Parts["PartMap\nPartTypeId -> Box<dyn Part>"]
    Parts --> Native["typed native parts\nPosition, Health, ActorEnergy"]
    Parts --> Dynamic["dynamic / script parts\nScriptPart, PartValue"]
    Parts --> Derived["effects::Derived\ncached derived values\nnever serialized"]
    Broker --> Alive["live entities\nhecs::Entity ids"]
    Alive --> Hot["content hot reload\nnew loads / patches affect future spawns"]
    Broker --> Snapshot["save::snapshot_entities"]
    Scheduler["Scheduler\nclock, actors, energy, speed"] --> Save["SaveData"]
    Snapshot --> Save
    Save --> Encode["versioned postcard + zstd\npack-specific zone_id"]
    Encode --> Decode["read save"]
    Decode --> Restore["restore_broker + Sim::load"]
    Restore --> Broker
    Restore --> Scheduler
    Restore --> Rebuild["RecomputeDerived\nrebuild cache after load"]
    Rebuild --> Derived
    Restore --> Handlers["re-register native/script handlers\n wiring is not serialized"]
    Broker --> Despawn["despawn"]
    Despawn --> Scheduler
```

**Legend:** `PartMap` is the dynamic per-entity store alongside the native `hecs` world; there is no generic `StatusStore` or invented ECS facade. `Attributes`, `Level`, and `Bonuses` are persistent typed parts; `Derived` is a cached read model, omitted from saves and rebuilt through `RecomputeDerived` after restore. Save/restore preserves entity ids, persistent parts, RNG, and scheduler clock/actors, while `Zone` is supplied by the same pack and validated by `zone_id`.
