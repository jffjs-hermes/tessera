//! Developer tooling inside the `tessera` binary.
//!
//! This change ships the **headless console engine** ([`console`]): a pure
//! parser plus an executor that runs commands between turns against the live
//! [`Sim`]. It is compiled unconditionally — it has no egui dependency. The
//! windowed egui console panel / inspector will live behind a `devtools`
//! feature gate in separate later modules; the headless parser + executor here
//! stay ungated so CI and `--headless` can use them without a feature flag.

pub mod console;
