//! Mod manifests: parsing and strict validation of a mod's `mod.toml`.
//!
//! This crate owns the data model for the `mod.toml` manifest schema and its
//! strict validation, plus the deterministic discovery and load-order
//! registry. It does **not** apply patches (a later work item); the types here
//! are deliberately ordered (`BTreeMap`/`BTreeSet`, validated `ModId` keys) so
//! discovery and ordering consume them deterministically without revalidation.
//!
//! Determinism (no `HashMap`/`HashSet`, no unordered iteration anywhere here):
//! the manifest's own dependency and load-order collections are sorted, the
//! API version is a plain integer checked against a single `tessera-core`
//! constant, and the load order is a pure function of the mod set.

pub mod discovery;
pub mod error;
pub mod load_order;
pub mod manifest;

pub use discovery::{ModSource, discover_dir};
pub use error::ManifestError;
pub use load_order::{LoadOrderError, LoadedMod, ModRegistry};
pub use manifest::{DependencyReq, LoadOrder, ModId, ModManifest, parse_manifest};
