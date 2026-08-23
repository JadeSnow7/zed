//! Standalone stdio MCP server for the Shared Context bus.
//!
//! Spawned by external Harnesses (Claude Code, Codex, Gemini CLI, ...) as a
//! project-level `context_servers` entry in Zed's settings; see
//! `crates/shared_context/src/shared_context.rs` for the storage layer and
//! `crates/shared_context/src/mcp_server.rs` for the protocol handling.
//!
//! stdout is reserved entirely for JSON-RPC responses --- all diagnostics go
//! to stderr.

use std::path::PathBuf;

use shared_context::SharedContextStore;

/// Overrides the default `shared_context.sqlite` location; used by the
/// stdio integration test to point at a temporary directory instead of the
/// user's real Zed data directory.
const DB_PATH_OVERRIDE_ENV_VAR: &str = "ZED_SHARED_CONTEXT_DB_PATH";

fn db_path() -> PathBuf {
    if let Ok(path) = std::env::var(DB_PATH_OVERRIDE_ENV_VAR) {
        return PathBuf::from(path);
    }
    paths::database_dir()
        .join("shared_context")
        .join("shared_context.sqlite")
}

fn main() -> anyhow::Result<()> {
    let db_path = db_path();
    let store = pollster::block_on(SharedContextStore::open(&db_path))
        .inspect_err(|err| eprintln!("shared-context-mcp: failed to open {db_path:?}: {err:#}"))?;
    shared_context::mcp_server::run_stdio_server(store)
        .inspect_err(|err| eprintln!("shared-context-mcp: server loop failed: {err:#}"))
}
