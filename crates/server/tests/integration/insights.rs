//! Insights and reflect on Postgres. The scenarios live in `tests/insights_shared`, which the
//! SQLite suite runs too.

use std::sync::Arc;

use memoryos_core::MemoryStore;

use crate::common::{unique, Server};
use crate::pages_shared::Harness;

/// The server stays up, and its database exists, while the guard lives.
async fn postgres() -> (Harness, Server) {
    let server = Server::start().await;
    let store: Arc<dyn MemoryStore> = server.store.clone();
    let harness = Harness::new(server.url.clone(), store, &unique("insights"));
    (harness, server)
}

insight_scenarios!(postgres);
