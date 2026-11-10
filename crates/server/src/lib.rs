//! The MemoryOS HTTP service: REST API, MCP endpoint, authorization, limits and telemetry.
//! See docs/SPEC.md sections 3, 6, 9 and 10 and docs/openapi.yaml.

pub mod app;
pub mod auth;
pub mod cli;
pub mod config;
pub mod error;
mod local_server;
mod mcp;
mod middleware;
mod rate_limit;
mod readiness;
mod reranker;
pub mod routes;
mod service;
pub mod telemetry;
pub mod tui;
pub mod ui;
mod update;

/// The release version CI stamps in through `MEMORYOS_VERSION`, else the crate's.
pub const VERSION: &str = match option_env!("MEMORYOS_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};
