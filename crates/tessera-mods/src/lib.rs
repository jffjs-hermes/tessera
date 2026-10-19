//! Mod manifests: parsing and strict validation of a mod's `mod.toml`.
//!
//! This crate owns the data model for the `mod.toml` manifest schema and its
//! strict validation. It does **not** discover mods on disk or in zips, build
//! load order, or apply patches; those arrive in later work. The types here are
//! deliberately ordered (`BTreeMap`/`BTreeSet`, validated `ModId` keys) so the
//! downstream discovery and ordering work can consume them deterministically
//! without revalidation.
//!
//! Determinism (no `HashMap`/`HashSet`, no unordered iteration anywhere here):
//! the manifest's own dependency and load-order collections are sorted, and
//! the API version is a plain integer checked against a single `tessera-core`
//! constant.

pub mod error;
pub mod manifest;

pub use error::ManifestError;
pub use manifest::{DependencyReq, LoadOrder, ModId, ModManifest, parse_manifest};
