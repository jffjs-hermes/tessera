//! Tessera core: engine-side data model with no windowing or GPU dependencies.
//!
//! Determinism rules (see `docs/determinism.md`): all simulation randomness comes
//! from [`rng::SimRng`], and simulation code never iterates a `HashMap`/`HashSet`.
//! Use `BTreeMap`/`Vec` instead.

pub mod blueprints;
pub mod ecs;
pub mod effects;
pub mod events;
pub mod kinds;
pub mod path;
pub mod rng;
pub mod save;
pub mod script_part;
pub mod tiles;
pub mod tuning;
pub mod turns;
pub mod vfs;
pub mod world;
pub mod worldgen;
pub mod zone;

/// Version of the engine content API that mod manifests and scripts must match.
/// A load error is raised before any content file is read when a mod's
/// `api` field differs. One integer across the project keeps the engine,
/// mods and scripts on the same versioned surface.
pub const TESSERA_API_VERSION: u32 = 1;
