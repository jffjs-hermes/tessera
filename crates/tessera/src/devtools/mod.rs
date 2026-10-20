//! Developer tooling inside the `tessera` binary.
//!
//! The headless console engine lives in [`console`] (parser + executor +
//! [`console::ConsolePanel`] transcript state) and is compiled unconditionally —
//! it has no egui dependency. The read-only entity snapshot builder lives in
//! [`inspector`] (also unconditional; the F3 window is gated). The **windowed**
//! egui overlay (console panel + entity inspector) lives behind the `devtools`
//! cargo feature so a default build has zero egui in the dependency graph.
//!
//! The egui wiring is deliberately not via eframe — we create an
//! [`egui_wgpu::Renderer`] from the *existing* wgpu Device/Queue/surface format
//! and an [`egui_winit::State`] fed from the existing winit event handler.
//! [`Devtools::on_window_event`] returns whether egui consumed the input
//! (game keys are suppressed when it does, so typing `s` in the console does
//! not move the player); [`Devtools::paint`] renders the egui frame *after* the
//! cell renderer into the same surface frame.

pub mod console;
pub mod inspector;

#[cfg(feature = "devtools")]
mod panel;
#[cfg(feature = "devtools")]
pub use panel::Devtools;
#[cfg(feature = "devtools")]
pub use panel::SceneCtx;
