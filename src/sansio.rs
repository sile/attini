//! Sans I/O building blocks for the attini coding agent prototype.
//!
//! Modules under `sansio` must not depend on tokio, tokio-rustls, the
//! async parts of shiguredo_http11, `std::fs`, `std::process`, terminal
//! I/O, or any other blocking or asynchronous operation. They own
//! parsing, data types, and state transitions; the surrounding I/O
//! shell (async transport, TUI, filesystem, subprocess) lives in
//! sibling modules and drives these types with concrete inputs.
//!
//! Keeping the boundary structural — one directory for Sans I/O, other
//! modules for I/O — makes accidental dependency creep easy to spot in
//! code review.

pub mod agent;
pub mod deepseek;
pub mod permissions;
pub mod sse;
