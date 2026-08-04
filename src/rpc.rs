//! JSON-RPC 2.0 control interface for the coding agent.
//!
//! Compared to the interactive TUI in [`crate::tui`], this module
//! runs a headless RPC server so a caller (test harness, IDE
//! integration, or ad-hoc script) can drive the agent
//! programmatically over line-delimited JSON.
//!
//! - [`wire`] defines the JSON-RPC 2.0 envelope types
//!   (request / response / notification / error) and the parser
//!   that classifies an incoming line into a
//!   [`wire::Incoming`].
//! - [`server`] owns the [`AgentCore`] state machine and drives it
//!   from a single TCP connection, mapping RPC methods to core
//!   events and pushing observable events back as notifications.
//!
//! [`AgentCore`]: crate::sansio::agent::AgentCore

pub mod server;
pub mod wire;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

/// Runtime configuration for the RPC server. Kept parallel to
/// [`crate::tui::TuiConfig`] so the CLI layer can populate them
/// from the same option set.
#[derive(Debug, Clone)]
pub struct RpcConfig {
    pub model: String,
    pub listen_addr: SocketAddr,
    pub transcript_path: Option<PathBuf>,
    pub metrics_snapshot_interval: Option<Duration>,
}
