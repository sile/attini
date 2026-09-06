//! Sans I/O building blocks and async I/O shell for the attini coding
//! agent prototype.
//!
//! The `sansio` module holds parsers, data types, and state machines
//! that do not perform any I/O. Async transport, TUI, filesystem, and
//! subprocess integration live in sibling modules that drive the
//! Sans I/O types with concrete inputs.

pub mod agent_cli;
pub mod child_output;
pub mod curl;
pub mod memories;
pub mod metrics;
pub mod permissions;
pub mod sansio;
pub mod session;
pub mod session_cmd;
pub mod skills;
pub mod tools;

pub use metrics::Counter;
