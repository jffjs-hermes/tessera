# System boundaries

The binary assembles the headless simulation and the read-only renderer. Content is loaded through `Vfs`; base content uses the same format intended for mods.

```mermaid
flowchart LR
    subgraph Content[Content boundary]
        Base["data/base\nblueprints, zones, scripts"]
        Mods["data/mods\nlocal directory or zip"]
        Vfs["tessera-core::vfs::Vfs"]
        Base --> Vfs
        Mods --> Vfs
    end

    subgraph Core["tessera-core — native deterministic simulation"]
        World["World\nblueprints + registries"]
        Zone["WorldMap / Zone\nterrain grid"]
        Broker["EntityBroker\nhecs + PartMap"]
        Events["SimContext / EventBus"]
        Scheduler["Scheduler\nenergy clock"]
        Effects["effects.rs\nStatusEffects, Charges"]
        Vfs --> World
        Vfs --> Zone
        World --> Broker
        Zone --> Events
        Broker <--> Events
        Scheduler --> Events
        Effects --> Events
    end

    subgraph Script["tessera-script — Luau host"]
        Host["ScriptHost\nmlua Luau"]
        Sandbox["sandbox.rs\nrestricted globals"]
        Value["value.rs\nPartValue bridge"]
        Host --> Sandbox
        Host --> Value
    end

    Bridge["tessera::scripting::ScriptBridge\nWorldAccess + SharedSim"]
    Sim["tessera::sim::Sim\nheadless sim owner"]
    Render["tessera-render\nread-only cells"]
    Binary["tessera binary\ninput / window / CLI"]

    World --> Bridge
    Broker <--> Bridge
    Zone --> Bridge
    Scheduler <--> Bridge
    Bridge <--> Host
    Sim --> Scheduler
    Sim --> Events
    Sim --> Render
    Render --> Binary
    Binary --> Sim

    classDef planned stroke-dasharray: 5 5
    PlannedMods["tessera-mods — planned crate"]:::planned
    PlannedMods -. future loader boundary .-> Vfs
```

**Legend:** solid arrows are current data/control paths. `EntityBroker`, `Scheduler`, and `EventBus` are owned by the headless `Sim`; `tessera-render` reads snapshots/cells and does not own simulation state. `tessera-mods` is planned as a separate crate; the current mod loading path is in the existing core/binary code.
