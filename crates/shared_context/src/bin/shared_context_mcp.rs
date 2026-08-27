//! Standalone stdio MCP server for the Shared Context bus.
//!
//! Spawned by external Harnesses (Claude Code, Codex, Gemini CLI, ...) as a
//! project-level `context_servers` entry in Zed's settings; see
//! `crates/shared_context/src/shared_context.rs` for the storage layer and
//! `crates/shared_context/src/mcp_server.rs` for the protocol handling.
//!
//! stdout is reserved entirely for JSON-RPC responses --- all diagnostics go
//! to stderr.

use shared_context::SharedContextStore;

fn main() -> anyhow::Result<()> {
    // `shared_context::DB_PATH_ENV_VAR` if Zed passed it, else this process's
    // own default. Zed always passes it, precisely because this process cannot
    // see a `--user-data-dir` given to its parent.
    let db_path = shared_context::db_path_from_env();
    let store = pollster::block_on(SharedContextStore::open(&db_path))
        .inspect_err(|err| eprintln!("shared-context-mcp: failed to open {db_path:?}: {err:#}"))?;
    shared_context::mcp_server::run_stdio_server(store)
        .inspect_err(|err| eprintln!("shared-context-mcp: server loop failed: {err:#}"))
}
