//! `dataseed harvest` — connect to a real Postgres database, sample rows,
//! and emit a `.dataseed` file inferred from the actual data.
//!
//! Feature-gated: only compiled when the `harvest` cargo feature is enabled
//! (the default). `cargo install dataseed --no-default-features` yields a
//! slim plant-only binary without postgres/regex pulled in.
//!
//! The pipeline is four passes over four owned data structures, no
//! in-place mutation:
//!
//! ```text
//!     introspect → HarvestSchema  (connect.rs)
//!     sample     → HarvestSchema  (sample.rs, fills sample/stats fields)
//!     infer      → InferenceOutput (infer.rs)
//!     emit       → String         (emit.rs; also runs the dataseed parser
//!                                  on its own output as a self-check)
//! ```

pub mod model;

#[cfg(feature = "harvest")]
pub mod connect;
#[cfg(feature = "harvest")]
pub mod sample;
#[cfg(feature = "harvest")]
pub mod infer;
#[cfg(feature = "harvest")]
pub mod topo;
#[cfg(feature = "harvest")]
pub mod emit;

#[cfg(feature = "harvest")]
pub mod run;

#[cfg(feature = "harvest")]
pub use run::{run_harvest, HarvestOptions};
