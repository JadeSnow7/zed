//! The Mission's full-page tabs: Shared Context, Evidence, and the human
//! teammate's queue.
//!
//! The Mission sidebar (`mission_panel`) can only show the last few rows of
//! each trail; these are where the whole thing lives. All three are editor-pane
//! items rather than panels, because they are things you read beside code and
//! leave open, not chrome you toggle.
//!
//! Shared Context and Evidence read the persisted `shared_context` store
//! pull-based, matching how `mission_panel` treats it: the store publishes no
//! change events, so each tab loads when it opens and reloads on request. The
//! queue is different --- everything in it is live worker state, so it observes
//! the Mission's threads directly and pins them so a worker going idle can't
//! empty the queue out from under the user.

use chrono::{DateTime, Utc};
use gpui::{
    AnyElement, App, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Subscription,
    WeakEntity, Window,
};
use ui::{Chip, Color, Icon, IconName, IconSize, Indicator, Label, LabelSize, Tooltip, prelude::*};
use workspace::{
    Workspace,
    item::{Item, ItemEvent},
};

use crate::{
    Agent, AgentDiffPane, AgentPanel, AgentThreadSource,
    mission_context_observer::shared_context_store,
    mission_panel::{
        MissionChange, MissionContextState, MissionSnapshot, WorkerRow, bilingual_label,
        evidence_is_stale, format_token_count, mission_context_state_from_result, mission_snapshot,
        relative_time, section_header, send_to_worker,
    },
    thread_metadata_store::{Mission, MissionId, ThreadMetadata, ThreadMetadataStore},
    worker_dashboard::authorize_worker,
};

/// Loads a Mission's Shared Context rows into whichever tab asked for them.
/// Implemented by every view here so the query, and the "unavailable vs.
/// simply empty" distinction it carries, is written once.
trait MissionContextHost: 'static + Sized {
    fn set_context_state(&mut self, state: MissionContextState, cx: &mut Context<Self>);
}

fn refresh_mission_context<T: MissionContextHost>(mission_id: MissionId, cx: &mut Context<T>) {
    // A missing store and a failed query both land on `Unavailable`, so the
    // "store never opened" case doesn't need its own early return -- which
    // matters because this runs during construction, where the entity cannot
    // be updated in place yet.
    let store = shared_context_store(cx);
    let mission_key = mission_id.to_key_string();
    let query = cx.background_spawn(async move {
        let store = store?;
        let mission_id = shared_context::MissionId::from_key_string(&mission_key).ok()?;
        Some(store.get_mission_context(mission_id, None))
    });
    cx.spawn(async move |this, cx| {
        let result = query.await;
        this.update(cx, |this, cx| {
            this.set_context_state(mission_context_state_from_result(result), cx);
        })
        .ok();
    })
    .detach();
}

/// Switches the agent panel to a worker's conversation. Shared by the tabs'
/// "Open session" affordances so none of them opens a second connection.
fn open_worker_session(
    workspace: &WeakEntity<Workspace>,
    metadata: &ThreadMetadata,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = workspace.upgrade() else {
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

/// A short, human-quotable form of the Mission id, as the design's
/// "mission 61b236e4" header uses.
fn short_mission_id(mission_id: MissionId) -> String {
    mission_id.to_key_string().chars().take(8).collect()
}

// --- Shared Context ---------------------------------------------------------

pub struct SharedContextView {
    mission: Mission,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    context_state: MissionContextState,
    filter: Entity<editor::Editor>,
    _filter_subscription: Subscription,
}

impl SharedContextView {
    pub fn deploy(
        mission: Mission,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let existing = workspace
            .items_of_type::<SharedContextView>(cx)
            .find(|view| view.read(cx).mission.id == mission.id);
        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
            return existing;
        }

        let weak_workspace = workspace.weak_handle();
        let view = cx.new(|cx| SharedContextView::new(mission, weak_workspace, window, cx));
        workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
        view
    }

    fn new(
        mission: Mission,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter = cx.new(|cx| {
            let mut editor = editor::Editor::single_line(window, cx);
            editor.set_placeholder_text("Search context", window, cx);
            editor
        });
        let _filter_subscription =
            cx.subscribe(&filter, |_, _, _: &editor::EditorEvent, cx| cx.notify());

        refresh_mission_context(mission.id, cx);
        Self {
            mission,
            workspace,
            focus_handle: cx.focus_handle(),
            context_state: MissionContextState::Loading,
            filter,
            _filter_subscription,
        }
    }

    fn query(&self, cx: &App) -> String {
        self.filter.read(cx).text(cx).trim().to_lowercase()
    }

    fn subscribers(&self, cx: &App) -> Vec<SharedString> {
        mission_snapshot(&self.mission, &self.workspace, cx)
            .workers
            .into_iter()
            .map(|worker| worker.label)
            .collect()
    }
}

impl MissionContextHost for SharedContextView {
    fn set_context_state(&mut self, state: MissionContextState, cx: &mut Context<Self>) {
        self.context_state = state;
        cx.notify();
    }
}

impl SharedContextView {
    fn render_row(
        title: SharedString,
        byline: String,
        trailing: Option<AnyElement>,
        cx: &App,
    ) -> AnyElement {
        v_flex()
            .w_full()
            .px_4()
            .py_1p5()
            .gap_0p5()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(Label::new(title).size(LabelSize::Small))
            .child(
                h_flex()
                    .gap_1p5()
                    .items_baseline()
                    .child(
                        Label::new(byline)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .children(trailing),
            )
            .into_any_element()
    }
}

impl Render for SharedContextView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self.query(cx);
        let matches = |haystack: &str| query.is_empty() || haystack.to_lowercase().contains(&query);
        let subscribers = self.subscribers(cx);

        let body: AnyElement = match &self.context_state {
            MissionContextState::Loading => empty_body("Loading the Mission's shared context…", cx),
            MissionContextState::NoSelection | MissionContextState::Empty => empty_body(
                "No decisions, artifacts, or evidence recorded for this Mission yet.",
                cx,
            ),
            MissionContextState::Unavailable => empty_body(
                "The shared context store is unavailable, so this Mission's trail can't be read.",
                cx,
            ),
            MissionContextState::Populated(context) => {
                let newest_artifact = context
                    .artifacts
                    .iter()
                    .map(|artifact| artifact.created_at)
                    .max();

                let decisions: Vec<AnyElement> = context
                    .decisions
                    .iter()
                    .rev()
                    .filter(|decision| {
                        matches(&decision.value)
                            || matches(&decision.key)
                            || matches(&decision.author)
                    })
                    .map(|decision| {
                        Self::render_row(
                            decision.value.clone().into(),
                            format!(
                                "{} · {}",
                                decision.author,
                                relative_time(decision.created_at)
                            ),
                            None,
                            cx,
                        )
                    })
                    .collect();

                let artifacts: Vec<AnyElement> = context
                    .artifacts
                    .iter()
                    .rev()
                    .filter(|artifact| {
                        matches(&artifact.path)
                            || matches(&artifact.change_summary)
                            || matches(&artifact.author)
                    })
                    .map(|artifact| {
                        Self::render_row(
                            artifact.path.clone().into(),
                            format!(
                                "{} · {} · {}",
                                artifact.author,
                                artifact.change_summary,
                                relative_time(artifact.created_at)
                            ),
                            None,
                            cx,
                        )
                    })
                    .collect();

                let evidence: Vec<AnyElement> = context
                    .evidence
                    .iter()
                    .rev()
                    .filter(|row| matches(&row.command) || matches(&row.author))
                    .map(|row| {
                        let stale = evidence_is_stale(row.created_at, newest_artifact);
                        Self::render_row(
                            row.command.clone().into(),
                            format!("{} · {}", row.author, relative_time(row.created_at)),
                            Some(
                                Label::new(if stale { "stale" } else { "current" })
                                    .size(LabelSize::XSmall)
                                    .color(if stale { Color::Warning } else { Color::Muted })
                                    .into_any_element(),
                            ),
                            cx,
                        )
                    })
                    .collect();

                v_flex()
                    .w_full()
                    .child(section_header(
                        "DECISIONS",
                        "决策",
                        Some(context.decisions.len().to_string().into()),
                    ))
                    .children(decisions)
                    .child(section_header(
                        "ARTIFACTS",
                        "产出物",
                        Some(context.artifacts.len().to_string().into()),
                    ))
                    .children(artifacts)
                    .child(
                        h_flex()
                            .w_full()
                            .items_baseline()
                            .child(section_header(
                                "EVIDENCE",
                                "验证证据",
                                Some(context.evidence.len().to_string().into()),
                            ))
                            .child(
                                Button::new("shared-context-open-evidence", "Open all")
                                    .label_size(LabelSize::Small)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        let (Some(workspace), mission) =
                                            (this.workspace.upgrade(), this.mission.clone())
                                        else {
                                            return;
                                        };
                                        workspace.update(cx, |workspace, cx| {
                                            EvidenceView::deploy(mission, workspace, window, cx);
                                        });
                                    })),
                            )
                            .pr_4(),
                    )
                    .children(evidence)
                    .into_any_element()
            }
        };

        v_flex()
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .track_focus(&self.focus_handle)
            .child(
                v_flex()
                    .w_full()
                    .flex_none()
                    .px_4()
                    .py_3()
                    .gap_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        h_flex()
                            .gap_1p5()
                            .items_baseline()
                            .child(Label::new("Shared context"))
                            .child(
                                Label::new("共享上下文")
                                    .size(LabelSize::Small)
                                    .color(Color::Disabled),
                            )
                            .child(
                                Label::new(format!(
                                    "· mission {} · written by {} workers · read by all",
                                    short_mission_id(self.mission.id),
                                    subscribers.len()
                                ))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            ),
                    )
                    .child(
                        div()
                            .w_full()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .bg(cx.theme().colors().element_background)
                            .child(self.filter.clone()),
                    ),
            )
            .child(
                v_flex()
                    .id("shared-context-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(body),
            )
            .child(
                h_flex()
                    .w_full()
                    .flex_none()
                    .flex_wrap()
                    .gap_1()
                    .px_4()
                    .py_2()
                    .border_t_1()
                    .border_color(cx.theme().colors().border)
                    .child(bilingual_label("SUBSCRIBED", "订阅者"))
                    .children(
                        subscribers
                            .into_iter()
                            .map(|name| Chip::new(name).label_size(LabelSize::XSmall)),
                    ),
            )
    }
}

impl EventEmitter<ItemEvent> for SharedContextView {}

impl Focusable for SharedContextView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for SharedContextView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Shared context".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ListTree))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Mission Shared Context Opened")
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

// --- Evidence ---------------------------------------------------------------

pub struct EvidenceView {
    mission: Mission,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    context_state: MissionContextState,
}

impl EvidenceView {
    pub fn deploy(
        mission: Mission,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let existing = workspace
            .items_of_type::<EvidenceView>(cx)
            .find(|view| view.read(cx).mission.id == mission.id);
        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
            return existing;
        }

        let weak_workspace = workspace.weak_handle();
        let view = cx.new(|cx| EvidenceView::new(mission, weak_workspace, cx));
        workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
        view
    }

    fn new(mission: Mission, workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let mission_id = mission.id;
        refresh_mission_context(mission_id, cx);
        Self {
            mission,
            workspace,
            focus_handle: cx.focus_handle(),
            context_state: MissionContextState::Loading,
        }
    }

    /// The worker whose role matches an Evidence row's author, so "Open
    /// session" lands on the worker that actually ran the command. The
    /// observer attributes rows to the thread's Mission role (see
    /// `mission_context_observer`), which is what makes this lookup work.
    fn worker_for_author(&self, author: &str, cx: &App) -> Option<ThreadMetadata> {
        mission_snapshot(&self.mission, &self.workspace, cx)
            .workers
            .into_iter()
            .find(|worker| worker.label.as_ref() == author)
            .map(|worker| worker.metadata)
    }
}

impl MissionContextHost for EvidenceView {
    fn set_context_state(&mut self, state: MissionContextState, cx: &mut Context<Self>) {
        self.context_state = state;
        cx.notify();
    }
}

impl Render for EvidenceView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (rows, passing, stale_count): (Vec<AnyElement>, usize, usize) = match &self
            .context_state
        {
            MissionContextState::Populated(context) => {
                let newest_artifact = context
                    .artifacts
                    .iter()
                    .map(|artifact| artifact.created_at)
                    .max();
                let mut passing = 0;
                let mut stale_count = 0;
                let mut rows = Vec::new();

                for row in context.evidence.iter().rev() {
                    let stale = evidence_is_stale(row.created_at, newest_artifact);
                    let succeeded = row.exit_code == Some(0);
                    if stale {
                        stale_count += 1;
                    } else if succeeded {
                        passing += 1;
                    }
                    rows.push(self.render_evidence(
                        row.command.clone(),
                        row.result.clone(),
                        row.author.clone(),
                        row.exit_code,
                        row.created_at,
                        stale,
                        cx,
                    ));
                }
                (rows, passing, stale_count)
            }
            MissionContextState::Loading => (
                vec![empty_body("Loading the Mission's evidence…", cx)],
                0,
                0,
            ),
            MissionContextState::Unavailable => (
                vec![empty_body(
                    "The shared context store is unavailable, so this Mission's evidence can't be read.",
                    cx,
                )],
                0,
                0,
            ),
            MissionContextState::NoSelection | MissionContextState::Empty => (
                vec![empty_body(
                    "No commands have been recorded as evidence for this Mission yet.",
                    cx,
                )],
                0,
                0,
            ),
        };

        v_flex()
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .track_focus(&self.focus_handle)
            .child(
                h_flex()
                    .w_full()
                    .flex_none()
                    .items_center()
                    .gap_1p5()
                    .px_4()
                    .py_3()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(Label::new("Evidence"))
                    .child(
                        Label::new("验证证据")
                            .size(LabelSize::Small)
                            .color(Color::Disabled),
                    )
                    .child(
                        Label::new("· every claim links to a command that actually ran")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .child(
                        Label::new(format!("{passing} passing · {stale_count} stale"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .child(
                v_flex()
                    .id("evidence-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_4()
                    .gap_3()
                    .children(rows),
            )
    }
}

impl EvidenceView {
    #[allow(clippy::too_many_arguments)]
    fn render_evidence(
        &self,
        command: String,
        result: String,
        author: String,
        exit_code: Option<i32>,
        created_at: DateTime<Utc>,
        stale: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let succeeded = exit_code == Some(0);
        let exit_label = match exit_code {
            Some(code) => format!("exit {code}"),
            None => "no exit code".to_string(),
        };
        let worker = self.worker_for_author(&author, cx);

        v_flex()
            .w_full()
            .rounded_md()
            .border_1()
            .border_color(if stale {
                cx.theme().status().warning_border
            } else {
                cx.theme().colors().border
            })
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .px_3()
                    .py_2()
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
                    .child(
                        Label::new(command)
                            .size(LabelSize::Small)
                            .buffer_font(cx)
                            .truncate(),
                    )
                    .child(div().flex_1())
                    .child(
                        Label::new(format!(
                            "{author} · {} · {exit_label}",
                            relative_time(created_at)
                        ))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    ),
            )
            .when(!result.trim().is_empty(), |this| {
                this.child(
                    div()
                        .w_full()
                        .px_3()
                        .py_2()
                        .bg(cx.theme().colors().element_background)
                        .child(
                            Label::new(result.trim().to_string())
                                .size(LabelSize::XSmall)
                                .buffer_font(cx)
                                .color(Color::Muted),
                        ),
                )
            })
            .when(stale, |this| {
                this.child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .items_center()
                        .px_3()
                        .py_2()
                        .child(
                            Label::new("stale — ran before the last recorded edit")
                                .size(LabelSize::XSmall)
                                .color(Color::Warning),
                        )
                        .child(div().flex_1())
                        .children(worker.clone().map(|metadata| {
                            let workspace = self.workspace.clone();
                            Button::new(
                                SharedString::from(format!(
                                    "evidence-rerun-{}",
                                    metadata.thread_id.to_key_string()
                                )),
                                format!("Ask {author} to re-run"),
                            )
                            .start_icon(Icon::new(IconName::HistoryRerun))
                            .label_size(LabelSize::Small)
                            .tooltip(Tooltip::text(
                                "Opens the worker's conversation; re-running is its call, not Zed's",
                            ))
                            .on_click(move |_, window, cx| {
                                open_worker_session(&workspace, &metadata, window, cx);
                            })
                        })),
                )
            })
            .into_any_element()
    }
}

impl EventEmitter<ItemEvent> for EvidenceView {}

impl Focusable for EvidenceView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for EvidenceView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Evidence".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::CheckDouble))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Mission Evidence Opened")
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

// --- The human teammate's queue --------------------------------------------

pub struct MissionQueueView {
    mission: Mission,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    comment_editor: Entity<editor::Editor>,
    /// One per live worker thread, so a permission request appears the moment
    /// a worker raises it rather than at the next refresh.
    _worker_subscriptions: Vec<Subscription>,
    _panel_observation: Option<Subscription>,
}

impl MissionQueueView {
    pub fn deploy(
        mission: Mission,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let existing = workspace
            .items_of_type::<MissionQueueView>(cx)
            .find(|view| view.read(cx).mission.id == mission.id);
        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
            return existing;
        }

        let panel = workspace.panel::<AgentPanel>(cx);
        let weak_workspace = workspace.weak_handle();
        let view = cx.new(|cx| MissionQueueView::new(mission, weak_workspace, panel, window, cx));
        workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
        view
    }

    fn new(
        mission: Mission,
        workspace: WeakEntity<Workspace>,
        agent_panel: Option<Entity<AgentPanel>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let comment_editor = cx.new(|cx| {
            let mut editor = editor::Editor::auto_height(1, 6, window, cx);
            editor.set_placeholder_text("Reply to the mission — goes to every worker", window, cx);
            editor
        });

        let mut this = Self {
            mission,
            workspace,
            focus_handle: cx.focus_handle(),
            comment_editor,
            _worker_subscriptions: Vec::new(),
            _panel_observation: None,
        };

        if let Some(panel) = agent_panel {
            this._panel_observation = Some(cx.observe(&panel, |this, _, cx| {
                this.sync_worker_subscriptions(cx);
            }));
        }
        // Deferred because `deploy` runs inside a `Workspace` update, and the
        // snapshot this needs has to read that same workspace back out.
        cx.spawn(async move |this, cx| {
            this.update(cx, |this, cx| this.sync_worker_subscriptions(cx))
                .ok();
        })
        .detach();
        this
    }

    fn sync_worker_subscriptions(&mut self, cx: &mut Context<Self>) {
        self._worker_subscriptions = mission_snapshot(&self.mission, &self.workspace, cx)
            .workers
            .into_iter()
            .filter_map(|worker| worker.thread)
            .map(|thread| cx.observe(&thread, |_, _, cx| cx.notify()))
            .collect();
        cx.notify();
    }

    /// Posts the comment to every worker in the Mission as an ordinary user
    /// message. There is no Mission-level message bus: "visible to all
    /// workers" means Zed sends it to each of them.
    fn post_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let comment = self.comment_editor.read(cx).text(cx).trim().to_string();
        if comment.is_empty() {
            return;
        }

        let workers = mission_snapshot(&self.mission, &self.workspace, cx).workers;
        let mut delivered = false;
        for worker in workers {
            let Some(thread) = worker.thread else {
                continue;
            };
            send_to_worker(&thread, comment.clone(), cx);
            delivered = true;
        }

        if delivered {
            self.comment_editor
                .update(cx, |editor, cx| editor.clear(window, cx));
        }
        cx.notify();
    }

    /// Opens the diff for a contended file. The Mission's workers share one
    /// working tree, so there is no second revision to diff against; the
    /// honest thing to show is the first author's diff versus HEAD, with the
    /// contention called out beside it.
    fn open_conflict_diff(&mut self, file_name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let snapshot = mission_snapshot(&self.mission, &self.workspace, cx);
        let Some(thread) = snapshot
            .changes
            .iter()
            .find(|change| change.name == file_name)
            .and_then(|change| change.workers.first())
            .and_then(|author| {
                snapshot
                    .workers
                    .iter()
                    .find(|worker| &worker.label == author)?
                    .thread
                    .clone()
            })
        else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            AgentDiffPane::deploy_in_workspace(thread, workspace, window, cx);
        });
    }
}

impl MissionQueueView {
    fn render_permission_card(&self, worker: &WorkerRow, cx: &mut Context<Self>) -> AnyElement {
        let Some(permission) = worker.permission.clone() else {
            return div().into_any_element();
        };
        let Some(thread) = worker.thread.clone() else {
            return div().into_any_element();
        };
        let role = worker.label.clone();
        let metadata = worker.metadata.clone();
        let workspace = self.workspace.clone();

        let button = |id: SharedString,
                      text: &'static str,
                      option: Option<(
            agent_client_protocol::schema::v1::PermissionOptionId,
            agent_client_protocol::schema::v1::PermissionOptionKind,
        )>| {
            let thread = thread.clone();
            let tool_call_id = permission.tool_call_id.clone();
            option.map(move |option| {
                Button::new(id, text)
                    .label_size(LabelSize::Small)
                    .on_click(move |_, _, cx| {
                        authorize_worker(&thread, tool_call_id.clone(), option.clone(), cx);
                    })
            })
        };

        v_flex()
            .w_full()
            .gap_1p5()
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().status().warning_border)
            .child(
                Label::new(format!("{role} asked"))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(Label::new(permission.label.clone()).size(LabelSize::Small))
            .child(
                h_flex()
                    .gap_1()
                    .children(button(
                        format!("queue-allow-{}", permission.tool_call_id.0).into(),
                        "Allow",
                        permission.allow.clone(),
                    ))
                    .children(button(
                        format!("queue-reject-{}", permission.tool_call_id.0).into(),
                        "Reject",
                        permission.reject.clone(),
                    ))
                    .child(
                        Button::new(
                            SharedString::from(format!(
                                "queue-open-session-{}",
                                metadata.thread_id.to_key_string()
                            )),
                            "Open session",
                        )
                        .label_size(LabelSize::Small)
                        .on_click(move |_, window, cx| {
                            open_worker_session(&workspace, &metadata, window, cx);
                        }),
                    ),
            )
            .into_any_element()
    }

    fn render_conflict_card(&self, change: &MissionChange, cx: &mut Context<Self>) -> AnyElement {
        let name = change.name.clone();
        let authors = change.workers.join(" vs ");
        let change_name = change.name.clone();

        v_flex()
            .w_full()
            .gap_1p5()
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().status().warning_border)
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .child(
                        Icon::new(IconName::GitMergeConflict)
                            .size(IconSize::XSmall)
                            .color(Color::Warning),
                    )
                    .child(
                        Label::new(format!("Conflict · {authors}"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .child(
                Label::new(format!("Both workers edited {name}."))
                    .size(LabelSize::Small),
            )
            .child(
                h_flex().gap_1().child(
                    Button::new(
                        SharedString::from(format!("queue-open-diff-{name}")),
                        "Open diff",
                    )
                        .start_icon(Icon::new(IconName::FileDiff))
                        .label_size(LabelSize::Small)
                        .tooltip(Tooltip::text(
                            "The Mission's workers share one working tree, so this shows the first author's diff against HEAD",
                        ))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_conflict_diff(&change_name, window, cx);
                        })),
                ),
            )
            .into_any_element()
    }

    fn render_standup(&self, snapshot: &MissionSnapshot, cx: &mut Context<Self>) -> AnyElement {
        let mut section = v_flex()
            .w_full()
            .child(section_header("STANDUP", "状态汇报", None));

        for worker in &snapshot.workers {
            let mut meta = worker.label.to_string();
            if let Some(tokens) = worker.tokens {
                meta.push_str(&format!(" · {} tok", format_token_count(tokens)));
            }
            if let Some(cost) = worker.cost {
                meta.push_str(&format!(" · ${cost:.2}"));
            }

            section = section.child(
                v_flex()
                    .w_full()
                    .px_4()
                    .py_1p5()
                    .gap_0p5()
                    .child(match &worker.summary {
                        Some(summary) => Label::new(summary.clone()).size(LabelSize::Small),
                        None => Label::new("Nothing reported yet")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    })
                    .child(Label::new(meta).size(LabelSize::XSmall).color(Color::Muted)),
            );
        }

        let mut total = format!("{} tok", format_token_count(snapshot.total_tokens()));
        if let Some(cost) = snapshot.total_cost() {
            total.push_str(&format!(" · ${cost:.2}"));
        }
        section = section.child(
            h_flex()
                .w_full()
                .px_4()
                .py_2()
                .border_t_1()
                .border_color(cx.theme().colors().border)
                .child(
                    Label::new("Mission total")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(div().flex_1())
                .child(
                    Label::new(total)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
        );

        section.into_any_element()
    }
}

impl Render for MissionQueueView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snapshot = mission_snapshot(&self.mission, &self.workspace, cx);
        let blocked: Vec<AnyElement> = snapshot
            .blocked()
            .map(|worker| self.render_permission_card(worker, cx))
            .collect();
        let conflicts: Vec<AnyElement> = snapshot
            .contended()
            .map(|change| self.render_conflict_card(change, cx))
            .collect();
        let asked_count = blocked.len() + conflicts.len();
        let standup = self.render_standup(&snapshot, cx);
        let can_comment = snapshot
            .workers
            .iter()
            .any(|worker| worker.thread.is_some());

        v_flex()
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .track_focus(&self.focus_handle)
            .child(
                h_flex()
                    .w_full()
                    .flex_none()
                    .items_center()
                    .gap_1p5()
                    .px_4()
                    .py_3()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(Indicator::dot().color(Color::Success))
                    .child(Label::new(snapshot.human.name.clone()))
                    .child(
                        Label::new("· you · Human teammate · owner & reviewer")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(
                v_flex()
                    .id("mission-queue-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(section_header(
                        "ASKED OF YOU",
                        "待你处理",
                        Some(asked_count.to_string().into()),
                    ))
                    .child(
                        v_flex()
                            .w_full()
                            .gap_2()
                            .px_4()
                            .py_2()
                            .when(asked_count == 0, |this| {
                                this.child(
                                    Label::new("Nothing is waiting on you right now.")
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                            })
                            .children(blocked)
                            .children(conflicts),
                    )
                    .child(section_header("YOUR COMMENT", "留言", None))
                    .child(
                        v_flex()
                            .w_full()
                            .gap_1()
                            .px_4()
                            .py_2()
                            .child(
                                div()
                                    .w_full()
                                    .px_2()
                                    .py_1p5()
                                    .rounded_md()
                                    .bg(cx.theme().colors().element_background)
                                    .child(self.comment_editor.clone()),
                            )
                            .child(
                                h_flex()
                                    .w_full()
                                    .items_center()
                                    .gap_1()
                                    .child(
                                        Label::new(if can_comment {
                                            "Sent to every loaded worker as a user message"
                                        } else {
                                            "No worker thread is loaded, so there is nobody to send to"
                                        })
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                    )
                                    .child(div().flex_1())
                                    .child(
                                        Button::new("mission-queue-comment", "Send")
                                            .start_icon(Icon::new(IconName::Send))
                                            .label_size(LabelSize::Small)
                                            .disabled(!can_comment)
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.post_comment(window, cx);
                                            })),
                                    ),
                            ),
                    )
                    .child(standup),
            )
    }
}

impl EventEmitter<ItemEvent> for MissionQueueView {}

impl Focusable for MissionQueueView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for MissionQueueView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "My queue".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Person))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Mission Queue Opened")
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

fn empty_body(message: &'static str, _cx: &App) -> AnyElement {
    div()
        .p_4()
        .child(
            Label::new(message)
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
        .into_any_element()
}

// --- Status bar -------------------------------------------------------------

/// The design's window footer: which Mission is running, how many agents are
/// working, and how many of them are waiting on the user. Auto-hides when the
/// workspace has no Mission, so it costs nothing to users who don't run them.
pub struct MissionStatusIndicator {
    workspace: WeakEntity<Workspace>,
    mission: Option<Mission>,
    _refresh: gpui::Task<()>,
}

impl MissionStatusIndicator {
    pub fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        Self {
            workspace,
            mission: None,
            _refresh: Self::spawn_refresh(cx),
        }
    }

    /// Missions are created rarely and `ThreadMetadataStore` publishes no
    /// change events, so the indicator re-reads the Mission list on a slow
    /// timer rather than trying to observe a store that can't be observed.
    /// Everything that changes second-to-second comes from the live threads,
    /// which are read on every render.
    fn spawn_refresh(cx: &mut Context<Self>) -> gpui::Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                let missions = this
                    .update(cx, |_, cx| {
                        ThreadMetadataStore::try_global(cx)
                            .map(|store| store.read(cx).list_missions(cx))
                    })
                    .ok()
                    .flatten();

                if let Some(missions) = missions {
                    let newest = missions.await.ok().and_then(|missions| {
                        missions
                            .into_iter()
                            .max_by_key(|mission| mission.created_at)
                    });
                    if this
                        .update(cx, |this, cx| {
                            if this.mission.as_ref().map(|mission| mission.id)
                                != newest.as_ref().map(|mission| mission.id)
                            {
                                this.mission = newest;
                                cx.notify();
                            }
                        })
                        .is_err()
                    {
                        return;
                    }
                }

                cx.background_executor()
                    .timer(std::time::Duration::from_secs(5))
                    .await;
            }
        })
    }
}

impl Render for MissionStatusIndicator {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(mission) = self.mission.clone() else {
            return div();
        };
        let snapshot = mission_snapshot(&mission, &self.workspace, cx);
        let counts = snapshot.counts();
        if counts.agents == 0 {
            return div();
        }

        let mut summary = format!(
            "{} · {} {} working",
            mission.title,
            counts.running,
            if counts.running == 1 {
                "agent"
            } else {
                "agents"
            }
        );
        if counts.blocked > 0 {
            summary.push_str(&format!(" · {} waiting on you", counts.blocked));
        }

        div().child(
            Button::new("mission-status-indicator", summary)
                .label_size(LabelSize::Small)
                .color(if counts.blocked > 0 {
                    Color::Warning
                } else {
                    Color::Muted
                })
                .tooltip(Tooltip::text("Open this Mission's queue"))
                .on_click(cx.listener(move |this, _, window, cx| {
                    let (Some(workspace), mission) = (this.workspace.upgrade(), mission.clone())
                    else {
                        return;
                    };
                    workspace.update(cx, |workspace, cx| {
                        MissionQueueView::deploy(mission, workspace, window, cx);
                    });
                })),
        )
    }
}

impl workspace::StatusItemView for MissionStatusIndicator {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn workspace::ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        // Hides itself when the workspace has no Mission, so there is nothing
        // for the user to turn off.
        None
    }
}
