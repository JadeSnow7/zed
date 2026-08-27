//! The Mission sidebar: one Mission at a time, as agents-as-teammates.
//!
//! Shows the selected Mission's workers, the files they have changed, and the
//! Decision/Evidence trail they have recorded to the Shared Context bus
//! (`shared_context`). Each section is a jumping-off point rather than a
//! destination: a worker row opens that worker's runtime dashboard
//! (`worker_dashboard`), a section header opens the corresponding full-page
//! tab (`mission_views`), and the human teammate row opens the queue of things
//! only the user can answer.
//!
//! This panel does not create Missions or threads (that's
//! `mission_orchestrator`) and does not aggregate Mission/thread state (also
//! `mission_orchestrator`, reused here). Activating a worker's conversation
//! calls straight into `AgentPanel::load_agent_thread`, the same entrypoint
//! `crates/sidebar` uses, so no second connection is ever opened.
//!
//! It renders in two places: as the `SidebarView::Mission` view of
//! `crates/sidebar` (the primary surface, which is why the layout follows the
//! sidebar design rather than a dock panel's), and as a dock `Panel` for users
//! who want it beside the thread list. The two are separate instances of the
//! same type, so they cannot disagree about what a Mission looks like.
//!
//! Three different refresh paths, because the three sources differ:
//!
//! - `ThreadMetadataStore` is a GPUI entity and *is* observed, so a worker
//!   created elsewhere appears without the user touching anything. It notifies
//!   on every new entry in every thread, though, which is far more often than
//!   the Mission list actually changes --- so the dock instance gates its
//!   observation on being visible (it refreshes on activation anyway) while the
//!   sidebar instance, which has no activation hook, cannot.
//! - `shared_context` publishes no change events at all, so the Decision and
//!   Evidence trail is pulled: on activation, and when the selected Mission
//!   changes.
//! - The selected Mission's live worker threads are observed directly. "This
//!   worker is blocked on a permission" is useless if it only shows up at the
//!   next refresh.

use acp_thread::AcpThread;
use chrono::{DateTime, Utc};
use client::UserStore;
use collections::{HashMap, HashSet};
use editor::Editor;
use gpui::{
    Action as _, AnyElement, App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle,
    Focusable, IntoElement, ParentElement, Pixels, Render, SharedString, SharedUri, Styled,
    Subscription, Task, WeakEntity, Window, div, px,
};
use project::AgentServerStore;
use ui::{
    Avatar, Color, ContextMenu, Divider, Icon, IconButton, IconName, IconSize, Indicator, Label,
    LabelSize, ListItem, ListItemSpacing, PopoverMenu, Tooltip, prelude::*,
};
use util::ResultExt as _;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::{
    Agent, AgentPanel, AgentThreadSource, CreateMission, MissionState, MissionThreadState,
    conversation_view::ThreadView,
    mission_context_observer::shared_context_store,
    mission_state,
    mission_views::{EvidenceView, MissionQueueView, SharedContextView},
    thread_metadata_store::{Mission, MissionId, ThreadId, ThreadMetadata, ThreadMetadataStore},
    thread_mission_state,
    threads_archive_view::format_history_entry_timestamp,
    worker_dashboard::{PendingPermission, WorkerDashboard, harness_icon, worker_status},
};

const MIN_PANEL_WIDTH: Pixels = px(280.);
const DEFAULT_PANEL_WIDTH: Pixels = px(320.);

/// One Mission and the threads currently tagged with its id.
pub struct MissionGroup {
    pub mission: Mission,
    pub threads: Vec<ThreadMetadata>,
}

/// Missions grouped with their threads, plus threads that don't belong to
/// any Mission (either created before Missions existed, or never assigned
/// one). Ungrouped threads are always kept, never dropped, so the Mission
/// concept never hides pre-existing data.
#[derive(Default)]
pub struct MissionTree {
    pub groups: Vec<MissionGroup>,
    pub ungrouped: Vec<ThreadMetadata>,
}

/// Groups `threads` by `mission_id` under the given `missions`. A thread
/// whose `mission_id` doesn't match any Mission in `missions` (a stale
/// reference to a deleted or not-yet-loaded Mission) still surfaces, in the
/// ungrouped bucket, rather than silently disappearing.
pub fn build_mission_tree(
    missions: Vec<Mission>,
    threads: impl IntoIterator<Item = ThreadMetadata>,
) -> MissionTree {
    let mut by_mission: HashMap<MissionId, Vec<ThreadMetadata>> = HashMap::default();
    let mut ungrouped = Vec::new();
    for thread in threads {
        match thread.mission_id {
            Some(mission_id) => by_mission.entry(mission_id).or_default().push(thread),
            None => ungrouped.push(thread),
        }
    }

    let mut groups = Vec::with_capacity(missions.len());
    for mission in missions {
        let threads = by_mission.remove(&mission.id).unwrap_or_default();
        groups.push(MissionGroup { mission, threads });
    }
    for (_, mut orphaned) in by_mission {
        ungrouped.append(&mut orphaned);
    }

    MissionTree { groups, ungrouped }
}

/// One file a Mission's workers changed, aggregated across all of them.
#[derive(Debug, PartialEq, Eq)]
pub struct MissionChange {
    pub name: String,
    pub lines_added: u32,
    pub lines_removed: u32,
    /// Every worker that touched this file, in the order first seen.
    pub workers: Vec<SharedString>,
}

impl MissionChange {
    /// Two workers editing one file is the situation the Mission sidebar
    /// exists to surface: nothing has gone wrong yet, but the user is the only
    /// one who can decide who owns the file.
    pub fn is_contended(&self) -> bool {
        self.workers.len() > 1
    }
}

/// One worker's edit to one file, as read off its `ActionLog`.
#[derive(Debug)]
pub struct WorkerFileChange {
    pub worker: SharedString,
    pub name: String,
    pub lines_added: u32,
    pub lines_removed: u32,
}

/// Folds each worker's changed files into one per-file view of the Mission,
/// summing line counts and recording which workers touched each file. Sorted
/// by name so the panel doesn't reshuffle between renders as workers report
/// their changes in whatever order `changed_buffers` happens to yield.
pub fn merge_worker_changes(
    changes: impl IntoIterator<Item = WorkerFileChange>,
) -> Vec<MissionChange> {
    let mut by_name: HashMap<String, MissionChange> = HashMap::default();
    for change in changes {
        let entry = by_name
            .entry(change.name.clone())
            .or_insert_with(|| MissionChange {
                name: change.name,
                lines_added: 0,
                lines_removed: 0,
                workers: Vec::new(),
            });
        entry.lines_added += change.lines_added;
        entry.lines_removed += change.lines_removed;
        if !entry.workers.contains(&change.worker) {
            entry.workers.push(change.worker);
        }
    }

    let mut merged: Vec<_> = by_name.into_values().collect();
    merged.sort_by(|a, b| a.name.cmp(&b.name));
    merged
}

/// How many distinct workers authored the Mission's changes. Shown next to the
/// file count so "5 files" reads differently when one worker wrote them all
/// than when four did.
pub fn change_author_count(changes: &[MissionChange]) -> usize {
    let mut authors: HashSet<&SharedString> = HashSet::default();
    for change in changes {
        authors.extend(change.workers.iter());
    }
    authors.len()
}

/// The numbers behind the sidebar's summary strip. Derived from worker states
/// alone so the "N blocked" claim can be tested without live threads.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct MissionCounts {
    pub agents: usize,
    pub blocked: usize,
    pub running: usize,
}

pub fn mission_counts(states: impl IntoIterator<Item = MissionThreadState>) -> MissionCounts {
    let mut counts = MissionCounts::default();
    for state in states {
        counts.agents += 1;
        match state {
            MissionThreadState::Waiting => counts.blocked += 1,
            MissionThreadState::Running => counts.running += 1,
            _ => {}
        }
    }
    counts
}

/// `42.1k` for 42_100, `938` for 938. The design's token counts have to fit a
/// sidebar row and a tab header, so they are always compact.
pub fn format_token_count(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

/// `10m ago`, matching the design. Every timestamp in the Mission surfaces is
/// relative: what matters is whether a decision or a test run is recent
/// relative to the work in flight, not the wall clock it happened at.
pub fn relative_time(timestamp: DateTime<Utc>) -> String {
    format!("{} ago", format_history_entry_timestamp(timestamp))
}

/// Whether a piece of evidence predates the newest artifact recorded for the
/// Mission, i.e. a command that ran before the last edit and therefore no
/// longer proves anything about the current tree.
pub fn evidence_is_stale(
    evidence_recorded_at: DateTime<Utc>,
    newest_artifact_at: Option<DateTime<Utc>>,
) -> bool {
    newest_artifact_at.is_some_and(|artifact_at| artifact_at > evidence_recorded_at)
}

/// What the Context section should show for the currently selected Mission.
#[derive(Default, Clone)]
pub enum MissionContextState {
    #[default]
    NoSelection,
    Loading,
    Empty,
    Populated(shared_context::MissionContext),
    /// The Shared Context store isn't available (failed to open at startup)
    /// or the query itself failed; distinct from `Empty` so the panel can
    /// say "context unavailable" rather than falsely implying the Mission
    /// simply has no recorded decisions/artifacts/evidence yet.
    Unavailable,
}

/// Turns a `get_mission_context` outcome into panel state. Kept separate
/// from IO so the "no records yet" vs. "query failed" distinction can be
/// tested without a real Shared Context store.
pub fn mission_context_state_from_result(
    result: Option<anyhow::Result<shared_context::MissionContext>>,
) -> MissionContextState {
    match result {
        None => MissionContextState::Unavailable,
        Some(Err(_)) => MissionContextState::Unavailable,
        Some(Ok(context)) => {
            if context.decisions.is_empty()
                && context.artifacts.is_empty()
                && context.evidence.is_empty()
            {
                MissionContextState::Empty
            } else {
                MissionContextState::Populated(context)
            }
        }
    }
}

/// Emitted for hosts that own more than this panel. `crates/sidebar` uses it
/// to switch its sidebar slot back to the thread list; the dock panel has
/// nowhere to switch to and ignores it.
pub enum MissionPanelEvent {
    ShowThreadList,
}

/// One row of the Workers section, lifted out of the live thread in a single
/// pass. Rendering from a snapshot keeps the render body from holding a borrow
/// of `cx` through the `AcpThread` while it also needs `&mut Context<Self>`
/// for the permission buttons.
pub struct WorkerRow {
    pub metadata: ThreadMetadata,
    pub thread: Option<Entity<AcpThread>>,
    pub label: SharedString,
    pub harness: SharedString,
    /// What the worker is doing right now: its active tool call, or the fact
    /// that it is waiting on the user. `None` when nothing is in flight.
    pub activity: Option<SharedString>,
    pub state: MissionThreadState,
    pub permission: Option<PendingPermission>,
    pub tokens: Option<u64>,
    pub cost: Option<f64>,
    /// The worker's most recent assistant message, one line, for the standup.
    pub summary: Option<SharedString>,
}

/// The user, rendered as a teammate alongside the agents. There is no
/// persisted record of a human participant in a Mission; this is assembled
/// from the signed-in user and from the work that is currently waiting on
/// them.
pub struct HumanTeammate {
    pub name: SharedString,
    pub initials: SharedString,
    pub avatar: Option<SharedUri>,
    /// Permission prompts plus contended files -- everything in "Asked of you".
    pub to_review: usize,
}

impl HumanTeammate {
    fn new(user_store: Option<&Entity<UserStore>>, to_review: usize, cx: &App) -> Self {
        let user = user_store.and_then(|store| store.read(cx).current_user());
        let name: SharedString = user
            .as_ref()
            .map(|user| {
                user.name
                    .clone()
                    .map(SharedString::from)
                    .unwrap_or_else(|| user.username.clone())
            })
            .unwrap_or_else(|| "You".into());

        Self {
            initials: initials(&name),
            avatar: user.as_ref().map(|user| user.avatar_uri.clone()),
            name,
            to_review,
        }
    }
}

/// Up to two letters for the avatar fallback: the initials of the first two
/// words, or the first two characters of a single-word name.
fn initials(name: &str) -> SharedString {
    let mut words = name.split_whitespace();
    match (words.next(), words.next()) {
        (Some(first), Some(second)) => {
            let mut out = String::new();
            out.extend(first.chars().next());
            out.extend(second.chars().next());
            out.to_uppercase().into()
        }
        (Some(only), None) => only
            .chars()
            .take(2)
            .collect::<String>()
            .to_uppercase()
            .into(),
        _ => "?".into(),
    }
}

/// Everything the sidebar draws for the selected Mission.
pub struct MissionSnapshot {
    pub mission: Option<Mission>,
    pub workers: Vec<WorkerRow>,
    pub changes: Vec<MissionChange>,
    pub human: HumanTeammate,
}

impl MissionSnapshot {
    pub fn counts(&self) -> MissionCounts {
        mission_counts(self.workers.iter().map(|worker| worker.state))
    }

    pub fn contended(&self) -> impl Iterator<Item = &MissionChange> {
        self.changes.iter().filter(|change| change.is_contended())
    }

    pub fn blocked(&self) -> impl Iterator<Item = &WorkerRow> {
        self.workers
            .iter()
            .filter(|worker| worker.permission.is_some())
    }

    pub fn total_tokens(&self) -> u64 {
        self.workers
            .iter()
            .filter_map(|worker| worker.tokens)
            .sum::<u64>()
    }

    /// `None` when no worker reported a cost, so the UI can omit the figure
    /// rather than claim a Mission cost $0.00.
    pub fn total_cost(&self) -> Option<f64> {
        let costs: Vec<f64> = self
            .workers
            .iter()
            .filter_map(|worker| worker.cost)
            .collect();
        if costs.is_empty() {
            None
        } else {
            Some(costs.into_iter().sum())
        }
    }
}

pub struct MissionPanel {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    position: DockPosition,
    tree: MissionTree,
    selected_mission: Option<MissionId>,
    selected_thread: Option<ThreadId>,
    context_state: MissionContextState,
    is_active: bool,
    /// The "New task" popover's editor, kept on the panel so typing survives a
    /// re-render triggered by a worker's state changing underneath it.
    new_task_editor: Entity<Editor>,
    new_task_open: bool,
    /// One per live worker thread of the selected Mission, so status badges and
    /// the attention counts track generation rather than waiting for the next
    /// pull-based refresh. Rebuilt whenever the selected Mission changes.
    _worker_subscriptions: Vec<Subscription>,
    /// Whether [`Self::_metadata_observation`] should do nothing while the
    /// panel is hidden.
    ///
    /// True for the dock panel, which has a `set_active` hook and refreshes on
    /// the way back in, so work done while it is closed costs nothing to skip.
    /// False for the sidebar view, which has no such hook: gating it would mean
    /// never refreshing at all.
    ///
    /// This gate matters more than it looks. `ThreadMetadataStore` is written
    /// on *every* new entry in *every* thread, and each notification here costs
    /// a mission list query, a full tree rebuild, and --- when the selection
    /// changes --- three more Shared Context queries, once per live panel
    /// instance. A hidden panel paying that for a thread the user cannot see is
    /// pure waste.
    refreshes_only_while_active: bool,
    /// The dock panel refreshes when it becomes active; the sidebar view has
    /// no such hook, so both watch `ThreadMetadataStore` instead. Missions and
    /// workers are created through it, so a new worker shows up without the
    /// user having to toggle the view.
    _metadata_observation: Option<Subscription>,
}

/// Re-reads the Mission list whenever `ThreadMetadataStore` changes, unless
/// this panel is hidden and refreshes on activation anyway.
fn observe_metadata_store(
    window: &mut Window,
    cx: &mut Context<MissionPanel>,
) -> Option<Subscription> {
    let store = ThreadMetadataStore::try_global(cx)?;
    Some(cx.observe_in(&store, window, |this, _, window, cx| {
        if this.refreshes_only_while_active && !this.is_active {
            return;
        }
        this.refresh(window, cx);
    }))
}

impl MissionPanel {
    pub fn new(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let new_task_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Describe the task…", window, cx);
            editor
        });

        Self {
            workspace: workspace.weak_handle(),
            focus_handle: cx.focus_handle(),
            position: DockPosition::Left,
            tree: MissionTree::default(),
            selected_mission: None,
            selected_thread: None,
            context_state: MissionContextState::NoSelection,
            is_active: false,
            new_task_editor,
            new_task_open: false,
            _worker_subscriptions: Vec::new(),
            // `set_active` refreshes on the way in, so nothing is lost by
            // staying quiet while closed.
            refreshes_only_while_active: true,
            _metadata_observation: observe_metadata_store(window, cx),
        }
    }

    /// Builds a panel bound to an already-resolved workspace. `crates/sidebar`
    /// uses this: it holds the `MultiWorkspace`'s active workspace, not a
    /// `&mut Workspace` it can take a weak handle from.
    pub fn for_workspace(
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let new_task_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Describe the task…", window, cx);
            editor
        });

        let mut this = Self {
            workspace,
            focus_handle: cx.focus_handle(),
            position: DockPosition::Left,
            tree: MissionTree::default(),
            selected_mission: None,
            selected_thread: None,
            context_state: MissionContextState::NoSelection,
            is_active: true,
            new_task_editor,
            new_task_open: false,
            _worker_subscriptions: Vec::new(),
            // No `set_active` hook here --- the observation is the only way
            // this view ever learns about a new worker.
            refreshes_only_while_active: false,
            _metadata_observation: observe_metadata_store(window, cx),
        };
        this.refresh(window, cx);
        this
    }

    pub fn load(
        workspace: WeakEntity<Workspace>,
        cx: AsyncWindowContext,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        cx.spawn(async move |cx| {
            workspace.update_in(cx, |workspace, window, cx| {
                cx.new(|cx| MissionPanel::new(workspace, window, cx))
            })
        })
    }

    pub fn toggle_focus(
        workspace: &mut Workspace,
        _: &zed_actions::mission_panel::ToggleFocus,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        workspace.toggle_panel_focus::<Self>(window, cx);
    }

    /// Reloads the Mission list and thread metadata. Called when the panel
    /// becomes active and after a Mission is created; see the module-level
    /// doc comment for why this is pull- rather than push-based.
    pub fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(store) = ThreadMetadataStore::try_global(cx) else {
            return;
        };
        let missions_task = store.read(cx).list_missions(cx);
        cx.spawn_in(window, async move |this, cx| {
            let missions = missions_task.await.unwrap_or_default();
            this.update(cx, |this, cx| {
                let Some(store) = ThreadMetadataStore::try_global(cx) else {
                    return;
                };
                let threads: Vec<ThreadMetadata> = store.read(cx).entries().cloned().collect();
                this.tree = build_mission_tree(missions, threads);

                if let Some(mission_id) = this.selected_mission
                    && !this
                        .tree
                        .groups
                        .iter()
                        .any(|group| group.mission.id == mission_id)
                {
                    this.selected_mission = None;
                    this.context_state = MissionContextState::NoSelection;
                }
                // The design shows a Mission, never a Mission picker with
                // nothing picked, so default to the newest one.
                if this.selected_mission.is_none()
                    && let Some(group) = this.tree.groups.first()
                {
                    let mission_id = group.mission.id;
                    this.selected_mission = Some(mission_id);
                    this.refresh_context(mission_id, cx);
                }

                this.sync_worker_subscriptions(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();

        if let Some(mission_id) = self.selected_mission {
            self.refresh_context(mission_id, cx);
        }
    }

    fn refresh_context(&mut self, mission_id: MissionId, cx: &mut Context<Self>) {
        self.context_state = MissionContextState::Loading;
        let Some(store) = shared_context_store(cx) else {
            self.context_state = MissionContextState::Unavailable;
            cx.notify();
            return;
        };
        let mission_key = mission_id.to_key_string();
        let query = cx.background_spawn(async move {
            let shared_mission_id =
                shared_context::MissionId::from_key_string(&mission_key).ok()?;
            Some(store.get_mission_context(shared_mission_id, None))
        });
        cx.spawn(async move |this, cx| {
            let result = query.await;
            this.update(cx, |this, cx| {
                this.context_state = mission_context_state_from_result(result);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn select_mission(&mut self, mission_id: MissionId, cx: &mut Context<Self>) {
        self.selected_mission = Some(mission_id);
        self.new_task_open = false;
        self.refresh_context(mission_id, cx);
        self.sync_worker_subscriptions(cx);
        cx.notify();
    }

    fn selected_group(&self) -> Option<&MissionGroup> {
        let mission_id = self.selected_mission?;
        self.tree
            .groups
            .iter()
            .find(|group| group.mission.id == mission_id)
    }

    fn agent_panel(&self, cx: &App) -> Option<Entity<AgentPanel>> {
        self.workspace.upgrade()?.read(cx).panel::<AgentPanel>(cx)
    }

    fn agent_server_store(&self, cx: &App) -> Option<Entity<AgentServerStore>> {
        Some(
            self.workspace
                .upgrade()?
                .read(cx)
                .project()
                .read(cx)
                .agent_server_store()
                .clone(),
        )
    }

    /// The selected Mission's threads, paired with their live `AcpThread` when
    /// `AgentPanel` currently holds one. A worker with no live thread still
    /// appears -- it just has no runtime state to show.
    fn worker_threads(&self, cx: &App) -> Vec<(ThreadMetadata, Option<Entity<AcpThread>>)> {
        let Some(group) = self.selected_group() else {
            return Vec::new();
        };
        let panel = self.agent_panel(cx);

        group
            .threads
            .iter()
            .map(|thread| {
                let live = panel.as_ref().and_then(|panel| {
                    panel
                        .read(cx)
                        .conversation_view_for_id(&thread.thread_id, cx)?
                        .read(cx)
                        .root_thread(cx)
                });
                (thread.clone(), live)
            })
            .collect()
    }

    /// Re-observes the selected Mission's live worker threads. Without this the
    /// sidebar would only learn about a worker becoming blocked at the next
    /// pull-based refresh, which for "what needs me right now?" is too late.
    fn sync_worker_subscriptions(&mut self, cx: &mut Context<Self>) {
        self._worker_subscriptions = self
            .worker_threads(cx)
            .into_iter()
            .filter_map(|(_, thread)| thread)
            .map(|thread| cx.observe(&thread, |_, _, cx| cx.notify()))
            .collect();
    }

    pub fn snapshot(&self, cx: &App) -> MissionSnapshot {
        match self.selected_group() {
            Some(group) => mission_snapshot(&group.mission, &self.workspace, cx),
            None => MissionSnapshot {
                mission: None,
                workers: Vec::new(),
                changes: Vec::new(),
                human: HumanTeammate::new(
                    self.workspace
                        .upgrade()
                        .map(|workspace| workspace.read(cx).user_store().clone())
                        .as_ref(),
                    0,
                    cx,
                ),
            },
        }
    }

    /// Opens the worker's runtime dashboard as an editor tab. Distinct from
    /// `activate_thread`, which switches the agent panel to its conversation.
    fn open_worker(&mut self, thread: ThreadMetadata, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mission) = self.selected_group().map(|group| group.mission.clone()) else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            WorkerDashboard::deploy(
                mission.id,
                mission.title.clone().into(),
                thread,
                workspace,
                window,
                cx,
            );
        });
    }

    /// Switches the main conversation view to `thread` by calling
    /// `AgentPanel::load_agent_thread` -- the same entrypoint
    /// `crates/sidebar` uses when a thread row is clicked there. This never
    /// creates a thread or a new connection; it only changes which existing
    /// thread `AgentPanel` displays.
    fn activate_thread(
        &mut self,
        thread: ThreadMetadata,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected_thread = Some(thread.thread_id);
        cx.notify();

        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let Some(agent_panel) = workspace.read(cx).panel::<AgentPanel>(cx) else {
            return;
        };
        agent_panel.update(cx, |panel, cx| {
            panel.load_agent_thread(
                Agent::from(thread.agent_id.clone()),
                thread.thread_id,
                Some(thread.folder_paths().clone()),
                thread.title.clone(),
                true,
                AgentThreadSource::MissionPanel,
                window,
                cx,
            );
        });
        workspace.update(cx, |workspace, cx| {
            workspace.focus_panel::<AgentPanel>(window, cx);
        });
    }

    fn open_shared_context(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(mission), Some(workspace)) = (
            self.selected_group().map(|group| group.mission.clone()),
            self.workspace.upgrade(),
        ) else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            SharedContextView::deploy(mission, workspace, window, cx);
        });
    }

    fn open_evidence(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(mission), Some(workspace)) = (
            self.selected_group().map(|group| group.mission.clone()),
            self.workspace.upgrade(),
        ) else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            EvidenceView::deploy(mission, workspace, window, cx);
        });
    }

    fn open_queue(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(mission), Some(workspace)) = (
            self.selected_group().map(|group| group.mission.clone()),
            self.workspace.upgrade(),
        ) else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            MissionQueueView::deploy(mission, workspace, window, cx);
        });
    }

    /// Sends the "New task" text to one worker's thread as an ordinary user
    /// message. Delegation is just a prompt: nothing here bypasses the
    /// worker's own turn-taking.
    fn delegate_task(&mut self, worker: &WorkerRow, window: &mut Window, cx: &mut Context<Self>) {
        let task = self.new_task_editor.read(cx).text(cx).trim().to_string();
        if task.is_empty() {
            return;
        }

        let Some(thread) = worker.thread.clone() else {
            // Without a live thread there is nothing to send to, so open the
            // worker's conversation and leave the text for the user.
            self.activate_thread(worker.metadata.clone(), window, cx);
            return;
        };

        send_to_worker(&thread, task, cx);
        self.new_task_editor
            .update(cx, |editor, cx| editor.clear(window, cx));
        self.new_task_open = false;
        cx.notify();
    }

    fn toggle_new_task(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.new_task_open = !self.new_task_open;
        if self.new_task_open {
            self.new_task_editor
                .read(cx)
                .focus_handle(cx)
                .focus(window, cx);
        }
        cx.notify();
    }

    fn dismiss_new_task(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        if self.new_task_open {
            self.new_task_open = false;
            cx.notify();
        } else {
            cx.propagate();
        }
    }

    fn dispatch_create_mission(window: &mut Window, cx: &mut App) {
        window.dispatch_action(CreateMission.boxed_clone(), cx);
    }
}

// --- rendering ---

impl MissionPanel {
    fn render_header(&self, snapshot: &MissionSnapshot, cx: &mut Context<Self>) -> AnyElement {
        let title: SharedString = snapshot
            .mission
            .as_ref()
            .map(|mission| SharedString::from(mission.title.clone()))
            .unwrap_or_else(|| "No Mission".into());
        let state = snapshot.mission.as_ref().and_then(|mission| {
            let panel = self.agent_panel(cx)?;
            Some(mission_state(panel.read(cx), mission.id, cx))
        });
        let missions: Vec<(MissionId, SharedString)> = self
            .tree
            .groups
            .iter()
            .map(|group| (group.mission.id, group.mission.title.clone().into()))
            .collect();
        let selected = self.selected_mission;
        let ungrouped = self.tree.ungrouped.len();

        h_flex()
            .id("mission-panel-header")
            .w_full()
            .h(px(32.))
            .flex_none()
            .items_center()
            .gap_1p5()
            .pl_2p5()
            .pr_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                Indicator::dot()
                    .color(state.map_or(Color::Muted, |state| mission_state_color(state))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .child(Label::new(title).truncate()),
            )
            .child(
                PopoverMenu::new("mission-switcher")
                    .trigger(
                        IconButton::new("mission-switcher-trigger", IconName::ChevronDown)
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text("Switch Mission")),
                    )
                    .menu({
                        let panel = cx.entity().downgrade();
                        move |window, cx| {
                            let missions = missions.clone();
                            let panel = panel.clone();
                            Some(ContextMenu::build(
                                window,
                                cx,
                                move |mut menu, _window, _cx| {
                                    for (mission_id, title) in missions {
                                        let is_selected = selected == Some(mission_id);
                                        let panel = panel.clone();
                                        menu = menu.toggleable_entry(
                                            title,
                                            is_selected,
                                            ui::IconPosition::Start,
                                            None,
                                            move |_window, cx| {
                                                panel
                                                    .update(cx, |this, cx| {
                                                        this.select_mission(mission_id, cx);
                                                    })
                                                    .ok();
                                            },
                                        );
                                    }
                                    menu = menu.separator();
                                    if ungrouped > 0 {
                                        menu = menu.entry(
                                            format!("{ungrouped} threads outside any Mission"),
                                            None,
                                            |window, cx| {
                                                window.dispatch_action(
                                                    ShowThreadList.boxed_clone(),
                                                    cx,
                                                );
                                            },
                                        );
                                    }
                                    menu.entry("New Mission…", None, |window, cx| {
                                        Self::dispatch_create_mission(window, cx);
                                    })
                                },
                            ))
                        }
                    }),
            )
            .child(
                IconButton::new("mission-new-task", IconName::Plus)
                    .icon_size(IconSize::XSmall)
                    .icon_color(if self.new_task_open {
                        Color::Accent
                    } else {
                        Color::Muted
                    })
                    .tooltip(Tooltip::text("Delegate a task to a worker"))
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_new_task(window, cx))),
            )
            .child(
                PopoverMenu::new("mission-overflow")
                    .trigger(
                        IconButton::new("mission-overflow-trigger", IconName::Ellipsis)
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted),
                    )
                    .menu(move |window, cx| {
                        Some(ContextMenu::build(window, cx, |menu, _window, _cx| {
                            menu.entry("New Mission…", None, |window, cx| {
                                Self::dispatch_create_mission(window, cx);
                            })
                            .separator()
                            .entry(
                                "Show thread list",
                                None,
                                |window, cx| {
                                    window.dispatch_action(ShowThreadList.boxed_clone(), cx);
                                },
                            )
                        }))
                    }),
            )
            .into_any_element()
    }

    /// The design's popover: a task description plus the teammate it goes to.
    fn render_new_task_popover(
        &self,
        snapshot: &MissionSnapshot,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut assignees = v_flex().gap_px();
        for (index, worker) in snapshot.workers.iter().enumerate() {
            let metadata = worker.metadata.clone();
            let harness = worker.harness.clone();
            assignees = assignees.child(
                ListItem::new(SharedString::from(format!("mission-assign-{index}")))
                    .spacing(ListItemSpacing::Sparse)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let Some(worker) = this
                            .snapshot(cx)
                            .workers
                            .into_iter()
                            .find(|row| row.metadata.thread_id == metadata.thread_id)
                        else {
                            return;
                        };
                        this.delegate_task(&worker, window, cx);
                    }))
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .child(Label::new(worker.label.clone()).size(LabelSize::Small))
                            .child(div().flex_1())
                            .child(
                                Label::new(harness)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    ),
            );
        }

        let human_name = snapshot.human.name.clone();
        assignees = assignees.child(
            ListItem::new("mission-assign-human")
                .spacing(ListItemSpacing::Sparse)
                .on_click(cx.listener(|this, _, window, cx| this.open_queue(window, cx)))
                .child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .child(Label::new(human_name).size(LabelSize::Small))
                        .child(div().flex_1())
                        .child(
                            Label::new("online")
                                .size(LabelSize::XSmall)
                                .color(Color::Success),
                        ),
                ),
        );
        assignees = assignees.child(
            ListItem::new("mission-assign-new-worker")
                .spacing(ListItemSpacing::Sparse)
                .on_click(cx.listener(|this, _, window, cx| {
                    this.new_task_open = false;
                    cx.notify();
                    let Some(mission) = this.selected_group().map(|group| group.mission.clone())
                    else {
                        return;
                    };
                    crate::mission_orchestrator::add_worker_to_mission(
                        mission,
                        this.workspace.clone(),
                        window,
                        cx,
                    );
                }))
                .child(
                    Label::new("New worker…")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
        );

        v_flex()
            .absolute()
            .top(px(30.))
            .right(px(6.))
            .w(px(246.))
            .p_1p5()
            .gap_0p5()
            .elevation_2(cx)
            .occlude()
            .on_action(cx.listener(Self::dismiss_new_task))
            .child(bilingual_label("NEW TASK", "新建任务"))
            .child(
                div()
                    .w_full()
                    .px_1p5()
                    .py_1()
                    .rounded_sm()
                    .bg(cx.theme().colors().editor_background)
                    .child(self.new_task_editor.clone()),
            )
            .child(bilingual_label("ASSIGN TO", "指派给"))
            .child(assignees)
            .into_any_element()
    }

    fn render_summary(&self, snapshot: &MissionSnapshot, cx: &mut Context<Self>) -> AnyElement {
        let counts = snapshot.counts();
        let agents = format!(
            "{} {}",
            counts.agents,
            if counts.agents == 1 {
                "agent"
            } else {
                "agents"
            }
        );

        h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap_1()
            .px_2p5()
            .py_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                Label::new(agents)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                Label::new("·")
                    .size(LabelSize::XSmall)
                    .color(Color::Disabled),
            )
            .child(
                Label::new("1 teammate")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(div().flex_1())
            .when(counts.blocked > 0, |this| {
                this.child(
                    Label::new(format!("{} blocked", counts.blocked))
                        .size(LabelSize::XSmall)
                        .color(Color::Warning),
                )
            })
            .into_any_element()
    }

    fn render_workers(&self, snapshot: &MissionSnapshot, cx: &mut Context<Self>) -> AnyElement {
        let store = self.agent_server_store(cx);
        let mut section = v_flex().w_full().child(section_header(
            "WORKERS",
            "工作单元",
            Some("harness · role".into()),
        ));

        for worker in &snapshot.workers {
            let metadata = worker.metadata.clone();
            let (status_label, status_color) = worker_status(worker.state);
            let subtitle = match &worker.activity {
                Some(activity) => format!("{} · {}", worker.harness, activity),
                None => worker.harness.to_string(),
            };

            section = section.child(
                ListItem::new(SharedString::from(format!(
                    "mission-worker-{}",
                    worker.metadata.thread_id.to_key_string()
                )))
                .selectable(true)
                .focused(self.selected_thread == Some(worker.metadata.thread_id))
                .spacing(ListItemSpacing::Sparse)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_worker(metadata.clone(), window, cx);
                }))
                .child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .items_start()
                        .child(harness_icon(&worker.metadata.agent_id, store.as_ref(), cx))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(Label::new(worker.label.clone()).truncate())
                                .child(
                                    Label::new(subtitle)
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted)
                                        .truncate(),
                                ),
                        )
                        .child(
                            h_flex()
                                .flex_none()
                                .gap_1()
                                .items_center()
                                .child(Indicator::dot().color(status_color))
                                .child(
                                    Label::new(status_label)
                                        .size(LabelSize::XSmall)
                                        .color(status_color),
                                ),
                        ),
                ),
            );
        }

        section = section.child(self.render_human_row(snapshot, cx));

        if snapshot.workers.is_empty() {
            section = section.child(empty_state_label(
                "No workers yet. Create a Mission to add some.",
            ));
        }

        section.into_any_element()
    }

    fn render_human_row(&self, snapshot: &MissionSnapshot, cx: &mut Context<Self>) -> AnyElement {
        let human = &snapshot.human;
        let to_review = human.to_review;

        ListItem::new("mission-human-teammate")
            .selectable(true)
            .spacing(ListItemSpacing::Sparse)
            .on_click(cx.listener(|this, _, window, cx| this.open_queue(window, cx)))
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_start()
                    .child(match &human.avatar {
                        Some(uri) => Avatar::new(uri.clone()).size(px(16.)).into_any_element(),
                        None => avatar_fallback(human.initials.clone(), cx),
                    })
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(Label::new(human.name.clone()).truncate())
                                    .child(
                                        Label::new("· you")
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    ),
                            )
                            .child(
                                Label::new("Human · owner & reviewer")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .truncate(),
                            ),
                    )
                    .when(to_review > 0, |this| {
                        this.child(
                            h_flex()
                                .flex_none()
                                .gap_1()
                                .items_center()
                                .child(Indicator::dot().color(Color::Warning))
                                .child(
                                    Label::new(format!("{to_review} to review"))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Warning),
                                ),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_changes(&self, snapshot: &MissionSnapshot, cx: &mut Context<Self>) -> AnyElement {
        if snapshot.changes.is_empty() {
            return div().into_any_element();
        }

        let authors = change_author_count(&snapshot.changes);
        let meta = format!(
            "{} {} · {} {}",
            snapshot.changes.len(),
            if snapshot.changes.len() == 1 {
                "file"
            } else {
                "files"
            },
            authors,
            if authors == 1 { "author" } else { "authors" }
        );

        let mut section =
            v_flex()
                .w_full()
                .child(section_header("CHANGES", "变更", Some(meta.into())));

        for change in &snapshot.changes {
            let workers = change.workers.join(" and ");
            let thread = change.workers.first().and_then(|author| {
                snapshot
                    .workers
                    .iter()
                    .find(|worker| &worker.label == author)?
                    .thread
                    .clone()
            });
            section = section.child(
                ListItem::new(SharedString::from(format!(
                    "mission-change-{}",
                    change.name
                )))
                .spacing(ListItemSpacing::Sparse)
                .tooltip(Tooltip::text(format!("Changed by {workers}")))
                .on_click(cx.listener(move |this, _, window, cx| {
                    let (Some(thread), Some(workspace)) =
                        (thread.clone(), this.workspace.upgrade())
                    else {
                        return;
                    };
                    workspace.update(cx, |workspace, cx| {
                        crate::AgentDiffPane::deploy_in_workspace(thread, workspace, window, cx);
                    });
                }))
                .child(
                    h_flex()
                        .w_full()
                        .gap_1p5()
                        .items_center()
                        .child(
                            Label::new(change.name.clone())
                                .size(LabelSize::Small)
                                .truncate(),
                        )
                        .when(change.is_contended(), |this| {
                            this.child(
                                Icon::new(IconName::GitMergeConflict)
                                    .size(IconSize::XSmall)
                                    .color(Color::Warning),
                            )
                        })
                        .child(div().flex_1())
                        .child(
                            Label::new(format!("+{}", change.lines_added))
                                .size(LabelSize::XSmall)
                                .color(Color::Created),
                        )
                        .child(
                            Label::new(format!("−{}", change.lines_removed))
                                .size(LabelSize::XSmall)
                                .color(Color::Deleted),
                        ),
                ),
            );
        }

        section.into_any_element()
    }

    fn render_shared_context(&self, cx: &mut Context<Self>) -> AnyElement {
        let context = match &self.context_state {
            MissionContextState::NoSelection => {
                return empty_section("SHARED CONTEXT", "共享上下文", "Select a Mission.");
            }
            MissionContextState::Loading => {
                return empty_section("SHARED CONTEXT", "共享上下文", "Loading…");
            }
            MissionContextState::Unavailable => {
                return empty_section(
                    "SHARED CONTEXT",
                    "共享上下文",
                    "Shared context is unavailable.",
                );
            }
            MissionContextState::Empty => {
                return empty_section("SHARED CONTEXT", "共享上下文", "Nothing recorded yet.");
            }
            MissionContextState::Populated(context) => context,
        };

        let newest_artifact = context
            .artifacts
            .iter()
            .map(|artifact| artifact.created_at)
            .max();

        let mut decisions = v_flex().w_full().child(
            h_flex()
                .id("mission-decisions-header")
                .w_full()
                .gap_1p5()
                .px_2p5()
                .py_1()
                .items_baseline()
                .cursor_pointer()
                .on_click(cx.listener(|this, _, window, cx| this.open_shared_context(window, cx)))
                .child(Label::new("Decisions").size(LabelSize::Small))
                .child(
                    Label::new(context.decisions.len().to_string())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(div().flex_1())
                .child(
                    Icon::new(IconName::ArrowRight)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                ),
        );
        for decision in context.decisions.iter().rev().take(3) {
            decisions = decisions.child(
                v_flex()
                    .px_2p5()
                    .pb_1()
                    .child(Label::new(decision.value.clone()).size(LabelSize::Small))
                    .child(
                        Label::new(format!(
                            "{} · {}",
                            // The role names the worker; `author` only names
                            // whatever recorded the row. Prefer the former when
                            // it is there. See `shared_context::Decision::role`.
                            decision.role.as_deref().unwrap_or(&decision.author),
                            relative_time(decision.created_at)
                        ))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    ),
            );
        }

        let mut evidence = v_flex().w_full().child(
            h_flex()
                .id("mission-evidence-header")
                .w_full()
                .gap_1p5()
                .px_2p5()
                .py_1()
                .items_baseline()
                .cursor_pointer()
                .on_click(cx.listener(|this, _, window, cx| this.open_evidence(window, cx)))
                .child(Label::new("Evidence").size(LabelSize::Small))
                .child(
                    Label::new("验证证据")
                        .size(LabelSize::XSmall)
                        .color(Color::Disabled),
                )
                .child(div().flex_1())
                .child(
                    Label::new(context.evidence.len().to_string())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
        );
        for row in context.evidence.iter().rev().take(3) {
            let stale = evidence_is_stale(row.created_at, newest_artifact);
            evidence = evidence.child(
                h_flex()
                    .w_full()
                    .gap_1p5()
                    .px_2p5()
                    .pb_1()
                    .items_center()
                    .child(
                        Label::new(row.command.clone())
                            .size(LabelSize::Small)
                            .buffer_font(cx)
                            .truncate(),
                    )
                    .child(div().flex_1())
                    .child(
                        Label::new(if stale {
                            "stale".to_string()
                        } else {
                            relative_time(row.created_at)
                        })
                        .size(LabelSize::XSmall)
                        .color(if stale {
                            Color::Warning
                        } else {
                            Color::Muted
                        }),
                    ),
            );
        }

        v_flex()
            .w_full()
            .child(section_header("SHARED CONTEXT", "共享上下文", None))
            .child(decisions)
            .child(evidence)
            .into_any_element()
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> AnyElement {
        let count = self.tree.groups.len().saturating_sub(1);

        h_flex()
            .id("mission-past-missions")
            .w_full()
            .flex_none()
            .items_center()
            .px_2p5()
            .py_1p5()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .cursor_pointer()
            .on_click(cx.listener(|_, _, window, cx| {
                window.dispatch_action(ShowThreadList.boxed_clone(), cx);
            }))
            .child(
                Label::new("Past missions")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(div().flex_1())
            .child(
                Label::new(count.to_string())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .into_any_element()
    }
}

/// Everything a Mission surface draws, lifted out of the live threads in one
/// pass. A free function rather than a method so the sidebar, the Mission
/// queue tab, and the status bar item all read the same Mission the same way.
pub fn mission_snapshot(
    mission: &Mission,
    workspace: &WeakEntity<Workspace>,
    cx: &App,
) -> MissionSnapshot {
    let Some(workspace_entity) = workspace.upgrade() else {
        return MissionSnapshot {
            mission: Some(mission.clone()),
            workers: Vec::new(),
            changes: Vec::new(),
            human: HumanTeammate::new(None, 0, cx),
        };
    };
    let panel = workspace_entity.read(cx).panel::<AgentPanel>(cx);
    let store = workspace_entity
        .read(cx)
        .project()
        .read(cx)
        .agent_server_store()
        .clone();
    let metadata_store = ThreadMetadataStore::try_global(cx);

    let threads: Vec<ThreadMetadata> = metadata_store
        .map(|store| {
            store
                .read(cx)
                .entries()
                .filter(|metadata| metadata.mission_id == Some(mission.id))
                .cloned()
                .collect()
        })
        .unwrap_or_default();

    let workers: Vec<WorkerRow> = threads
        .into_iter()
        .map(|metadata| {
            let thread = panel.as_ref().and_then(|panel| {
                panel
                    .read(cx)
                    .conversation_view_for_id(&metadata.thread_id, cx)?
                    .read(cx)
                    .root_thread(cx)
            });
            let state = panel
                .as_ref()
                .map(|panel| thread_mission_state(panel.read(cx), metadata.thread_id, cx))
                .unwrap_or(MissionThreadState::Created);
            let harness = store
                .read(cx)
                .agent_display_name(&metadata.agent_id)
                .unwrap_or_else(|| Agent::from(metadata.agent_id.clone()).label());
            let permission = thread
                .as_ref()
                .and_then(|thread| PendingPermission::for_thread(thread.read(cx), cx));
            let activity = if permission.is_some() {
                Some(SharedString::from("awaiting approval"))
            } else {
                thread.as_ref().and_then(|thread| {
                    crate::worker_dashboard::active_tool_call_label(thread.read(cx), cx)
                })
            };

            WorkerRow {
                label: worker_label(&metadata),
                harness,
                activity,
                state,
                permission,
                tokens: thread
                    .as_ref()
                    .and_then(|thread| Some(thread.read(cx).token_usage()?.used_tokens)),
                cost: thread
                    .as_ref()
                    .and_then(|thread| Some(thread.read(cx).cost()?.amount)),
                summary: thread
                    .as_ref()
                    .and_then(|thread| last_assistant_summary(thread.read(cx), cx)),
                metadata,
                thread,
            }
        })
        .collect();

    let changes = merge_worker_changes(workers.iter().flat_map(|worker| {
        let Some(thread) = &worker.thread else {
            return Vec::new();
        };
        thread
            .read(cx)
            .action_log()
            .read(cx)
            .changed_buffers(cx)
            .filter_map(|(buffer, diff)| {
                let path = buffer.read(cx).file()?.path().clone();
                let stats = action_log::DiffStats::single_file(diff.read(cx));
                Some(WorkerFileChange {
                    worker: worker.label.clone(),
                    name: path
                        .file_name()
                        .unwrap_or_else(|| path.as_unix_str())
                        .to_string(),
                    lines_added: stats.lines_added,
                    lines_removed: stats.lines_removed,
                })
            })
            .collect()
    }));

    let to_review = workers
        .iter()
        .filter(|worker| worker.permission.is_some())
        .count()
        + changes
            .iter()
            .filter(|change| change.is_contended())
            .count();

    MissionSnapshot {
        mission: Some(mission.clone()),
        workers,
        changes,
        human: HumanTeammate::new(Some(workspace_entity.read(cx).user_store()), to_review, cx),
    }
}

/// Sends `message` to a worker's thread as an ordinary user turn.
/// `AcpThread::send` hands back a bare future rather than a `Task`, so it has
/// to be driven somewhere; every caller here is a fire-and-forget button, so
/// a failure is logged rather than propagated.
pub fn send_to_worker(thread: &Entity<AcpThread>, message: String, cx: &mut App) {
    let send = thread.update(cx, |thread, cx| thread.send(vec![message.into()], cx));
    cx.background_spawn(async move {
        send.await.log_err();
    })
    .detach();
}

/// Sends a Worker Dashboard instruction without interrupting a turn already in
/// progress. Idle threads take the direct path; generating threads use the
/// same FIFO queue as the main agent composer.
pub fn send_to_worker_view(
    thread_view: &Entity<ThreadView>,
    message: String,
    window: &mut Window,
    cx: &mut App,
) -> Task<anyhow::Result<()>> {
    let is_idle = thread_view.read(cx).thread.read(cx).status() == acp_thread::ThreadStatus::Idle;
    if is_idle {
        let send = thread_view.update(cx, |view, cx| {
            view.thread
                .update(cx, |thread, cx| thread.send(vec![message.into()], cx))
        });
        cx.background_spawn(async move { send.await.map(|_| ()) })
    } else {
        thread_view.update(cx, |view, cx| {
            view.add_to_queue(vec![message.into()], Vec::new(), window, cx);
        });
        Task::ready(Ok(()))
    }
}

/// How a worker is named across the sidebar and its dashboard tab: its
/// Mission role when it has one, its thread title otherwise.
pub fn worker_label(metadata: &ThreadMetadata) -> SharedString {
    metadata
        .role
        .clone()
        .map(SharedString::from)
        .unwrap_or_else(|| metadata.display_title())
}

/// The worker's most recent assistant message, collapsed to a single line for
/// the standup and the worker tab's summary strip.
pub fn last_assistant_summary(thread: &AcpThread, cx: &App) -> Option<SharedString> {
    use acp_thread::AgentThreadEntry;

    let message = thread
        .entries()
        .iter()
        .rev()
        .find_map(|entry| match entry {
            AgentThreadEntry::AssistantMessage(message) => Some(message),
            _ => None,
        })?;

    let text = message.to_markdown(cx);
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))?;
    Some(SharedString::from(line.to_string()))
}

/// The design pairs every section header with a Chinese subtitle and an
/// optional right-aligned count. Defined once so the pairing can't drift
/// between the sidebar and the full-page tabs.
pub fn section_header(
    english: &'static str,
    chinese: &'static str,
    meta: Option<SharedString>,
) -> impl IntoElement {
    h_flex()
        .w_full()
        .items_baseline()
        .gap_1p5()
        .px_2p5()
        .pt_2()
        .pb_0p5()
        .child(
            Label::new(english)
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
        .child(
            Label::new(chinese)
                .size(LabelSize::XSmall)
                .color(Color::Disabled),
        )
        .child(div().flex_1())
        .children(meta.map(|meta| Label::new(meta).size(LabelSize::XSmall).color(Color::Muted)))
}

/// The bilingual label without the section chrome, for popovers and headers.
pub fn bilingual_label(english: &'static str, chinese: &'static str) -> impl IntoElement {
    h_flex()
        .items_baseline()
        .gap_1p5()
        .px_1p5()
        .py_0p5()
        .child(
            Label::new(english)
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
        .child(
            Label::new(chinese)
                .size(LabelSize::XSmall)
                .color(Color::Disabled),
        )
}

fn empty_section(
    english: &'static str,
    chinese: &'static str,
    message: &'static str,
) -> AnyElement {
    v_flex()
        .w_full()
        .child(section_header(english, chinese, None))
        .child(empty_state_label(message))
        .into_any_element()
}

fn empty_state_label(text: &'static str) -> AnyElement {
    div()
        .px_2p5()
        .py_1()
        .child(Label::new(text).size(LabelSize::Small).color(Color::Muted))
        .into_any_element()
}

/// A monogram stand-in for a user with no avatar, so the human teammate row
/// keeps the same shape whether or not the user is signed in.
fn avatar_fallback(initials: SharedString, cx: &App) -> AnyElement {
    div()
        .size(px(16.))
        .flex_none()
        .rounded_full()
        .bg(cx.theme().colors().element_background)
        .flex()
        .items_center()
        .justify_center()
        .child(
            Label::new(initials)
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
        .into_any_element()
}

pub fn mission_state_color(state: MissionState) -> Color {
    match state {
        MissionState::Created => Color::Muted,
        MissionState::Running => Color::Info,
        MissionState::Waiting => Color::Warning,
        MissionState::Completed => Color::Success,
        MissionState::Failed => Color::Error,
    }
}

gpui::actions!(
    mission_panel,
    [
        /// Switches the Mission sidebar back to the thread list.
        ShowThreadList,
    ]
);

impl EventEmitter<PanelEvent> for MissionPanel {}
impl EventEmitter<MissionPanelEvent> for MissionPanel {}

impl Focusable for MissionPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for MissionPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Read the Mission's workers and their changed files once: every
        // section below needs them, and walking each worker's changed buffers
        // per section would be the same work repeated.
        let snapshot = self.snapshot(cx);
        let new_task = self
            .new_task_open
            .then(|| self.render_new_task_popover(&snapshot, cx));

        v_flex()
            .id("mission-panel")
            .key_context("MissionPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .relative()
            .bg(cx.theme().colors().panel_background)
            .on_action(cx.listener(|_, _: &ShowThreadList, _window, cx| {
                cx.emit(MissionPanelEvent::ShowThreadList);
            }))
            .child(self.render_header(&snapshot, cx))
            .child(self.render_summary(&snapshot, cx))
            .child(
                v_flex()
                    .id("mission-panel-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(self.render_workers(&snapshot, cx))
                    .child(Divider::horizontal())
                    .child(self.render_changes(&snapshot, cx))
                    .child(self.render_shared_context(cx)),
            )
            .child(self.render_footer(cx))
            .children(new_task)
    }
}

impl Panel for MissionPanel {
    fn persistent_name() -> &'static str {
        "MissionPanel"
    }

    fn panel_key() -> &'static str {
        "MissionPanel"
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        position != DockPosition::Bottom
    }

    fn set_position(
        &mut self,
        position: DockPosition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        DEFAULT_PANEL_WIDTH
    }

    fn min_size(&self, _window: &Window, _cx: &App) -> Option<Pixels> {
        Some(MIN_PANEL_WIDTH)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::ListTodo)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Missions")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(zed_actions::mission_panel::ToggleFocus)
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.is_active = active;
        if active {
            self.refresh(window, cx);
        }
    }

    fn activation_priority(&self) -> u32 {
        8
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::test_support;
    use crate::thread_metadata_store::WorktreePaths;
    use acp_thread::StubAgentConnection;
    use gpui::{TestAppContext, VisualTestContext};
    use project::{FakeFs, Project};
    use serde_json::json;
    use std::path::Path;
    use workspace::MultiWorkspace;

    fn mission(title: &str) -> Mission {
        Mission {
            id: MissionId::new(),
            title: title.to_string(),
            created_at: chrono::Utc::now(),
        }
    }

    fn thread_with_mission(mission_id: Option<MissionId>, role: Option<&str>) -> ThreadMetadata {
        ThreadMetadata {
            thread_id: ThreadId::new(),
            session_id: None,
            agent_id: "Test".into(),
            title: Some("A thread".into()),
            title_override: None,
            updated_at: chrono::Utc::now(),
            created_at: Some(chrono::Utc::now()),
            interacted_at: None,
            worktree_paths: WorktreePaths::from_folder_paths(&util::path_list::PathList::default()),
            remote_connection: None,
            archived: false,
            mission_id,
            role: role.map(|role| role.to_string()),
        }
    }

    fn change(worker: &str, name: &str, added: u32, removed: u32) -> WorkerFileChange {
        WorkerFileChange {
            worker: worker.into(),
            name: name.to_string(),
            lines_added: added,
            lines_removed: removed,
        }
    }

    #[test]
    fn one_worker_per_file_is_never_contended() {
        let merged = merge_worker_changes([
            change("Implementation", "mission_panel.rs", 142, 31),
            change("Test", "thread_metadata_store.rs", 34, 8),
        ]);

        assert_eq!(merged.len(), 2);
        assert!(merged.iter().all(|change| !change.is_contended()));
        // Sorted by name, so the panel doesn't reshuffle between renders.
        assert_eq!(merged[0].name, "mission_panel.rs");
        assert_eq!(merged[1].name, "thread_metadata_store.rs");
    }

    #[test]
    fn two_workers_on_one_file_contend_and_their_line_counts_sum() {
        let merged = merge_worker_changes([
            change("Implementation", "mission_panel.rs", 142, 31),
            change("Review", "mission_panel.rs", 8, 2),
            change("Review", "shared_context.rs", 21, 6),
        ]);

        assert_eq!(merged.len(), 2);
        let contended = &merged[0];
        assert_eq!(contended.name, "mission_panel.rs");
        assert!(contended.is_contended());
        assert_eq!(contended.workers, vec!["Implementation", "Review"]);
        assert_eq!(contended.lines_added, 150);
        assert_eq!(contended.lines_removed, 33);
        assert!(!merged[1].is_contended());
    }

    #[test]
    fn a_worker_editing_one_file_twice_is_still_a_single_author() {
        let merged = merge_worker_changes([
            change("Implementation", "mission_panel.rs", 10, 1),
            change("Implementation", "mission_panel.rs", 5, 2),
        ]);

        assert_eq!(merged.len(), 1);
        assert!(!merged[0].is_contended());
        assert_eq!(merged[0].workers, vec!["Implementation"]);
        assert_eq!(merged[0].lines_added, 15);
    }

    #[test]
    fn the_author_count_counts_workers_not_files() {
        let merged = merge_worker_changes([
            change("Implementation", "mission_panel.rs", 10, 1),
            change("Implementation", "shared_context.rs", 5, 2),
            change("Review", "mission_panel.rs", 1, 1),
        ]);

        assert_eq!(merged.len(), 2);
        assert_eq!(change_author_count(&merged), 2);
        assert_eq!(change_author_count(&[]), 0);
    }

    #[test]
    fn worker_label_prefers_the_mission_role_over_the_thread_title() {
        let with_role = thread_with_mission(Some(MissionId::new()), Some("Review"));
        assert_eq!(worker_label(&with_role), SharedString::from("Review"));

        let without_role = thread_with_mission(Some(MissionId::new()), None);
        assert_eq!(worker_label(&without_role), without_role.display_title());
    }

    #[test]
    fn only_a_worker_waiting_on_the_user_counts_as_blocked() {
        let counts = mission_counts([
            MissionThreadState::Running,
            MissionThreadState::Waiting,
            MissionThreadState::Completed,
            MissionThreadState::Failed,
        ]);

        assert_eq!(counts.agents, 4);
        // A failed worker needs attention, but it isn't blocking on the user
        // the way a pending permission is, so the summary strip must not
        // fold the two together.
        assert_eq!(counts.blocked, 1);
        assert_eq!(counts.running, 1);
    }

    #[test]
    fn token_counts_stay_compact_enough_for_a_sidebar_row() {
        assert_eq!(format_token_count(938), "938");
        assert_eq!(format_token_count(42_100), "42.1k");
        assert_eq!(format_token_count(1_500_000), "1.5M");
    }

    #[test]
    fn evidence_recorded_before_the_last_edit_is_stale() {
        let earlier = chrono::Utc::now() - chrono::Duration::minutes(10);
        let later = chrono::Utc::now();

        assert!(evidence_is_stale(earlier, Some(later)));
        assert!(!evidence_is_stale(later, Some(earlier)));
        // Nothing has been edited since, so the command still proves what it
        // claimed to prove.
        assert!(!evidence_is_stale(earlier, None));
    }

    #[test]
    fn avatar_initials_handle_single_and_multi_word_names() {
        assert_eq!(initials("Huaodong Deng"), SharedString::from("HD"));
        assert_eq!(initials("huaodong"), SharedString::from("HU"));
        assert_eq!(initials(""), SharedString::from("?"));
    }

    #[test]
    fn groups_threads_by_mission_and_keeps_ungrouped_threads() {
        let mission_a = mission("Ship the feature");
        let mission_b = mission("Fix the bug");

        let thread_a1 = thread_with_mission(Some(mission_a.id), Some("coding"));
        let thread_a2 = thread_with_mission(Some(mission_a.id), Some("testing"));
        let thread_b1 = thread_with_mission(Some(mission_b.id), None);
        let thread_unassigned = thread_with_mission(None, None);

        let tree = build_mission_tree(
            vec![mission_a.clone(), mission_b.clone()],
            vec![
                thread_a1.clone(),
                thread_unassigned.clone(),
                thread_a2.clone(),
                thread_b1.clone(),
            ],
        );

        assert_eq!(tree.groups.len(), 2);
        let group_a = tree
            .groups
            .iter()
            .find(|group| group.mission.id == mission_a.id)
            .unwrap();
        assert_eq!(
            group_a
                .threads
                .iter()
                .map(|t| t.thread_id)
                .collect::<Vec<_>>(),
            vec![thread_a1.thread_id, thread_a2.thread_id]
        );
        let group_b = tree
            .groups
            .iter()
            .find(|group| group.mission.id == mission_b.id)
            .unwrap();
        assert_eq!(
            group_b
                .threads
                .iter()
                .map(|t| t.thread_id)
                .collect::<Vec<_>>(),
            vec![thread_b1.thread_id]
        );

        assert_eq!(tree.ungrouped.len(), 1);
        assert_eq!(tree.ungrouped[0].thread_id, thread_unassigned.thread_id);
    }

    #[test]
    fn threads_with_unknown_mission_id_fall_back_to_ungrouped() {
        let known_mission = mission("Known");
        let stale_mission_id = MissionId::new();
        let thread_stale = thread_with_mission(Some(stale_mission_id), None);

        let tree = build_mission_tree(vec![known_mission], vec![thread_stale.clone()]);

        assert_eq!(tree.groups.len(), 1);
        assert!(tree.groups[0].threads.is_empty());
        assert_eq!(tree.ungrouped.len(), 1);
        assert_eq!(tree.ungrouped[0].thread_id, thread_stale.thread_id);
    }

    #[test]
    fn empty_mission_context_is_not_reported_as_an_error() {
        let empty_context = shared_context::MissionContext {
            mission_id: "some-id".to_string(),
            decisions: Vec::new(),
            artifacts: Vec::new(),
            evidence: Vec::new(),
        };

        assert!(matches!(
            mission_context_state_from_result(Some(Ok(empty_context))),
            MissionContextState::Empty
        ));
        assert!(matches!(
            mission_context_state_from_result(Some(Err(anyhow::anyhow!("boom")))),
            MissionContextState::Unavailable
        ));
        assert!(matches!(
            mission_context_state_from_result(None),
            MissionContextState::Unavailable
        ));
    }

    pub(crate) async fn setup_workspace_with_two_threads(
        cx: &mut TestAppContext,
    ) -> (
        Entity<Workspace>,
        Entity<AgentPanel>,
        ThreadMetadata,
        ThreadMetadata,
        VisualTestContext,
    ) {
        test_support::init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
            ThreadMetadataStore::init_global(cx);
        });

        let fs = FakeFs::new(cx.executor());
        cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
        fs.insert_tree("/project", json!({ "file.txt": "" })).await;
        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace
            .read_with(cx, |mw, _cx| mw.workspace().clone())
            .unwrap();
        let mut cx = VisualTestContext::from_window(multi_workspace.into(), cx);
        test_support::register_test_sidebar(true, &mut cx);

        let agent_panel = workspace.update_in(&mut cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });

        test_support::open_thread_with_connection(
            &agent_panel,
            StubAgentConnection::new(),
            &mut cx,
        );
        let thread_a = agent_panel.read_with(&cx, |panel, cx| {
            let thread_id = panel.active_thread_id(cx).unwrap();
            ThreadMetadataStore::global(cx)
                .read(cx)
                .entry(thread_id)
                .unwrap()
                .clone()
        });

        test_support::open_thread_with_connection(
            &agent_panel,
            StubAgentConnection::new(),
            &mut cx,
        );
        let thread_b = agent_panel.read_with(&cx, |panel, cx| {
            let thread_id = panel.active_thread_id(cx).unwrap();
            ThreadMetadataStore::global(cx)
                .read(cx)
                .entry(thread_id)
                .unwrap()
                .clone()
        });

        (workspace, agent_panel, thread_a, thread_b, cx)
    }

    /// Puts both threads under one Mission and hands back a panel that has
    /// already loaded it, which is the state every render test below needs.
    pub(crate) async fn setup_mission_panel(
        cx: &mut TestAppContext,
    ) -> (
        Entity<Workspace>,
        Entity<MissionPanel>,
        Mission,
        VisualTestContext,
    ) {
        let (workspace, _agent_panel, thread_a, thread_b, mut cx) =
            setup_workspace_with_two_threads(cx).await;

        let create = cx.update(|_window, cx| {
            ThreadMetadataStore::global(cx)
                .read(cx)
                .create_mission("Agent Workspace UI".to_string(), cx)
        });
        let mission = create.await.unwrap();

        cx.update(|_window, cx| {
            ThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.set_thread_mission(
                    thread_a.thread_id,
                    Some(mission.id),
                    Some("Implementation".to_string()),
                    cx,
                );
                store.set_thread_mission(
                    thread_b.thread_id,
                    Some(mission.id),
                    Some("Review".to_string()),
                    cx,
                );
            });
        });
        cx.run_until_parked();

        let panel = workspace.update_in(&mut cx, |workspace, window, cx| {
            cx.new(|cx| MissionPanel::new(workspace, window, cx))
        });
        panel.update_in(&mut cx, |panel, window, cx| panel.refresh(window, cx));
        cx.run_until_parked();

        (workspace, panel, mission, cx)
    }

    fn draw_panel(panel: &Entity<MissionPanel>, cx: &mut VisualTestContext) {
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(px(320.), px(800.)),
            |_, _| panel.clone().into_any_element(),
        );
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn the_sidebar_selects_a_mission_and_renders_its_workers(cx: &mut TestAppContext) {
        let (_workspace, panel, mission, mut cx) = setup_mission_panel(cx).await;

        panel.read_with(&cx, |panel, cx| {
            assert_eq!(panel.selected_mission, Some(mission.id));
            let snapshot = panel.snapshot(cx);
            let mut labels: Vec<_> = snapshot
                .workers
                .iter()
                .map(|worker| worker.label.to_string())
                .collect();
            labels.sort();
            assert_eq!(labels, vec!["Implementation", "Review"]);
            // Nobody is blocked and nothing is contended, so the human
            // teammate row has nothing to badge.
            assert_eq!(snapshot.human.to_review, 0);
        });

        draw_panel(&panel, &mut cx);
    }

    #[gpui::test]
    async fn the_new_task_popover_renders_over_the_sidebar(cx: &mut TestAppContext) {
        let (_workspace, panel, _mission, mut cx) = setup_mission_panel(cx).await;

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.toggle_new_task(window, cx);
            assert!(panel.new_task_open);
        });

        draw_panel(&panel, &mut cx);
    }

    #[gpui::test]
    async fn opening_a_worker_renders_its_dashboard(cx: &mut TestAppContext) {
        let (workspace, panel, _mission, mut cx) = setup_mission_panel(cx).await;

        let worker = panel.read_with(&cx, |panel, cx| {
            panel
                .snapshot(cx)
                .workers
                .into_iter()
                .next()
                .expect("the Mission should have workers")
                .metadata
        });
        panel.update_in(&mut cx, |panel, window, cx| {
            panel.open_worker(worker, window, cx);
        });
        cx.run_until_parked();

        let dashboard = workspace
            .read_with(&cx, |workspace, cx| {
                workspace.items_of_type::<WorkerDashboard>(cx).next()
            })
            .expect("the worker dashboard should have opened as a tab");

        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(px(900.), px(700.)),
            |_, _| dashboard.clone().into_any_element(),
        );
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn the_mission_tabs_render(cx: &mut TestAppContext) {
        let (workspace, panel, _mission, mut cx) = setup_mission_panel(cx).await;

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.open_shared_context(window, cx);
            panel.open_evidence(window, cx);
            panel.open_queue(window, cx);
        });
        cx.run_until_parked();

        let (shared, evidence, queue) = workspace.read_with(&cx, |workspace, cx| {
            (
                workspace
                    .items_of_type::<crate::SharedContextView>(cx)
                    .next(),
                workspace.items_of_type::<crate::EvidenceView>(cx).next(),
                workspace
                    .items_of_type::<crate::MissionQueueView>(cx)
                    .next(),
            )
        });
        let shared = shared.expect("shared context tab should have opened");
        let evidence = evidence.expect("evidence tab should have opened");
        let queue = queue.expect("queue tab should have opened");

        let draw = |element: gpui::AnyView, cx: &mut VisualTestContext| {
            cx.draw(
                gpui::point(px(0.), px(0.)),
                gpui::size(px(900.), px(700.)),
                |_, _| element.clone().into_any_element(),
            );
            cx.run_until_parked();
        };
        draw(shared.into(), &mut cx);
        draw(evidence.into(), &mut cx);
        draw(queue.into(), &mut cx);
    }

    #[gpui::test]
    async fn clicking_a_thread_row_reuses_load_agent_thread_instead_of_creating_one(
        cx: &mut TestAppContext,
    ) {
        let (workspace, agent_panel, thread_a, thread_b, mut cx) =
            setup_workspace_with_two_threads(cx).await;

        // `thread_b` was opened last, so it's the one currently on screen.
        assert_eq!(
            test_support::active_thread_id(&agent_panel, &cx),
            thread_b.thread_id
        );

        let entry_count_before =
            cx.update(|_window, cx| ThreadMetadataStore::global(cx).read(cx).entries().count());

        let mission_panel = workspace.update_in(&mut cx, |workspace, window, cx| {
            cx.new(|cx| MissionPanel::new(workspace, window, cx))
        });

        mission_panel.update_in(&mut cx, |panel, window, cx| {
            panel.activate_thread(thread_a.clone(), window, cx);
        });
        cx.run_until_parked();

        let entry_count_after =
            cx.update(|_window, cx| ThreadMetadataStore::global(cx).read(cx).entries().count());
        assert_eq!(
            entry_count_after, entry_count_before,
            "activating an existing thread must not create a new one"
        );

        assert_eq!(
            test_support::active_thread_id(&agent_panel, &cx),
            thread_a.thread_id,
            "clicking the thread row should switch AgentPanel to the existing thread via load_agent_thread"
        );
    }
}
