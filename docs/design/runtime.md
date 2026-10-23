# Runtime turn loop

`Sim::submit_action` drives the energy scheduler until an actor is ready, then `Scheduler::take_turn` dispatches through the shared `SimContext`.

```mermaid
flowchart TD
    Input["keyboard / CLI\nPlayerAction"] --> Submit["Sim::submit_action"]
    Submit --> Advance["Scheduler::advance_until_ready\nenergy / speed clock"]
    Advance --> Start["ActorTurnStart event"]
    Start --> Action{TurnAction}
    Action -->|Move| Move["Move event"]
    Action -->|Attack| Attack["Attack event"]
    Action -->|UseAbility| Ability["UseAbility event"]
    Action -->|Wait| Wait["cost turn; no action event"]
    Move --> Bus["SimContext::dispatch\nEventBus"]
    Attack --> Bus
    Ability --> Bus
    Start --> Bus
    Bus --> Native["native handlers\nmap + broker"]
    Bus --> Script["ScriptBridge\nscript handlers"]
    Native --> Broker["EntityBroker\nPartMap state"]
    Native --> Effects["effects.rs\nStatusEffects / Charges\nexpiry and stagger seams"]
    Script --> Broker
    Effects --> Bus
    Dirty["declared dirtying events\nspawn / part change / status"] --> Recompute["RecomputeDerived event"]
    Recompute --> Bus
    Bus --> Rules["rules/stats.luau\nformula bases"]
    Rules --> Finalize["recompute::finalize_derived\nmods + clamps + HP invariant"]
    Finalize --> Cache["effects::Derived\nreplace cached values"]
    Cache --> Native
    Bus --> Outcome["EventOutcome\nhandled / modified / cancelled"]
    Outcome --> Charge["Scheduler charges energy\nunless action cancelled"]
    Charge --> Next["next ready actor"]
    Broker --> Read["Sim::renderables / text_map\nread state only"]
    Zone["SimContext::map : Zone"] --> Native
    Read --> Renderer["tessera-render\nread-only renderer"]

    subgraph Ownership[Ownership]
        SimOwner["Sim owns the live headless loop"]
        RendererOwner["Renderer owns presentation only"]
    end
    SimOwner -.-> Submit
    RendererOwner -.-> Renderer
```

**Legend:** `Move`, `Attack`, `UseAbility`, `RecomputeDerived`, and `ActorTurnStart` are typed events. `rules/stats.luau` owns formula/tuning policy; `recompute::finalize_derived` owns native status modifiers, clamps, and the HP invariant. The renderer is downstream of state reads; it never dispatches events or mutates the broker. Effects are native parts and event handlers in `effects.rs`, not a separate `StatusStore` type. Native hot-loop reads use the cache and do not call scripts.
