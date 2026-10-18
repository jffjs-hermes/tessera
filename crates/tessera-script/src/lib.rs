//! Tessera script runtime: a deterministic, sandboxed Luau execution layer.
//!
//! This crate owns the VM lifecycle, sandbox setup, instruction/memory
//! budgets, the versioned v1 script API, handler registration/dispatch seam,
//! and the error/quarantine taxonomy. It does **not** load blueprint files,
//! watch the filesystem, or wire binary input — those live in the `tessera`
//! crate.
//!
//! Determinism (docs/determinism.md rule 3): the runtime keeps its script
//! registries in `BTreeMap`/`BTreeSet` (never `HashMap`/`HashSet`) and exposes
//! no wall-clock or `f32` gameplay values to scripts.

pub mod error;
pub mod host;
pub mod sandbox;
pub mod value;

pub use error::ScriptError;
pub use host::{
    DefaultWorldRead, DispatchResult, HandlerOutcome, HostRng, PendingAction, ScriptHost,
    WorldAccess, WorldRead,
};

/// Version of the script API implemented by this runtime (`-- tessera-api: 1`).
pub const TESSERA_API_VERSION: u32 = 1;

/// Default per-invocation instruction budget. A tunable constant, not a
/// gameplay quantity.
pub const SCRIPT_INSTRUCTION_BUDGET: u64 = 5_000_000;

/// Per-VM memory cap, ~4 MiB.
pub const SCRIPT_MEMORY_LIMIT: usize = 4 * 1024 * 1024;

/// A script handler must error this many consecutive times before it is
/// quarantined (unregistered) for that `(script, event)` pair.
pub const QUARANTINE_THRESHOLD: u32 = 3;

/// The first entity id the host assigns to a scripted `engine.spawn` /
/// `engine.spawn_at`. Encodes hecs `generation = 1` in the high 32 bits and a
/// high low-32 `id`, so the binary's `WorldAccess` can reconstruct the entity
/// with `Entity::from_bits` and the id never collides with the simulation's own
/// small generation-0 entities (hecs `spawn_at` despawns any existing
/// *same-id* entity, so both the generation and the id field must differ from
/// live ones).
pub const FIRST_SCRIPT_ENTITY: u64 = (1 << 32) | 0xE000_0000;
