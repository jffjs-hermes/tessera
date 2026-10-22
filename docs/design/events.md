# Typed event bus

Events are plain typed structs. Native handlers and script handlers enter the same dispatch path through the shared broker and `SimContext`.

```mermaid
flowchart TD
    Source["Scheduler / native part / ScriptBridge"] --> Event["typed Event\nMove, Attack, TakeDamage, UseAbility"]
    Event --> Validate["Event::validate"]
    Validate --> Dispatch["SimContext::dispatch"]
    Dispatch --> Snapshot["snapshot handler list"]
    Snapshot --> Mutators["mutator handlers\npriority + registration seq"]
    Mutators --> Decision{HandlerAction}
    Decision -->|Continue| More["next mutator"]
    More --> Mutators
    Decision -->|Cancel| Cancel["EventOutcome::Cancelled\nskip observers"]
    Decision -->|Fail| Fail["EventError"]
    Mutators --> Post["Event::post_handled\ncommit e.g. Move Position"]
    Post --> Observers["observer handlers\nLATE / read-only"]
    Observers --> Handled["Handled or Modified"]
    Native["native handlers\nmap checks, damage, effects"] --> Mutators
    Script["script handlers\nregistered on equal terms"] --> Mutators
    Context["SimContext\nArc<RwLock<EntityBroker>> + Zone + SimRng"] --> Dispatch
    Bus["EventBus\nBTreeMap ordered\nEARLY / NORMAL / LATE"] --> Dispatch
    Scheduler["Scheduler::take_turn"] --> Source
    Effects["effects.rs\nStatusEffects / Charges"] --> Source
    Broker["EntityBroker / PartMap"] --> Context
    Context --> Broker
    Dispatch --> Nested["nested follow-up events"]
    Nested --> Guard["in-flight (event, entity)\n+ MAX_DEPTH = 16"]
    Guard --> Dispatch
```

**Legend:** lower priority values run first; registration sequence breaks ties deterministically. Mutators may modify payloads or cancel; observers run after the mutator phase and cannot cancel. Re-entrant pairs and excessive nesting return errors rather than panicking.
