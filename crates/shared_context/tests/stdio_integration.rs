//! Spawns the actual `shared-context-mcp` binary and speaks JSON-RPC to it
//! over stdio, the same way an external Harness (Claude Code, Codex, Gemini
//! CLI) would after Zed hands it this server via ACP's `mcp_servers`. This is
//! the test that actually exercises the wire protocol, not just the library
//! functions it's built on.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

struct McpProcess {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    next_id: i64,
}

impl McpProcess {
    // This test's whole point is to exercise the real binary over real pipes,
    // which needs the stdio configuration `smol::process::Command::from()`
    // drops. Blocking here is fine and in fact wanted: the test drives the
    // server one request at a time and asserts on each response, so there is no
    // executor to starve. Same exemption `crates/project`'s integration tests
    // take for spawning helper processes.
    #[allow(clippy::disallowed_methods)]
    fn spawn(db_path: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_shared-context-mcp"))
            .env(shared_context::DB_PATH_ENV_VAR, db_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("failed to spawn shared-context-mcp");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        writeln!(self.stdin, "{request}").expect("writing request to stdin");
        self.stdin.flush().expect("flushing stdin");

        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .expect("reading response from stdout");
        assert!(!line.is_empty(), "server closed stdout without a response");
        let response: Value = serde_json::from_str(&line).expect("response was not valid JSON");
        assert_eq!(response["id"], json!(id));
        response
    }

    fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
    }
}

impl Drop for McpProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn initialize_then_list_tools() {
    let dir = tempfile::tempdir().unwrap();
    let mut proc = McpProcess::spawn(&dir.path().join("shared_context.sqlite"));

    let initialize = proc.request(
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "stdio-integration-test", "version": "0.0.0" }
        }),
    );
    assert_eq!(
        initialize["result"]["serverInfo"]["name"],
        "shared-context-mcp"
    );

    let list = proc.request("tools/list", json!({}));
    let tool_names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .expect("tools/list result should have a tools array")
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        tool_names,
        vec![
            "record_decision",
            "record_artifact",
            "record_evidence",
            "get_mission_context",
        ]
    );
}

#[test]
fn record_decision_then_read_it_back_via_get_mission_context() {
    let dir = tempfile::tempdir().unwrap();
    let mut proc = McpProcess::spawn(&dir.path().join("shared_context.sqlite"));
    let mission_id = "2f3d9b0e-6c3e-4b8a-9c3f-1a2b3c4d5e6f";

    let record = proc.call_tool(
        "record_decision",
        json!({
            "mission_id": mission_id,
            "key": "auth-strategy",
            "value": "use OAuth device flow",
            "author": "claude-code",
            "role": "coding",
        }),
    );
    assert_ne!(record["result"]["isError"], json!(true));

    let read = proc.call_tool("get_mission_context", json!({ "mission_id": mission_id }));
    let text = read["result"]["content"][0]["text"]
        .as_str()
        .expect("get_mission_context should return text content");
    let context: Value = serde_json::from_str(text).expect("context text should be JSON");
    assert_eq!(context["decisions"].as_array().unwrap().len(), 1);
    assert_eq!(context["decisions"][0]["key"], "auth-strategy");
    assert_eq!(context["decisions"][0]["value"], "use OAuth device flow");
    assert_eq!(context["decisions"][0]["author"], "claude-code");
    // `role` has to survive the round trip separately from `author`: it is what
    // puts a Harness's own record on that Harness's worker page.
    assert_eq!(context["decisions"][0]["role"], "coding");
}

#[test]
fn record_artifact_and_evidence_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let mut proc = McpProcess::spawn(&dir.path().join("shared_context.sqlite"));
    let mission_id = "2f3d9b0e-6c3e-4b8a-9c3f-1a2b3c4d5e6f";

    proc.call_tool(
        "record_artifact",
        json!({
            "mission_id": mission_id,
            "path": "src/auth.rs",
            "change_summary": "added device flow handler",
        }),
    );
    proc.call_tool(
        "record_evidence",
        json!({
            "mission_id": mission_id,
            "command": "cargo test -p auth",
            "result": "3 passed; 0 failed",
            "exit_code": 0,
        }),
    );

    let read = proc.call_tool("get_mission_context", json!({ "mission_id": mission_id }));
    let text = read["result"]["content"][0]["text"].as_str().unwrap();
    let context: Value = serde_json::from_str(text).unwrap();
    assert_eq!(context["artifacts"][0]["path"], "src/auth.rs");
    assert_eq!(context["evidence"][0]["command"], "cargo test -p auth");
    assert_eq!(context["evidence"][0]["exit_code"], 0);
}

#[test]
fn record_decision_with_unknown_mission_id_reports_a_tool_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut proc = McpProcess::spawn(&dir.path().join("shared_context.sqlite"));

    let response = proc.call_tool(
        "record_decision",
        json!({ "mission_id": "not-a-uuid", "key": "k", "value": "v" }),
    );
    assert_eq!(response["result"]["isError"], json!(true));
}
