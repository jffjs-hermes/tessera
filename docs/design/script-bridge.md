# Script bridge and determinism boundary

The bridge gives Luau owned values and narrow calls, not Rust borrows. Script mutations and native handlers operate on one shared broker and scheduler.

```mermaid
flowchart LR
    Luau["Luau scripts\ndata/base/scripts/*.luau"] --> Host["tessera-script::ScriptHost\nmlua"]
    Sandbox["sandbox.rs\nno io / os / network\ninstruction-time budget"] --> Host
    Host --> Engine["engine.* API\non, get_part, set_part, spawn_at\nplayer_turns"]
    Host <--> Values["value.rs\nPartValue / owned maps"]
    Engine <--> Bridge["tessera::scripting::ScriptBridge"]
    Bridge --> Access["SimWorldAccess\nWorldAccess + WorldRead"]
    Access <--> Broker["Arc<RwLock<EntityBroker>>"]
    Access --> Zone["Zone\nis_walkable / occupancy / map size"]
    Access <--> Scheduler["Arc<Mutex<Scheduler>>\nspawn/despawn same clock"]
    Access --> World["Arc<World>\nblueprint resolution"]
    Bridge --> Shared["SharedSim\nzone, scheduler, player, id map"]
    Shared --> Access
    Host --> Handlers["script event handlers"]
    Handlers --> Bus["same SimContext::dispatch\nas native handlers"]
    Rules["data/base/scripts/rules/stats.luau\nformula + tuning policy"] --> Handlers
    Bus <--> Broker
    Native["native core\nRNG, path queries, event invariants,
serialization, hot loops"] --> Bus
    Cache["effects::Derived\ncached read model"] --> Native

    subgraph Boundary[Determinism boundary]
        Read["reads return owned snapshots"]
        Write["writes are validated per call"]
        OneCall["one native map/nav query\n(no map loops in scripts)"]
    end
    Access --> Read
    Access --> Write
    Zone --> OneCall
```

**Legend:** the sandbox limits capabilities, while native core owns deterministic primitives, integer arithmetic, clamps, and invariants. `rules/stats.luau` chooses stats formulas and tuning policy; its writes are finalized into `effects::Derived`. Native hot loops read that cache and never call scripts. Scripts do not receive `&mut World`, broker guards, or unrestricted iteration. Script-facing ids from `spawn_at` are translated by `SharedSim` to live `hecs::Entity` ids.
