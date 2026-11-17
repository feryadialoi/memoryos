//! Black-box tests of the service over a real socket, with real Postgres. See common.rs.

#![cfg(feature = "integration")]

#[macro_use]
#[path = "../pages_shared/mod.rs"]
mod pages_shared;

mod api;
mod authorization;
mod common;
mod contract;
mod limits;
mod mcp;
mod pages;
mod process;
mod tui;
mod ui;
