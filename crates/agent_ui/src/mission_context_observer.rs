//! Bridges `AcpThread` events to the Shared Context bus (`crates/shared_context`).
//!
//! Zed observes two kinds of Harness-produced facts directly, without relying
//! on the Harness to self-report them over MCP: completed file-write tool
//! calls (`ToolCallStatus::Completed` with `ToolKind::Edit`/`Delete`/`Move`)
//! become Artifact rows, and finished terminal commands (`ToolKind::Execute`,
//! tracked via `Terminal::wait_for_exit`) become Evidence rows. Both write
//! through the same `SharedContextStore` the `shared-context-mcp` binary
//! reads, called in-process here rather than over MCP.
//!
//! "Decision" facts have no Zed-observable signal --- a Harness's stated
//! intent isn't a state change Zed can see --- so those only ever arrive via
//! the `record_decision` MCP tool.
//!
//! There is no `AcpThreadEvent` variant dedicated to "a tool call completed"
//! or "a terminal command exited": the former must be inferred by inspecting
//! the entry a `NewEntry`/`EntryUpdated(index)` event points at, and the
//! latter has no thread-level event at all, only `Terminal::wait_for_exit`
//! on the terminal entity itself. See [`observe_entry`].

use acp_thread::{AcpThread, AgentThreadEntry, ToolCallContent, ToolCallStatus};
use agent_client_protocol::schema::v1 as acp;
use collections::HashSet;
use gpui::{App, AppContext as _, Entity, Global};
use shared_context::SharedContextStore;

use crate::thread_metadata_store::{ThreadId, ThreadMetadataStore};

/// `author` value used for rows Zed's observer records automatically, as
/// opposed to a Harness calling `record_artifact`/`record_evidence` itself.
///
/// This is now the observer's *only* author value. It used to write the
/// thread's Mission role here instead, which conflated "who recorded this" with
/// "whose work is this" --- the role travels in its own column now.
const OBSERVER_AUTHOR: &str = "zed-observer";

struct GlobalSharedContext(Option<SharedContextStore>);

impl Global for GlobalSharedContext {}

/// Opens the Shared Context store synchronously, matching the precedent set
/// by `crates/db`'s own `open_db` (also opened via a blocking `gpui::block_on`
/// at startup): `ThreadSafeConnectionBuilder::build`'s future carries a
/// `PhantomData<*mut M>` migrator marker that is never `Send`, so it cannot be
/// driven from `cx.background_spawn`, only blocked on directly. The store
/// itself, once open, is `Send + Sync` and freely usable from spawned tasks.
pub fn init(cx: &mut App) {
    if cx.has_global::<GlobalSharedContext>() {
        return;
    }
    let db_path = shared_context::default_db_path();
    let store = match gpui::block_on(SharedContextStore::open(&db_path)) {
        Ok(store) => Some(store),
        Err(err) => {
            log::error!(
                "mission_context_observer: failed to open shared context store at {db_path:?}: {err:#}"
            );
            None
        }
    };
    cx.set_global(GlobalSharedContext(store));
}

/// Exposed crate-wide so other UI (e.g. `mission_panel`) can read from the
/// same process-wide store this observer writes to, without opening a
/// second connection to the Shared Context database.
pub(crate) fn shared_context_store(cx: &App) -> Option<SharedContextStore> {
    cx.try_global::<GlobalSharedContext>()
        .and_then(|global| global.0.clone())
}

/// The Mission a thread belongs to and the Mission role of the worker running
/// in it, if any.
///
/// The role is carried separately from the author (always [`OBSERVER_AUTHOR`]
/// for rows this module writes) so per-worker surfaces have something to filter
/// on that a Harness's own `record_*` calls can also supply. `None` when the
/// thread has no role: unattributed is the honest answer, and better than
/// borrowing the observer's name as a pseudo-role that groups every roleless
/// thread together.
fn mission_context_for_thread(
    thread_id: ThreadId,
    cx: &App,
) -> Option<(shared_context::MissionId, Option<String>)> {
    let store = ThreadMetadataStore::try_global(cx)?;
    let metadata = store.read(cx).entry(thread_id)?;
    let mission_id = metadata.mission_id?;
    let role = metadata
        .role
        .as_ref()
        .map(|role| role.trim())
        .filter(|role| !role.is_empty())
        .map(|role| role.to_string());
    let mission_id =
        shared_context::MissionId::from_key_string(&mission_id.to_key_string()).ok()?;
    Some((mission_id, role))
}

fn strip_code_fence(source: &str) -> String {
    source
        .strip_prefix("```\n")
        .and_then(|s| s.strip_suffix("\n```"))
        .unwrap_or(source)
        .to_string()
}

/// Per-`ConversationView` dedupe state. Without it, a tool call's entry can
/// fire several `EntryUpdated` events after it reaches `Completed` (e.g. from
/// unrelated rendering-state changes), and a terminal-bearing entry can be
/// seen more than once before its wait task would otherwise be spawned --
/// both would otherwise double-write rows.
#[derive(Default)]
pub struct ObserverState {
    recorded_artifact_tool_calls: HashSet<acp::ToolCallId>,
    recorded_evidence_terminals: HashSet<acp::TerminalId>,
}

/// Call from `AcpThreadEvent::NewEntry`/`EntryUpdated(index)` handling, with
/// the entry's index. A cheap no-op when `thread_id` isn't part of a Mission,
/// when the entry at `index` isn't a tool call, or when there is nothing new
/// to record.
pub fn observe_entry(
    state: &mut ObserverState,
    thread_id: ThreadId,
    thread: &Entity<AcpThread>,
    index: usize,
    cx: &App,
) {
    let Some((mission_id, role)) = mission_context_for_thread(thread_id, cx) else {
        return;
    };

    let thread_ref = thread.read(cx);
    let Some(AgentThreadEntry::ToolCall(tool_call)) = thread_ref.entries().get(index) else {
        return;
    };

    if matches!(tool_call.kind, acp::ToolKind::Execute) {
        for content in &tool_call.content {
            let ToolCallContent::Terminal(terminal) = content else {
                continue;
            };
            let terminal_id = terminal.read(cx).id().clone();
            if !state.recorded_evidence_terminals.insert(terminal_id) {
                continue;
            }
            spawn_terminal_evidence_wait(mission_id, role.clone(), terminal.clone(), cx);
        }
        return;
    }

    if !matches!(
        tool_call.kind,
        acp::ToolKind::Edit | acp::ToolKind::Delete | acp::ToolKind::Move
    ) {
        return;
    }
    if !matches!(tool_call.status, ToolCallStatus::Completed) {
        return;
    }
    if !state
        .recorded_artifact_tool_calls
        .insert(tool_call.id.clone())
    {
        return;
    }

    let paths: Vec<String> = tool_call
        .locations
        .iter()
        .map(|location| location.path.display().to_string())
        .collect();
    if paths.is_empty() {
        return;
    }
    let verb = match tool_call.kind {
        acp::ToolKind::Delete => "Deleted",
        acp::ToolKind::Move => "Moved/renamed",
        _ => "Edited",
    };
    let change_summary = format!("{verb} {}", paths.join(", "));

    let Some(store) = shared_context_store(cx) else {
        return;
    };
    cx.background_spawn(async move {
        for path in paths {
            if let Err(err) = store
                .record_artifact(
                    mission_id,
                    path,
                    change_summary.clone(),
                    OBSERVER_AUTHOR.to_string(),
                    role.clone(),
                )
                .await
            {
                log::error!("mission_context_observer: failed to record artifact: {err:#}");
            }
        }
    })
    .detach();
}

fn spawn_terminal_evidence_wait(
    mission_id: shared_context::MissionId,
    role: Option<String>,
    terminal: Entity<acp_thread::Terminal>,
    cx: &App,
) {
    let Some(store) = shared_context_store(cx) else {
        return;
    };
    let wait_for_exit = terminal.read(cx).wait_for_exit();
    cx.spawn(async move |cx| {
        wait_for_exit.await;
        let Some((command, result, exit_code)) = terminal.read_with(cx, |terminal, cx| {
            let output = terminal.output()?;
            let command = strip_code_fence(terminal.command().read(cx).source());
            Some((
                command,
                output.content.clone(),
                output.exit_status.and_then(|status| status.code()),
            ))
        }) else {
            return;
        };

        if let Err(err) = store
            .record_evidence(
                mission_id,
                command,
                result,
                exit_code,
                OBSERVER_AUTHOR.to_string(),
                role,
            )
            .await
        {
            log::error!("mission_context_observer: failed to record evidence: {err:#}");
        }
    })
    .detach();
}
