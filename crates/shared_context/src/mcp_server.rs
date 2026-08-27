//! A minimal Model Context Protocol (MCP) server, wired to [`SharedContextStore`].
//!
//! This intentionally does not reuse `context_server`'s client-side protocol
//! types (`crates/context_server`): that crate hard-depends on `gpui` for its
//! request/response plumbing, which would drag a full GUI toolchain into a
//! binary meant to be spawned, over stdio, by external Harness processes that
//! may run headless. The wire format below (JSON-RPC 2.0, newline-delimited
//! over stdio, `camelCase` fields) matches what `context_server`'s transport
//! and types already expect of MCP servers, so it interoperates with it and
//! with the reference Harness clients (Claude Code, Codex, Gemini CLI) --- it
//! is just implemented independently.

use std::io::{BufRead, Write};

use anyhow::{Context as _, Result, anyhow};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{MAX_CONTEXT_ROWS_PER_TABLE, MissionId, SharedContextStore};

const PROTOCOL_VERSION: &str = "2025-06-18";
const SERVER_NAME: &str = "shared-context-mcp";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

const MISSION_ID_HINT: &str = "If you do not know the current mission_id, check the initial \
    task description you were given for a Mission id. If none is present, this conversation is \
    not part of a Mission, and you should not call this tool.";

const METHOD_NOT_FOUND: i32 = -32601;
const INVALID_PARAMS: i32 = -32602;
const PARSE_ERROR: i32 = -32700;

#[derive(Debug, Deserialize)]
struct IncomingMessage {
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

fn ok_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn err_response(id: Value, code: i32, message: impl Into<String>) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message.into() } })
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "record_decision",
            "description": format!(
                "Record a durable decision for a Mission so other Harnesses working the same \
                 Mission can see it. {MISSION_ID_HINT}"
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "mission_id": { "type": "string", "description": "The Mission id this decision belongs to." },
                    "key": { "type": "string", "description": "Short label for the decision, e.g. \"auth-strategy\"." },
                    "value": { "type": "string", "description": "The decision text, verbatim." },
                    "author": { "type": "string", "description": "Readable identifier for the Harness/agent recording this, e.g. \"claude-code\"." },
                    "role": { "type": "string", "description": "Copy the `role` value from the <zed-mission-context> block you were given, verbatim. Zed groups rows by role to show each worker its own trail; omit this and what you record will not appear on your worker page." }
                },
                "required": ["mission_id", "key", "value"]
            }
        },
        {
            "name": "record_artifact",
            "description": format!(
                "Record that a file was created or changed as part of a Mission. Zed's own \
                 observer already records this automatically for edits it performs on a \
                 Harness's behalf; call this directly only for a change Zed cannot see (e.g. \
                 made inside your own sandbox, outside of a Zed client capability). \
                 {MISSION_ID_HINT}"
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "mission_id": { "type": "string", "description": "The Mission id this artifact belongs to." },
                    "path": { "type": "string", "description": "Path of the file that changed." },
                    "change_summary": { "type": "string", "description": "One-line summary of what changed." },
                    "author": { "type": "string", "description": "Readable identifier for the Harness/agent recording this." },
                    "role": { "type": "string", "description": "Copy the `role` value from the <zed-mission-context> block you were given, verbatim. Zed groups rows by role to show each worker its own trail; omit this and what you record will not appear on your worker page." }
                },
                "required": ["mission_id", "path", "change_summary"]
            }
        },
        {
            "name": "record_evidence",
            "description": format!(
                "Record the outcome of a command run as part of a Mission. Zed's own observer \
                 already records this automatically for commands it runs on a Harness's behalf; \
                 call this directly only for a command Zed cannot see. {MISSION_ID_HINT}"
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "mission_id": { "type": "string", "description": "The Mission id this evidence belongs to." },
                    "command": { "type": "string", "description": "The command that was run." },
                    "result": { "type": "string", "description": "Its output, or a summary of it." },
                    "exit_code": { "type": "integer", "description": "Its exit code, if known." },
                    "author": { "type": "string", "description": "Readable identifier for the Harness/agent recording this." },
                    "role": { "type": "string", "description": "Copy the `role` value from the <zed-mission-context> block you were given, verbatim. Zed groups rows by role to show each worker its own trail; omit this and what you record will not appear on your worker page." }
                },
                "required": ["mission_id", "command", "result"]
            }
        },
        {
            "name": "get_mission_context",
            "description": format!(
                "Read what other Harnesses have recorded for a Mission so far: decisions, \
                 file/artifact changes, and command evidence, most recent first, capped at \
                 {MAX_CONTEXT_ROWS_PER_TABLE} rows per category. {MISSION_ID_HINT}"
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "mission_id": { "type": "string", "description": "The Mission id to read context for." },
                    "since": { "type": "string", "description": "RFC3339 timestamp; only return rows created after this." }
                },
                "required": ["mission_id"]
            }
        }
    ])
}

fn required_str(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("missing required argument `{key}`"))
}

fn optional_str(args: &Value, key: &str, default: &str) -> String {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| default.to_string())
}

/// The caller's Mission role, if it passed one.
///
/// Absent rather than defaulted: a row with no role is honestly unattributed,
/// whereas a placeholder would silently group every Harness that forgot the
/// argument under one imaginary worker. `author` can default to `"unknown"`
/// because nothing filters on it.
fn optional_role(args: &Value) -> Option<String> {
    args.get("role")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|role| !role.is_empty())
        .map(str::to_string)
}

fn parse_mission_id(args: &Value) -> Result<MissionId> {
    MissionId::from_key_string(&required_str(args, "mission_id")?)
}

async fn call_record_decision(store: &SharedContextStore, args: &Value) -> Result<String> {
    let mission_id = parse_mission_id(args)?;
    let key = required_str(args, "key")?;
    let value = required_str(args, "value")?;
    let author = optional_str(args, "author", "unknown");
    let role = optional_role(args);
    store
        .record_decision(mission_id, key.clone(), value, author, role)
        .await?;
    Ok(format!(
        "Recorded decision `{key}` for mission {mission_id}."
    ))
}

async fn call_record_artifact(store: &SharedContextStore, args: &Value) -> Result<String> {
    let mission_id = parse_mission_id(args)?;
    let path = required_str(args, "path")?;
    let change_summary = required_str(args, "change_summary")?;
    let author = optional_str(args, "author", "unknown");
    let role = optional_role(args);
    store
        .record_artifact(mission_id, path.clone(), change_summary, author, role)
        .await?;
    Ok(format!(
        "Recorded artifact change to `{path}` for mission {mission_id}."
    ))
}

async fn call_record_evidence(store: &SharedContextStore, args: &Value) -> Result<String> {
    let mission_id = parse_mission_id(args)?;
    let command = required_str(args, "command")?;
    let result = required_str(args, "result")?;
    let exit_code = args
        .get("exit_code")
        .and_then(Value::as_i64)
        .map(|code| code as i32);
    let author = optional_str(args, "author", "unknown");
    let role = optional_role(args);
    store
        .record_evidence(mission_id, command.clone(), result, exit_code, author, role)
        .await?;
    Ok(format!(
        "Recorded evidence for `{command}` for mission {mission_id}."
    ))
}

fn call_get_mission_context(store: &SharedContextStore, args: &Value) -> Result<String> {
    let mission_id = parse_mission_id(args)?;
    let since = match args.get("since").and_then(Value::as_str) {
        Some(raw) => Some(
            DateTime::parse_from_rfc3339(raw)
                .with_context(|| format!("invalid `since` timestamp `{raw}`"))?
                .with_timezone(&Utc),
        ),
        None => None,
    };
    let context = store.get_mission_context(mission_id, since)?;
    Ok(serde_json::to_string_pretty(&context)?)
}

async fn handle_tool_call(
    store: &SharedContextStore,
    params: Option<Value>,
) -> std::result::Result<Value, (i32, String)> {
    let params = params.ok_or((INVALID_PARAMS, "missing params".to_string()))?;
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or((INVALID_PARAMS, "missing tool name".to_string()))?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let call_result = match name {
        "record_decision" => call_record_decision(store, &arguments).await,
        "record_artifact" => call_record_artifact(store, &arguments).await,
        "record_evidence" => call_record_evidence(store, &arguments).await,
        "get_mission_context" => call_get_mission_context(store, &arguments),
        other => return Err((INVALID_PARAMS, format!("unknown tool: {other}"))),
    };

    Ok(match call_result {
        Ok(text) => json!({ "content": [{ "type": "text", "text": text }] }),
        Err(err) => json!({
            "content": [{ "type": "text", "text": err.to_string() }],
            "isError": true
        }),
    })
}

/// Handles one line of JSON-RPC input, returning the line to write back (if
/// any --- notifications get no response). Exposed separately from
/// [`run_stdio_server`] so tests can drive the protocol without spawning a
/// child process.
pub async fn handle_message(store: &SharedContextStore, line: &str) -> Option<String> {
    let message: IncomingMessage = match serde_json::from_str(line) {
        Ok(message) => message,
        Err(err) => {
            return Some(err_response(Value::Null, PARSE_ERROR, err.to_string()).to_string());
        }
    };

    let Some(id) = message.id else {
        // A JSON-RPC notification (e.g. `notifications/initialized`): no response.
        return None;
    };

    let result = match message.method.as_str() {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION }
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => handle_tool_call(store, message.params).await,
        other => Err((METHOD_NOT_FOUND, format!("unknown method: {other}"))),
    };

    Some(
        match result {
            Ok(value) => ok_response(id, value),
            Err((code, message)) => err_response(id, code, message),
        }
        .to_string(),
    )
}

/// Runs the MCP server loop over stdin/stdout until stdin closes. Each
/// request is handled to completion before the next line is read: Harnesses
/// are not expected to pipeline `tools/call` requests against this server,
/// and sequential handling keeps the sqlite access pattern simple.
pub fn run_stdio_server(store: SharedContextStore) -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line.context("reading a line from stdin")?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = pollster::block_on(handle_message(&store, &line)) {
            writeln!(stdout, "{response}").context("writing a response to stdout")?;
            stdout.flush().context("flushing stdout")?;
        }
    }
    Ok(())
}
