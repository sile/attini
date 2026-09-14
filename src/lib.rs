//! Sans I/O building blocks and async I/O shell for the attini coding
//! agent prototype.
//!
//! The `sansio` module holds parsers, data types, and state machines
//! that do not perform any I/O. Async transport, TUI, filesystem, and
//! subprocess integration live in sibling modules that drive the
//! Sans I/O types with concrete inputs.

// `unsafe` is denied crate-wide so a new use cannot slip in unreviewed.
// The few places that must call libc (process-group setup / teardown,
// liveness probing) opt in with an item-level `#[expect(unsafe_code,
// reason = "...")]`, which keeps the exception visible and justified.
// `deny` (not `forbid`) is required so `#[expect]` can override it.
#![deny(unsafe_code)]

pub mod child_output;
pub mod curl;
pub mod metrics;
pub mod permissions;
pub mod sansio;
pub mod session;
pub mod session_cmd;
pub mod tell_cli;
pub mod tools;

pub use metrics::Counter;
