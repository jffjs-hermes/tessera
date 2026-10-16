//! wgpu cell-grid renderer. One instanced quad per cell, glyphs from an atlas.

pub mod atlas;
mod gpu;

pub use gpu::{Cell, Renderer};
