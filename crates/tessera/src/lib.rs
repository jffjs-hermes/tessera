//! Tessera demo application library surface.
//!
//! The bulk of the demo lives in the binary (`main.rs`, which owns the winit
//! loop and content loading); this library crate exposes the deterministic,
//! headless simulation harness and the script integration so integration tests
//! can drive the Zapper demo without a window. Keeping these out of the binary
//! means `--headless` CI can exercise them from `cargo test`.

pub mod mods;
pub mod scripting;
pub mod sim;

pub use mods::{Overlay, apply_mod_patches, discover_sources};
pub use scripting::ScriptBridge;
pub use sim::{PlayerAction, RenderTarget, Sim, build_cells};
