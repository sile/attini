//! Sans I/O building blocks and async I/O shell for the attini coding
//! agent prototype.
//!
//! The `sansio` module holds parsers, data types, and state machines
//! that do not perform any I/O. Async transport, TUI, filesystem, and
//! subprocess integration live in sibling modules that drive the
//! Sans I/O types with concrete inputs.

pub mod deepseek;
pub mod sansio;
pub mod tui;
