//! Black-box tests of the service over a real socket, with real Postgres. See common.rs.

#![cfg(feature = "integration")]

#[macro_use]
#[path = "../pages_shared/mod.rs"]
mod pages_shared;

#[macro_use]
#[path = "../insights_shared/mod.rs"]
mod insights_shared;

mod api;
mod authorization;
mod common;
mod contract;
mod insights;
mod limits;
mod mcp;
mod pages;
mod process;
mod tui;
mod ui;
