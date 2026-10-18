//! Tessera core: engine-side data model with no windowing or GPU dependencies.
//!
//! Determinism rules (see `docs/determinism.md`): all simulation randomness comes
//! from [`rng::SimRng`], and simulation code never iterates a `HashMap`/`HashSet`.
//! Use `BTreeMap`/`Vec` instead.

pub mod blueprints;
pub mod ecs;
pub mod events;
pub mod path;
pub mod rng;
pub mod script_part;
pub mod tiles;
pub mod turns;
pub mod vfs;
pub mod world;
pub mod worldgen;
pub mod zone;