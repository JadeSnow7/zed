//! Runtime dashboard for a single Mission worker, opened as an editor-pane tab.
//!
//! A Mission's workers are threads (`thread_metadata_store`), and the agent
//! panel can only show one of them at a time. This item makes a worker
//! something you can leave open beside your code: what it is running right
//! now, what it has changed, what it is blocked on, and what it has recorded
//! to the Shared Context bus. It is an observation surface --- prompting a
//! worker still happens in its agent-panel thread, reachable here via
//! "Open thread".
//!
//! Runtime sections need a live `AcpThread`, which only exists while
//! `AgentPanel` holds the thread (active or retained). The dashboard pins its
//! thread for as long as the tab is open (see `AgentPanel::pin_thread`) so a
//! worker that goes idle doesn't disappear out from under an open tab, but
//! pinning cannot resurrect a thread that was never loaded this session ---
//! for those, only the persisted Shared Context sections render, alongside an
//! "Open thread" prompt.

use acp_thread::{AcpThread, AgentThreadEntry, ToolCall, ToolCallStatus};
use agent_client_protocol::schema::v1 as acp;
use gpui::{
    AnyElement, App, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Subscription,
    WeakEntity,
};
use project::{AgentId, AgentServerStore};
use ui::{Icon, IconName, Indicator, Tooltip, prelude::*};
use workspace::{
    Workspace,
    item::{Item, ItemEvent, TabContentParams},
};

use crate::{
    Agent, AgentPanel, AgentThreadSource, MissionThreadState,
    mission_context_observer::shared_context_store,
    thread_metadata_store::{MissionId, ThreadId, ThreadMetadata, ThreadMetadataStore},
    thread_mission_state,
};

/// Everything the dashboard draws, lifted out of the live thread in one pass.
/// Rendering from a snapshot keeps the render body from holding a borrow of
/// `cx` through the `AcpThread` while it also needs `&mut Context<Self>` for
/// the permission buttons, and lets the "which sections have content" logic be
/// tested without standing up a live thread.
#[derive(Default)]
pub(crate) struct WorkerSnapshot {
    /// Whether `AgentPanel` currently holds this worker's thread. When it
    /// doesn't, no runtime state is readable and the tab offers to open it.
    thread_loaded: bool,
    permission: Option<PendingPermission>,
    current_tool_call: Option<CurrentToolCall>,
    changes: Vec<ChangedFile>,
}

impl WorkerSnapshot {
    /// True when the worker's thread is loaded but has nothing in flight ---
    /// distinct from the thread not being loaded at all.
    fn is_idle(&self) -> bool {
        self.thread_loaded
            && self.permission.is_none()
            && self.current_tool_call.is_none()
            && self.changes.is_empty()
    }
}

struct CurrentToolCall {
    label: SharedString,
    locations: String,
}

struct ChangedFile {
    name: String,
    lines_added: u32,
    lines_removed: u32,
}

/// Answers a worker's pending permission prompt. Free function so
/// `MissionPanel`'s attention cards and this dashboard's buttons take the
/// identical path into `AcpThread`.
pub(crate) fn authorize_worker(
    thread: &Entity<AcpThread>,
    tool_call_id: acp::ToolCallId,
    option: (acp::PermissionOptionId, acp::PermissionOptionKind),
    cx: &mut App,
) {
    let (option_id, option_kind) = option;
    thread.update(cx, |thread, cx| {
        thread.authorize_tool_call(
            tool_call_id,
            acp_thread::SelectedPermissionOutcome::new(option_id, option_kind),
            cx,
        );
    });
}

/// The tool call the worker is blocked on, if it is blocked. Scans back from
/// the end and stops at the most recent user message, the same boundary
/// `AcpThread::is_waiting_for_confirmation` uses: a prompt from before the
/// user's last turn is not what the worker is waiting on now.
fn pending_permission(thread: &AcpThread) -> Option<&ToolCall> {
    for entry in thread.entries().iter().rev() {
        match entry {
            AgentThreadEntry::UserMessage(_) => return None,
            AgentThreadEntry::ToolCall(
                call @ ToolCall {
                    status: ToolCallStatus::WaitingForConfirmation { .. },
                    ..
                },
            ) => return Some(call),
            _ => {}
        }
    }
    None
}

/// The tool call the worker is currently executing, if any.
fn active_tool_call(thread: &AcpThread) -> Option<&ToolCall> {
    thread.entries().iter().rev().find_map(|entry| match entry {
        AgentThreadEntry::ToolCall(
            call @ ToolCall {
                status: ToolCallStatus::InProgress | ToolCallStatus::Pending,
                ..
            },
        ) => Some(call),
        _ => None,
    })
}

/// The parts of a pending permission prompt the dashboard renders, copied out
/// of the thread so the render pass isn't holding a borrow of `cx` through the
/// `AcpThread` while it needs `&mut Context<Self>` for the button handlers.
/// `MissionPanel` reuses this so the two surfaces agree on what a worker is
/// blocked on and what answering it means.
#[derive(Clone)]
pub(crate) struct PendingPermission {
    pub tool_call_id: acp::ToolCallId,
    pub label: SharedString,
    pub allow: Option<(acp::PermissionOptionId, acp::PermissionOptionKind)>,
    pub reject: Option<(acp::PermissionOptionId, acp::PermissionOptionKind)>,
}

impl PendingPermission {
    pub(crate) fn for_thread(thread: &AcpThread, cx: &App) -> Option<Self> {
        Self::from_tool_call(pending_permission(thread)?, cx)
    }

    fn from_tool_call(tool_call: &ToolCall, cx: &App) -> Option<Self> {
        let ToolCallStatus::WaitingForConfirmation { options, .. } = &tool_call.status else {
            return None;
        };
        let option = |kind| {
            options
                .first_option_of_kind(kind)
                .map(|option| (option.option_id.clone(), option.kind))
        };

        Some(Self {
            tool_call_id: tool_call.id.clone(),
            label: tool_call.label.read(cx).source().clone(),
            allow: option(acp::PermissionOptionKind::AllowOnce),
            reject: option(acp::PermissionOptionKind::RejectOnce),
        })
    }
}

/// Shared Context rows recorded by this worker. Loaded pull-based, matching
/// `MissionPanel`'s treatment of the same store.
#[derive(Default)]
enum WorkerContextState {
    #[default]
    Loading,
    Loaded(shared_context::MissionContext),
    /// The store failed to open at startup, or the query failed. Distinct from
    /// a loaded-but-empty context so the tab can say so rather than implying
    /// the worker simply hasn't recorded anything.
    Unavailable,
}

pub struct WorkerDashboard {
    mission_id: MissionId,
    thread_id: ThreadId,
    role: SharedString,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    context_state: WorkerContextState,
    /// The `AcpThread` the runtime sections are currently observing. Compared
    /// against the live one to notice a thread being loaded or dropped by
    /// `AgentPanel` while this tab is open.
    observed_thread: Option<Entity<AcpThread>>,
    _thread_observation: Option<Subscription>,
    _panel_observation: Option<Subscription>,
}

impl WorkerDashboard {
    /// Opens the dashboard for `metadata`'s worker, activating an existing tab
    /// for that worker rather than opening a second one.
    pub fn deploy(
        mission_id: MissionId,
        metadata: ThreadMetadata,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let thread_id = metadata.thread_id;
        let existing = workspace
            .items_of_type::<WorkerDashboard>(cx)
            .find(|dashboard| dashboard.read(cx).thread_id == thread_id);

        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
            return existing;
        }

        // The panel is resolved here rather than inside `new`: `new` runs
        // while this `Workspace` is leased by the enclosing update, so it
        // cannot read the workspace back out of its own weak handle.
        let panel = workspace.panel::<AgentPanel>(cx);
        let weak_workspace = workspace.weak_handle();
        let dashboard =
            cx.new(|cx| WorkerDashboard::new(mission_id, metadata, weak_workspace, panel, cx));
        workspace.add_item_to_active_pane(Box::new(dashboard.clone()), None, true, window, cx);
        dashboard
    }

    fn new(
        mission_id: MissionId,
        metadata: ThreadMetadata,
        workspace: WeakEntity<Workspace>,
        agent_panel: Option<Entity<AgentPanel>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let role = metadata
            .role
            .clone()
            .map(SharedString::from)
            .unwrap_or_else(|| metadata.display_title());

        let mut this = Self {
            mission_id,
            thread_id: metadata.thread_id,
            role,
            workspace,
            focus_handle: cx.focus_handle(),
            context_state: WorkerContextState::Loading,
            observed_thread: None,
            _thread_observation: None,
            _panel_observation: None,
        };

        if let Some(panel) = agent_panel {
            panel.update(cx, |panel, _| panel.pin_thread(this.thread_id));
            this._panel_observation = Some(cx.observe(&panel, |this, _, cx| {
                this.sync_thread_observation(cx);
            }));
            cx.on_release({
                let panel = panel.downgrade();
                let thread_id = this.thread_id;
                move |_, cx| {
                    panel
                        .update(cx, |panel, cx| panel.unpin_thread(thread_id, cx))
                        .ok();
                }
            })
            .detach();
            this.observe_thread_of(&panel, cx);
        }
        this.refresh_context(cx);
        this
    }

    fn agent_panel(&self, cx: &App) -> Option<Entity<AgentPanel>> {
        self.workspace.upgrade()?.read(cx).panel::<AgentPanel>(cx)
    }

    /// The worker's live thread, if `AgentPanel` currently holds it.
    fn thread(&self, cx: &App) -> Option<Entity<AcpThread>> {
        let panel = self.agent_panel(cx)?;
        let panel = panel.read(cx);
        panel
            .conversation_view_for_id(&self.thread_id, cx)?
            .read(cx)
            .root_thread(cx)
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

    fn metadata(&self, cx: &App) -> Option<ThreadMetadata> {
        ThreadMetadataStore::try_global(cx)?
            .read(cx)
            .entry(self.thread_id)
            .cloned()
    }

    /// Re-points the runtime sections at whichever `AcpThread` the panel holds
    /// for this worker now, which changes as `AgentPanel` loads and drops
    /// threads underneath an open tab.
    fn sync_thread_observation(&mut self, cx: &mut Context<Self>) {
        let Some(panel) = self.agent_panel(cx) else {
            return;
        };
        self.observe_thread_of(&panel, cx);
    }

    fn observe_thread_of(&mut self, panel: &Entity<AgentPanel>, cx: &mut Context<Self>) {
        let thread = panel
            .read(cx)
            .conversation_view_for_id(&self.thread_id, cx)
            .and_then(|view| view.read(cx).root_thread(cx));
        if thread.as_ref().map(Entity::entity_id) == self.observed_thread.as_ref().map(Entity::entity_id)
        {
            return;
        }

        self._thread_observation = thread
            .as_ref()
            .map(|thread| cx.observe(thread, |_, _, cx| cx.notify()));
        self.observed_thread = thread;
        cx.notify();
    }

    fn refresh_context(&mut self, cx: &mut Context<Self>) {
        let Some(store) = shared_context_store(cx) else {
            self.context_state = WorkerContextState::Unavailable;
            return;
        };
        let mission_key = self.mission_id.to_key_string();
        let query = cx.background_spawn(async move {
            let mission_id = shared_context::MissionId::from_key_string(&mission_key).ok()?;
            Some(store.get_mission_context(mission_id, None))
        });
        cx.spawn(async move |this, cx| {
            let result = query.await;
            this.update(cx, |this, cx| {
                this.context_state = match result {
                    Some(Ok(context)) => WorkerContextState::Loaded(context),
                    Some(Err(_)) | None => WorkerContextState::Unavailable,
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn state(&self, cx: &App) -> MissionThreadState {
        self.agent_panel(cx)
            .map(|panel| thread_mission_state(panel.read(cx), self.thread_id, cx))
            .unwrap_or(MissionThreadState::Created)
    }

    /// Switches the agent panel to this worker's thread. Goes through
    /// `AgentPanel::load_agent_thread`, the same entrypoint the sidebar and
    /// `MissionPanel` use, so no second connection is opened.
    fn open_thread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(metadata) = self.metadata(cx) else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let Some(panel) = workspace.read(cx).panel::<AgentPanel>(cx) else {
            return;
        };
        panel.update(cx, |panel, cx| {
            panel.load_agent_thread(
                Agent::from(metadata.agent_id.clone()),
                metadata.thread_id,
                Some(metadata.folder_paths().clone()),
                metadata.title.clone(),
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

    fn stop(&mut self, cx: &mut Context<Self>) {
        let Some(thread) = self.thread(cx) else {
            return;
        };
        thread.update(cx, |thread, cx| thread.cancel(cx)).detach();
    }

    fn authorize(
        &mut self,
        tool_call_id: acp::ToolCallId,
        option: (acp::PermissionOptionId, acp::PermissionOptionKind),
        cx: &mut Context<Self>,
    ) {
        let Some(thread) = self.thread(cx) else {
            return;
        };
        authorize_worker(&thread, tool_call_id, option, cx);
    }
}

/// The icon identifying a worker's harness, mirroring how `crates/sidebar`
/// resolves thread icons: a built-in name, overridden by the agent server's
/// own SVG when it ships one.
pub(crate) fn harness_icon(
    agent_id: &AgentId,
    store: Option<&Entity<AgentServerStore>>,
    cx: &App,
) -> AnyElement {
    let fallback = match Agent::from(agent_id.clone()) {
        Agent::NativeAgent => IconName::ZedAgent,
        _ => IconName::Terminal,
    };
    let external_svg = store.and_then(|store| store.read(cx).agent_icon(agent_id));

    match external_svg {
        Some(svg) => Icon::from_external_svg(svg)
            .size(IconSize::Small)
            .color(Color::Muted)
            .into_any_element(),
        None => Icon::new(fallback)
            .size(IconSize::Small)
            .color(Color::Muted)
            .into_any_element(),
    }
}

/// Label and colour for a worker's state, shared by this dashboard's status
/// pill and `MissionPanel`'s worker rows so the two never disagree.
pub(crate) fn worker_status(state: MissionThreadState) -> (&'static str, Color) {
    match state {
        MissionThreadState::Created => ("Not started", Color::Muted),
        MissionThreadState::Running => ("Working", Color::Success),
        MissionThreadState::Waiting => ("Waiting for you", Color::Warning),
        MissionThreadState::Completed => ("Idle", Color::Muted),
        MissionThreadState::Failed => ("Failed", Color::Error),
    }
}

impl WorkerDashboard {
    fn render_section(
        &self,
        label: impl Into<SharedString>,
        children: impl IntoIterator<Item = AnyElement>,
    ) -> impl IntoElement {
        v_flex()
            .gap_1()
            .child(
                Label::new(label.into())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .children(children)
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state(cx);
        let (status_label, status_color) = worker_status(state);
        let store = self.agent_server_store(cx);
        let agent_label = self.metadata(cx).map(|metadata| {
            store
                .as_ref()
                .and_then(|store| store.read(cx).agent_display_name(&metadata.agent_id))
                .unwrap_or_else(|| Agent::from(metadata.agent_id.clone()).label())
        });
        let is_generating = state == MissionThreadState::Running;

        h_flex()
            .w_full()
            .gap_2()
            .px_4()
            .py_3()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                v_flex()
                    .flex_1()
                    .child(Label::new(self.role.clone()))
                    .children(agent_label.map(|label| {
                        Label::new(format!("Harness: {label}"))
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                    })),
            )
            .child(
                Label::new(status_label)
                    .size(LabelSize::Small)
                    .color(status_color),
            )
            .child(
                Button::new("worker-open-thread", "Open thread")
                    .start_icon(Icon::new(IconName::Thread))
                    .label_size(LabelSize::Small)
                    .tooltip(Tooltip::text("Show this worker's thread in the agent panel"))
                    .on_click(cx.listener(|this, _, window, cx| this.open_thread(window, cx))),
            )
            .when(is_generating, |this| {
                this.child(
                    Button::new("worker-stop", "Stop")
                        .start_icon(Icon::new(IconName::Stop))
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
                )
            })
    }

    fn render_permission_request(
        &self,
        permission: PendingPermission,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let render_button = |id: &'static str,
                             text: &'static str,
                             option: Option<(acp::PermissionOptionId, acp::PermissionOptionKind)>,
                             cx: &mut Context<Self>| {
            let tool_call_id = permission.tool_call_id.clone();
            option.map(|(option_id, option_kind)| {
                Button::new(id, text)
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.authorize(
                            tool_call_id.clone(),
                            (option_id.clone(), option_kind),
                            cx,
                        );
                    }))
            })
        };

        let allow = render_button("worker-permission-allow", "Allow", permission.allow.clone(), cx);
        let reject = render_button(
            "worker-permission-reject",
            "Reject",
            permission.reject.clone(),
            cx,
        );

        v_flex()
            .gap_2()
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().status().warning_border)
            .child(
                h_flex()
                    .gap_2()
                    .child(Icon::new(IconName::Warning).color(Color::Warning))
                    .child(Label::new("Permission needed")),
            )
            .child(
                Label::new(permission.label.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(h_flex().gap_1().children(allow).children(reject))
            .into_any_element()
    }

    fn render_current_tool_call(&self, tool_call: CurrentToolCall, cx: &App) -> AnyElement {
        v_flex()
            .gap_1()
            .p_2()
            .rounded_md()
            .bg(cx.theme().colors().element_background)
            .child(Label::new(tool_call.label).size(LabelSize::Small))
            .when(!tool_call.locations.is_empty(), |this| {
                this.child(
                    Label::new(tool_call.locations)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .into_any_element()
    }

    fn snapshot(&self, cx: &App) -> WorkerSnapshot {
        let Some(thread) = self.thread(cx) else {
            return WorkerSnapshot::default();
        };
        let thread = thread.read(cx);

        let changes = thread
            .action_log()
            .read(cx)
            .changed_buffers(cx)
            .filter_map(|(buffer, diff)| {
                let path = buffer.read(cx).file()?.path().clone();
                let stats = action_log::DiffStats::single_file(diff.read(cx));
                Some(ChangedFile {
                    name: path
                        .file_name()
                        .unwrap_or_else(|| path.as_unix_str())
                        .to_string(),
                    lines_added: stats.lines_added,
                    lines_removed: stats.lines_removed,
                })
            })
            .collect();

        WorkerSnapshot {
            thread_loaded: true,
            permission: PendingPermission::for_thread(thread, cx),
            current_tool_call: active_tool_call(thread).map(|call| CurrentToolCall {
                label: call.label.read(cx).source().clone(),
                locations: call
                    .locations
                    .iter()
                    .map(|location| location.path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
            }),
            changes,
        }
    }

    fn render_changes(&self, changes: Vec<ChangedFile>) -> Vec<AnyElement> {
        changes
            .into_iter()
            .map(|file| {
                h_flex()
                    .gap_2()
                    .child(Label::new(file.name).size(LabelSize::Small))
                    .child(
                        Label::new(format!("+{}", file.lines_added))
                            .size(LabelSize::Small)
                            .color(Color::Created),
                    )
                    .child(
                        Label::new(format!("-{}", file.lines_removed))
                            .size(LabelSize::Small)
                            .color(Color::Deleted),
                    )
                    .into_any_element()
            })
            .collect()
    }

    /// Shared Context rows this worker recorded. The observer attributes rows
    /// to a thread's Mission role (see `mission_context_observer`), which is
    /// what makes this filter line up with what the worker actually did.
    fn render_recorded(&self) -> Vec<AnyElement> {
        let context = match &self.context_state {
            WorkerContextState::Loaded(context) => context,
            WorkerContextState::Loading => {
                return vec![
                    Label::new("Loading…")
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .into_any_element(),
                ];
            }
            WorkerContextState::Unavailable => {
                return vec![
                    Label::new("Shared Context unavailable")
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .into_any_element(),
                ];
            }
        };

        let role = self.role.as_ref();
        let mut rows = Vec::new();
        for decision in context.decisions.iter().filter(|row| row.author == role) {
            rows.push(
                h_flex()
                    .gap_2()
                    .child(Icon::new(IconName::ListTodo).size(IconSize::XSmall))
                    .child(Label::new(decision.key.clone()).size(LabelSize::Small))
                    .child(
                        Label::new(decision.value.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .into_any_element(),
            );
        }
        for evidence in context.evidence.iter().filter(|row| row.author == role) {
            let succeeded = evidence.exit_code == Some(0);
            rows.push(
                h_flex()
                    .gap_2()
                    .child(
                        Icon::new(if succeeded {
                            IconName::Check
                        } else {
                            IconName::XCircle
                        })
                        .size(IconSize::XSmall)
                        .color(if succeeded {
                            Color::Success
                        } else {
                            Color::Error
                        }),
                    )
                    .child(Label::new(evidence.command.clone()).size(LabelSize::Small))
                    .into_any_element(),
            );
        }

        if rows.is_empty() {
            rows.push(
                Label::new("Nothing recorded yet")
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .into_any_element(),
            );
        }
        rows
    }
}

impl Render for WorkerDashboard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snapshot = self.snapshot(cx);
        let is_idle = snapshot.is_idle();
        let thread_loaded = snapshot.thread_loaded;
        let has_changes = !snapshot.changes.is_empty();

        let current = snapshot
            .current_tool_call
            .map(|call| self.render_current_tool_call(call, cx));
        let changes = self.render_changes(snapshot.changes);
        let permission = snapshot
            .permission
            .map(|permission| self.render_permission_request(permission, cx));

        v_flex()
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .track_focus(&self.focus_handle)
            .child(self.render_header(cx))
            .child(
                v_flex()
                    .flex_1()
                    .gap_4()
                    .p_4()
                    .when(!thread_loaded, |this| {
                        this.child(
                            v_flex()
                                .gap_1()
                                .child(
                                    Label::new("This worker's thread isn't loaded")
                                        .color(Color::Muted),
                                )
                                .child(
                                    Label::new(
                                        "Open the thread to see what it is running right now.",
                                    )
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                                ),
                        )
                    })
                    .when(is_idle, |this| {
                        this.child(
                            Label::new("Nothing in flight right now")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    })
                    .children(permission)
                    .children(current.map(|current| {
                        self.render_section("Current tool call", [current])
                            .into_any_element()
                    }))
                    .when(has_changes, |this| {
                        this.child(self.render_section("Its changes", changes))
                    })
                    .child(
                        self.render_section("Recorded to Shared Context", self.render_recorded()),
                    ),
            )
    }
}

impl EventEmitter<ItemEvent> for WorkerDashboard {}

impl Focusable for WorkerDashboard {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for WorkerDashboard {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.role.clone()
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        let (_, status_color) = worker_status(self.state(cx));

        h_flex()
            .gap_1p5()
            .children(self.metadata(cx).map(|metadata| {
                harness_icon(&metadata.agent_id, self.agent_server_store(cx).as_ref(), cx)
            }))
            .child(
                Label::new(self.role.clone())
                    .when(!params.selected, |this| this.color(Color::Muted)),
            )
            .child(Indicator::dot().color(status_color))
            .into_any_element()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        None
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Mission Worker Dashboard Opened")
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unloaded_thread_is_not_idle() {
        let snapshot = WorkerSnapshot::default();
        assert!(!snapshot.thread_loaded);
        // "Idle" is a statement about a loaded worker, so an unloaded one must
        // not claim it -- the tab shows "open the thread" instead.
        assert!(!snapshot.is_idle());
    }

    #[test]
    fn a_loaded_worker_with_changes_is_not_idle() {
        let snapshot = WorkerSnapshot {
            thread_loaded: true,
            changes: vec![ChangedFile {
                name: "mission_panel.rs".into(),
                lines_added: 12,
                lines_removed: 3,
            }],
            ..Default::default()
        };
        assert!(!snapshot.is_idle());
    }

    #[test]
    fn a_loaded_worker_with_nothing_in_flight_is_idle() {
        let snapshot = WorkerSnapshot {
            thread_loaded: true,
            ..Default::default()
        };
        assert!(snapshot.is_idle());
    }

    #[test]
    fn worker_status_distinguishes_blocked_from_working() {
        assert_eq!(worker_status(MissionThreadState::Running).1, Color::Success);
        assert_eq!(worker_status(MissionThreadState::Waiting).1, Color::Warning);
        assert_eq!(worker_status(MissionThreadState::Failed).1, Color::Error);
    }
}
