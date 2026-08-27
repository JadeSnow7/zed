//! Storage for the Shared Context bus: a Mission-scoped record of decisions,
//! artifacts, and evidence that different Harnesses (Claude Code, Codex, Gemini
//! CLI, or Zed's native agent) contribute to and read from via the
//! `shared-context-mcp` stdio MCP server (see [`mcp_server`]).
//!
//! This crate deliberately does not depend on `agent_ui` or `gpui`: it needs to
//! build into a small, standalone binary that external Harness processes can
//! spawn over stdio, and it is written to independently of the mission
//! metadata store `crates/agent_ui/src/thread_metadata_store.rs` uses (which
//! lives inside Zed's main, single-process `db.sqlite`). [`MissionId`] reuses
//! that store's representation (a hyphenated UUID string) so ids round-trip
//! between the two stores, without sharing a type or a database file.

pub mod mcp_server;

use std::path::Path;

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use sqlez::{
    bindable::{Bind, Column, StaticColumnCount},
    domain::Domain,
    statement::Statement,
    thread_safe_connection::ThreadSafeConnection,
};
use sqlez_macros::sql;

/// Maximum number of rows returned per table by [`SharedContextStore::get_mission_context`].
pub const MAX_CONTEXT_ROWS_PER_TABLE: usize = 50;

/// Environment variable that overrides where `shared-context-mcp` opens its
/// database.
///
/// This is load-bearing in production, not just a test hook. `--user-data-dir`
/// is resolved by `paths::set_custom_data_dir` into a process-local `OnceLock`,
/// so a `shared-context-mcp` child started by a Zed running under a custom data
/// directory would otherwise fall back to [`default_db_path`] and quietly open
/// a *different* file than the Zed that spawned it --- two halves of one
/// Mission, writing to two databases. Zed passes this explicitly when it
/// registers the server; see `agent_ui::mission_orchestrator`.
pub const DB_PATH_ENV_VAR: &str = "ZED_SHARED_CONTEXT_DB_PATH";

/// Location of `shared_context.sqlite` for the current process's data
/// directory. Callers that may be a spawned child should prefer
/// [`db_path_from_env`].
pub fn default_db_path() -> std::path::PathBuf {
    paths::database_dir()
        .join("shared_context")
        .join("shared_context.sqlite")
}

/// [`DB_PATH_ENV_VAR`] if set, otherwise [`default_db_path`].
pub fn db_path_from_env() -> std::path::PathBuf {
    match std::env::var(DB_PATH_ENV_VAR) {
        Ok(path) if !path.is_empty() => std::path::PathBuf::from(path),
        _ => default_db_path(),
    }
}

/// Connection setup for `shared_context.sqlite`, mirroring `crates/db`'s
/// `DB_INITIALIZE_QUERY`. Unlike `db.sqlite`, this file is genuinely opened by
/// several processes at once --- Zed's in-process observer plus one
/// `shared-context-mcp` child per external Harness --- so the SQLite defaults
/// are not survivable here:
///
/// - `journal_mode=WAL` lets readers (the Mission panel, `get_mission_context`)
///   run while a writer holds the file. Under the default `DELETE` journal a
///   write transaction blocks readers outright, which reaches the user as a
///   spurious "Shared context is unavailable".
/// - `busy_timeout` covers what WAL does not: WAL still serializes *writers*,
///   so two processes recording at the same moment need one to wait rather than
///   take `SQLITE_BUSY` immediately (the default timeout is 0). Failed writes
///   are logged and dropped, never retried, so this is the difference between a
///   slow write and a lost one.
///
/// Worth knowing before this file ever moves: WAL needs a shared-memory `-shm`
/// mapping beside the database, which network filesystems and some cloud-synced
/// folders do not provide. `paths::database_dir()` is local, so this is safe
/// today; it would not be under a synced directory.
const DB_INITIALIZE_QUERY: &str = sql!(
    PRAGMA journal_mode=WAL;
    PRAGMA busy_timeout=500;
    PRAGMA synchronous=NORMAL;
);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MissionId(uuid::Uuid);

impl MissionId {
    /// Parses the hyphenated UUID string form produced by
    /// `agent_ui::thread_metadata_store::MissionId::to_key_string`.
    pub fn from_key_string(s: &str) -> Result<Self> {
        Ok(Self(
            uuid::Uuid::parse_str(s).context("invalid mission id")?,
        ))
    }

    pub fn to_key_string(&self) -> String {
        self.0.hyphenated().to_string()
    }
}

impl std::fmt::Display for MissionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_key_string())
    }
}

impl StaticColumnCount for MissionId {}

impl Bind for MissionId {
    fn bind(&self, statement: &Statement, start_index: i32) -> Result<i32> {
        self.0.bind(statement, start_index)
    }
}

impl Column for MissionId {
    fn column(statement: &mut Statement, start_index: i32) -> Result<(Self, i32)> {
        let (uuid, next) = Column::column(statement, start_index)?;
        Ok((MissionId(uuid), next))
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Decision {
    pub key: String,
    pub value: String,
    pub author: String,
    /// Mission role of the worker this row belongs to, when known.
    ///
    /// Separate from `author` on purpose. `author` is *who recorded it* --- a
    /// Harness's self-reported name, or `zed-observer` for rows Zed derived
    /// itself --- and that is not the same question as *whose work is this*,
    /// which is what a per-worker view needs to filter on. Collapsing the two
    /// meant a Harness's own `record_*` calls never matched its own dashboard.
    /// `None` for rows written before roles were recorded, and for rows whose
    /// thread has no Mission role.
    pub role: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Artifact {
    pub path: String,
    pub change_summary: String,
    pub author: String,
    /// Mission role of the worker this row belongs to, when known.
    ///
    /// Separate from `author` on purpose. `author` is *who recorded it* --- a
    /// Harness's self-reported name, or `zed-observer` for rows Zed derived
    /// itself --- and that is not the same question as *whose work is this*,
    /// which is what a per-worker view needs to filter on. Collapsing the two
    /// meant a Harness's own `record_*` calls never matched its own dashboard.
    /// `None` for rows written before roles were recorded, and for rows whose
    /// thread has no Mission role.
    pub role: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Evidence {
    pub command: String,
    pub result: String,
    pub exit_code: Option<i32>,
    pub author: String,
    /// Mission role of the worker this row belongs to, when known.
    ///
    /// Separate from `author` on purpose. `author` is *who recorded it* --- a
    /// Harness's self-reported name, or `zed-observer` for rows Zed derived
    /// itself --- and that is not the same question as *whose work is this*,
    /// which is what a per-worker view needs to filter on. Collapsing the two
    /// meant a Harness's own `record_*` calls never matched its own dashboard.
    /// `None` for rows written before roles were recorded, and for rows whose
    /// thread has no Mission role.
    pub role: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MissionContext {
    pub mission_id: String,
    pub decisions: Vec<Decision>,
    pub artifacts: Vec<Artifact>,
    pub evidence: Vec<Evidence>,
}

fn parse_created_at(raw: String) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(&raw)?.with_timezone(&Utc))
}

/// Marker type identifying this crate's migrations to [`ThreadSafeConnection`];
/// never constructed, only used as a type parameter.
struct SharedContextDb;

const ROLE_MIGRATION: &str = r#"
ALTER TABLE decisions ADD COLUMN role TEXT;
ALTER TABLE artifacts ADD COLUMN role TEXT;
ALTER TABLE evidence ADD COLUMN role TEXT;

UPDATE decisions SET role = author WHERE author NOT IN ('zed-observer', 'unknown');
UPDATE artifacts SET role = author WHERE author NOT IN ('zed-observer', 'unknown');
UPDATE evidence SET role = author WHERE author NOT IN ('zed-observer', 'unknown');
"#;

impl Domain for SharedContextDb {
    const NAME: &str = stringify!(SharedContextDb);

    const MIGRATIONS: &[&str] = &[
        sql!(
            CREATE TABLE IF NOT EXISTS decisions(
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                mission_id BLOB NOT NULL,
                key TEXT NOT NULL,
                value TEXT NOT NULL,
                author TEXT NOT NULL,
                created_at TEXT NOT NULL
            ) STRICT;

            CREATE INDEX IF NOT EXISTS decisions_mission_id ON decisions(mission_id);

            CREATE TABLE IF NOT EXISTS artifacts(
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                mission_id BLOB NOT NULL,
                path TEXT NOT NULL,
                change_summary TEXT NOT NULL,
                author TEXT NOT NULL,
                created_at TEXT NOT NULL
            ) STRICT;

            CREATE INDEX IF NOT EXISTS artifacts_mission_id ON artifacts(mission_id);

            CREATE TABLE IF NOT EXISTS evidence(
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                mission_id BLOB NOT NULL,
                command TEXT NOT NULL,
                result TEXT NOT NULL,
                exit_code INTEGER,
                author TEXT NOT NULL,
                created_at TEXT NOT NULL
            ) STRICT;

            CREATE INDEX IF NOT EXISTS evidence_mission_id ON evidence(mission_id);
        ),
        ROLE_MIGRATION,
    ];
}

/// Handle to the Shared Context sqlite file. Cheap to clone (wraps a
/// thread-safe connection pool), safe to hold from both the `shared-context-mcp`
/// binary and, in-process, from `agent_ui`'s observer.
#[derive(Clone)]
pub struct SharedContextStore(ThreadSafeConnection);

impl SharedContextStore {
    /// Opens (creating if necessary) the Shared Context store at `path`,
    /// running any pending migrations. Does not fall back to an in-memory
    /// database on failure: callers depend on this store being durable and
    /// shared across process restarts, so a failure to open it must be
    /// reported rather than silently swallowed.
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating shared context db directory {parent:?}"))?;
        }
        let connection =
            ThreadSafeConnection::builder::<SharedContextDb>(path.to_string_lossy().as_ref(), true)
                .with_db_initialization_query(DB_INITIALIZE_QUERY)
                .build()
                .await
                .with_context(|| format!("opening shared context db at {path:?}"))?;
        Ok(Self(connection))
    }

    pub async fn record_decision(
        &self,
        mission_id: MissionId,
        key: String,
        value: String,
        author: String,
        role: Option<String>,
    ) -> Result<()> {
        let created_at = Utc::now().to_rfc3339();
        self.0
            .write(move |conn| {
                let mut stmt = Statement::prepare(
                    conn,
                    "INSERT INTO decisions(mission_id, key, value, author, role, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                )?;
                let mut i = stmt.bind(&mission_id, 1)?;
                i = stmt.bind(&key, i)?;
                i = stmt.bind(&value, i)?;
                i = stmt.bind(&author, i)?;
                i = stmt.bind(&role, i)?;
                stmt.bind(&created_at, i)?;
                stmt.exec()
            })
            .await
    }

    pub async fn record_artifact(
        &self,
        mission_id: MissionId,
        path: String,
        change_summary: String,
        author: String,
        role: Option<String>,
    ) -> Result<()> {
        let created_at = Utc::now().to_rfc3339();
        self.0
            .write(move |conn| {
                let mut stmt = Statement::prepare(
                    conn,
                    "INSERT INTO artifacts(mission_id, path, change_summary, author, role, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                )?;
                let mut i = stmt.bind(&mission_id, 1)?;
                i = stmt.bind(&path, i)?;
                i = stmt.bind(&change_summary, i)?;
                i = stmt.bind(&author, i)?;
                i = stmt.bind(&role, i)?;
                stmt.bind(&created_at, i)?;
                stmt.exec()
            })
            .await
    }

    pub async fn record_evidence(
        &self,
        mission_id: MissionId,
        command: String,
        result: String,
        exit_code: Option<i32>,
        author: String,
        role: Option<String>,
    ) -> Result<()> {
        let created_at = Utc::now().to_rfc3339();
        self.0
            .write(move |conn| {
                let mut stmt = Statement::prepare(
                    conn,
                    "INSERT INTO evidence(mission_id, command, result, exit_code, author, role, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )?;
                let mut i = stmt.bind(&mission_id, 1)?;
                i = stmt.bind(&command, i)?;
                i = stmt.bind(&result, i)?;
                i = stmt.bind(&exit_code, i)?;
                i = stmt.bind(&author, i)?;
                i = stmt.bind(&role, i)?;
                stmt.bind(&created_at, i)?;
                stmt.exec()
            })
            .await
    }

    /// Returns up to [`MAX_CONTEXT_ROWS_PER_TABLE`] of the most recent rows in
    /// each table for `mission_id`, oldest first, optionally limited to rows
    /// created strictly after `since`.
    ///
    /// `since` is compared as a lower bound against a fixed year-0000 sentinel
    /// rather than `chrono`'s true `DateTime::MIN_UTC` when absent, since the
    /// latter's astronomical-year RFC3339 rendering isn't guaranteed to sort
    /// lexically below every ordinary timestamp.
    pub fn get_mission_context(
        &self,
        mission_id: MissionId,
        since: Option<DateTime<Utc>>,
    ) -> Result<MissionContext> {
        const EPOCH_SENTINEL: &str = "0000-01-01T00:00:00+00:00";
        let since = since
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_else(|| EPOCH_SENTINEL.to_string());

        let decisions_sql = format!(
            "SELECT key, value, author, role, created_at FROM decisions \
             WHERE mission_id = ?1 AND created_at > ?2 \
             ORDER BY created_at DESC LIMIT {MAX_CONTEXT_ROWS_PER_TABLE}"
        );
        let mut decision_rows: Vec<(String, String, String, Option<String>, String)> = self
            .0
            .select_bound::<(MissionId, String), (String, String, String, Option<String>, String)>(
                &decisions_sql,
            )?(
            (
            mission_id,
            since.clone(),
        )
        )?;
        decision_rows.reverse();
        let decisions = decision_rows
            .into_iter()
            .map(|(key, value, author, role, created_at)| {
                Ok(Decision {
                    key,
                    value,
                    author,
                    role,
                    created_at: parse_created_at(created_at)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let artifacts_sql = format!(
            "SELECT path, change_summary, author, role, created_at FROM artifacts \
             WHERE mission_id = ?1 AND created_at > ?2 \
             ORDER BY created_at DESC LIMIT {MAX_CONTEXT_ROWS_PER_TABLE}"
        );
        let mut artifact_rows: Vec<(String, String, String, Option<String>, String)> = self
            .0
            .select_bound::<(MissionId, String), (String, String, String, Option<String>, String)>(
                &artifacts_sql,
            )?(
            (
            mission_id,
            since.clone(),
        )
        )?;
        artifact_rows.reverse();
        let artifacts = artifact_rows
            .into_iter()
            .map(|(path, change_summary, author, role, created_at)| {
                Ok(Artifact {
                    path,
                    change_summary,
                    author,
                    role,
                    created_at: parse_created_at(created_at)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let evidence_sql = format!(
            "SELECT command, result, exit_code, author, role, created_at FROM evidence \
             WHERE mission_id = ?1 AND created_at > ?2 \
             ORDER BY created_at DESC LIMIT {MAX_CONTEXT_ROWS_PER_TABLE}"
        );
        let mut evidence_rows: Vec<(String, String, Option<i32>, String, Option<String>, String)> =
            self.0.select_bound::<
                (MissionId, String),
                (String, String, Option<i32>, String, Option<String>, String),
            >(&evidence_sql)?((mission_id, since))?;
        evidence_rows.reverse();
        let evidence = evidence_rows
            .into_iter()
            .map(|(command, result, exit_code, author, role, created_at)| {
                Ok(Evidence {
                    command,
                    result,
                    exit_code,
                    author,
                    role,
                    created_at: parse_created_at(created_at)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(MissionContext {
            mission_id: mission_id.to_key_string(),
            decisions,
            artifacts,
            evidence,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn open_test_store() -> (SharedContextStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = SharedContextStore::open(&dir.path().join("shared_context.sqlite"))
            .await
            .unwrap();
        (store, dir)
    }

    #[test]
    fn mission_id_round_trips_through_its_key_string() {
        let id = MissionId::from_key_string("2f3d9b0e-6c3e-4b8a-9c3f-1a2b3c4d5e6f").unwrap();
        assert_eq!(id.to_key_string(), "2f3d9b0e-6c3e-4b8a-9c3f-1a2b3c4d5e6f");
        assert!(MissionId::from_key_string("not-a-uuid").is_err());
    }

    #[test]
    fn record_and_read_decisions_artifacts_and_evidence() {
        pollster::block_on(async {
            let (store, _dir) = open_test_store().await;
            let mission =
                MissionId::from_key_string("2f3d9b0e-6c3e-4b8a-9c3f-1a2b3c4d5e6f").unwrap();
            let other_mission =
                MissionId::from_key_string("00000000-0000-0000-0000-000000000000").unwrap();

            store
                .record_decision(
                    mission,
                    "auth-strategy".into(),
                    "use OAuth device flow".into(),
                    "claude-code".into(),
                    Some("coding".into()),
                )
                .await
                .unwrap();
            store
                .record_artifact(
                    mission,
                    "src/auth.rs".into(),
                    "added device flow handler".into(),
                    "zed-observer".into(),
                    Some("coding".into()),
                )
                .await
                .unwrap();
            store
                .record_evidence(
                    mission,
                    "cargo test -p auth".into(),
                    "3 passed; 0 failed".into(),
                    Some(0),
                    "zed-observer".into(),
                    Some("testing".into()),
                )
                .await
                .unwrap();
            // Rows for a different mission must not leak into the first mission's context.
            store
                .record_decision(
                    other_mission,
                    "unrelated".into(),
                    "should not appear".into(),
                    "codex".into(),
                    None,
                )
                .await
                .unwrap();

            let context = store.get_mission_context(mission, None).unwrap();
            assert_eq!(context.mission_id, mission.to_key_string());
            assert_eq!(context.decisions.len(), 1);
            assert_eq!(context.decisions[0].key, "auth-strategy");
            assert_eq!(context.decisions[0].value, "use OAuth device flow");
            assert_eq!(context.decisions[0].author, "claude-code");
            // `author` and `role` answer different questions and are stored
            // separately: a Harness reports its own name, the role says whose
            // work the row is.
            assert_eq!(context.decisions[0].role.as_deref(), Some("coding"));
            assert_eq!(context.evidence[0].author, "zed-observer");
            assert_eq!(context.evidence[0].role.as_deref(), Some("testing"));
            assert_eq!(context.artifacts.len(), 1);
            assert_eq!(context.artifacts[0].path, "src/auth.rs");
            assert_eq!(context.evidence.len(), 1);
            assert_eq!(context.evidence[0].command, "cargo test -p auth");
            assert_eq!(context.evidence[0].exit_code, Some(0));

            let other_context = store.get_mission_context(other_mission, None).unwrap();
            assert_eq!(other_context.decisions.len(), 1);
            assert_eq!(other_context.decisions[0].key, "unrelated");
            assert!(other_context.artifacts.is_empty());
        });
    }

    #[test]
    fn get_mission_context_filters_by_since_and_orders_oldest_first() {
        pollster::block_on(async {
            let (store, _dir) = open_test_store().await;
            let mission =
                MissionId::from_key_string("2f3d9b0e-6c3e-4b8a-9c3f-1a2b3c4d5e6f").unwrap();

            store
                .record_decision(mission, "first".into(), "1".into(), "a".into(), None)
                .await
                .unwrap();
            let cutoff = Utc::now();
            store
                .record_decision(mission, "second".into(), "2".into(), "a".into(), None)
                .await
                .unwrap();
            store
                .record_decision(mission, "third".into(), "3".into(), "a".into(), None)
                .await
                .unwrap();

            let all = store.get_mission_context(mission, None).unwrap();
            assert_eq!(
                all.decisions
                    .iter()
                    .map(|d| d.key.as_str())
                    .collect::<Vec<_>>(),
                vec!["first", "second", "third"]
            );

            let since_cutoff = store.get_mission_context(mission, Some(cutoff)).unwrap();
            assert_eq!(
                since_cutoff
                    .decisions
                    .iter()
                    .map(|d| d.key.as_str())
                    .collect::<Vec<_>>(),
                vec!["second", "third"]
            );
        });
    }

    #[test]
    fn get_mission_context_caps_rows_per_table() {
        pollster::block_on(async {
            let (store, _dir) = open_test_store().await;
            let mission =
                MissionId::from_key_string("2f3d9b0e-6c3e-4b8a-9c3f-1a2b3c4d5e6f").unwrap();

            for i in 0..(MAX_CONTEXT_ROWS_PER_TABLE + 10) {
                store
                    .record_decision(mission, format!("key-{i}"), "v".into(), "a".into(), None)
                    .await
                    .unwrap();
            }

            let context = store.get_mission_context(mission, None).unwrap();
            assert_eq!(context.decisions.len(), MAX_CONTEXT_ROWS_PER_TABLE);
            // Capped to the most recent MAX_CONTEXT_ROWS_PER_TABLE, still oldest-first.
            assert_eq!(context.decisions.first().unwrap().key, "key-10");
            assert_eq!(
                context.decisions.last().unwrap().key,
                format!("key-{}", MAX_CONTEXT_ROWS_PER_TABLE + 9)
            );
        });
    }
}
