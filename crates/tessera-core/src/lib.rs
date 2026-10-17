//! Tessera core: engine-side data model with no windowing or GPU dependencies.
//!
//! Determinism rules (see `docs/determinism.md`): all simulation randomness comes
//! from [`rng::SimRng`], and simulation code never iterates a `HashMap`/`HashSet`.
//! Use `BTreeMap`/`Vec` instead.

pub mod ecs;
pub mod map;
pub mod rng;
pub mod tiles;
pub mod vfs;
