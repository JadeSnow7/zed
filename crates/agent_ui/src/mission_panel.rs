//! Mission tree + Shared Context panel.
//!
//! A read-only navigation panel over Missions and their threads
//! (`thread_metadata_store`) and the Decision/Artifact/Evidence trail
//! accumulated for the selected Mission (`shared_context`). This panel does
//! not create Missions or threads (that's `mission_orchestrator`), does not
//! aggregate Mission/thread state (that's also `mission_orchestrator`, reused
//! here), and does not duplicate the primary thread list/activation UI in
//! `crates/sidebar` -- clicking a thread row here calls straight into
//! `AgentPanel::load_agent_thread`, the same entrypoint the sidebar uses.
//!
//! Refresh is pull-based: the tree reloads when the panel becomes active or
//! when the selected Mission changes, not on a live subscription. Neither
//! `ThreadMetadataStore` nor `shared_context` currently publish change
//! events, so a push-based refresh would need new plumbing in both for a
//! panel whose data changes at the pace of Mission/thread creation and
//! occasional tool calls -- infrequently enough that this is a deliberate
//! simplification, not an oversight.

use collections::{HashMap, HashSet};
use gpui::{
    Action as _, App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, ParentElement, Pixels, Render, Styled, Task, WeakEntity, Window, div, px,
};
use ui::{Color, Icon, IconName, IconSize, Label, LabelSize, ListItem, ListItemSpacing, prelude::*};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::{
    Agent, AgentPanel, AgentThreadSource, CreateMission, MissionState, MissionThreadState,
    mission_context_observer::shared_context_store,
    mission_state,
    thread_metadata_store::{Mission, MissionId, ThreadId, ThreadMetadata, ThreadMetadataStore},
    thread_mission_state,
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

/// What the Context section should show for the currently selected Mission.
#[derive(Default)]
enum MissionContextState {
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
fn mission_context_state_from_result(
    result: Option<anyhow::Result<shared_context::MissionContext>>,
) -> MissionContextState {
    match result {
        None => MissionContextState::Unavailable,
        Some(Err(_)) => MissionContextState::Unavailable,
        Some(Ok(context)) => {
            if context.decisions.is_empty() && context.artifacts.is_empty() && context.evidence.is_empty()
            {
                MissionContextState::Empty
            } else {
                MissionContextState::Populated(context)
            }
        }
    }
}

pub struct MissionPanel {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    position: DockPosition,
    tree: MissionTree,
    expanded_missions: HashSet<MissionId>,
    selected_mission: Option<MissionId>,
    selected_thread: Option<ThreadId>,
    context_state: MissionContextState,
    is_active: bool,
}

impl MissionPanel {
    pub fn new(workspace: &mut Workspace, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            workspace: workspace.weak_handle(),
            focus_handle: cx.focus_handle(),
            position: DockPosition::Right,
            tree: MissionTree::default(),
            expanded_missions: HashSet::default(),
            selected_mission: None,
            selected_thread: None,
            context_state: MissionContextState::NoSelection,
            is_active: false,
        }
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
    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
            let shared_mission_id = shared_context::MissionId::from_key_string(&mission_key).ok()?;
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

    fn toggle_mission(&mut self, mission_id: MissionId, cx: &mut Context<Self>) {
        if self.expanded_missions.contains(&mission_id) {
            self.expanded_missions.remove(&mission_id);
        } else {
            self.expanded_missions.insert(mission_id);
        }
        self.selected_mission = Some(mission_id);
        self.refresh_context(mission_id, cx);
        cx.notify();
    }

    /// Switches the main conversation view to `thread` by calling
    /// `AgentPanel::load_agent_thread` -- the same entrypoint
    /// `crates/sidebar` uses when a thread row is clicked there. This never
    /// creates a thread or a new connection; it only changes which existing
    /// thread `AgentPanel` displays.
    fn activate_thread(&mut self, thread: ThreadMetadata, window: &mut Window, cx: &mut Context<Self>) {
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

    fn dispatch_create_mission(window: &mut Window, cx: &mut App) {
        window.dispatch_action(CreateMission.boxed_clone(), cx);
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .justify_between()
            .items_center()
            .px_2()
            .py_1()
            .child(Label::new("Missions").size(LabelSize::Small).color(Color::Muted))
            .child(
                Button::new("mission-panel-new-mission", "New Mission")
                    .start_icon(Icon::new(IconName::Plus))
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(|_, _, window, cx| {
                        Self::dispatch_create_mission(window, cx);
                    })),
            )
    }

    fn render_mission_row(
        &self,
        group: &MissionGroup,
        agent_panel: Option<&Entity<AgentPanel>>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mission_id = group.mission.id;
        let is_expanded = self.expanded_missions.contains(&mission_id);
        let is_selected = self.selected_mission == Some(mission_id);
        let state = agent_panel.map(|panel| mission_state(panel.read(cx), mission_id, cx));
        let row_id = format!("mission-row-{}", mission_id.to_key_string());

        ListItem::new(row_id)
            .selectable(true)
            .focused(is_selected)
            .spacing(ListItemSpacing::Sparse)
            .toggle(Some(is_expanded))
            .on_toggle(cx.listener(move |this, _, _, cx| {
                this.toggle_mission(mission_id, cx);
            }))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_mission(mission_id, cx);
            }))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(Label::new(group.mission.title.clone()))
                    .children(state.map(mission_state_badge)),
            )
    }

    fn render_thread_row(
        &self,
        thread: &ThreadMetadata,
        agent_panel: Option<&Entity<AgentPanel>>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_selected = self.selected_thread == Some(thread.thread_id);
        let thread_clone = thread.clone();
        let state = agent_panel
            .map(|panel| thread_mission_state(panel.read(cx), thread.thread_id, cx));
        let row_id = format!("thread-row-{}", thread.thread_id.to_key_string());

        ListItem::new(row_id)
            .selectable(true)
            .focused(is_selected)
            .indent_level(1)
            .indent_step_size(px(16.))
            .spacing(ListItemSpacing::Sparse)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.activate_thread(thread_clone.clone(), window, cx);
            }))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(Icon::new(IconName::ZedAssistant).size(IconSize::XSmall).color(Color::Muted))
                    .child(Label::new(thread.display_title()).size(LabelSize::Small))
                    .children(
                        thread
                            .role
                            .clone()
                            .map(|role| Label::new(role).size(LabelSize::Small).color(Color::Muted)),
                    )
                    .children(state.map(thread_state_badge)),
            )
    }

    fn render_tree(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let agent_panel = self
            .workspace
            .upgrade()
            .and_then(|workspace| workspace.read(cx).panel::<AgentPanel>(cx));

        let mut rows = Vec::new();
        for group in &self.tree.groups {
            rows.push(
                self.render_mission_row(group, agent_panel.as_ref(), cx)
                    .into_any_element(),
            );
            if self.expanded_missions.contains(&group.mission.id) {
                for thread in &group.threads {
                    rows.push(
                        self.render_thread_row(thread, agent_panel.as_ref(), cx)
                            .into_any_element(),
                    );
                }
            }
        }

        if !self.tree.ungrouped.is_empty() {
            rows.push(
                div()
                    .px_2()
                    .py_1()
                    .child(Label::new("Ungrouped").size(LabelSize::Small).color(Color::Muted))
                    .into_any_element(),
            );
            for thread in &self.tree.ungrouped {
                rows.push(
                    self.render_thread_row(thread, agent_panel.as_ref(), cx)
                        .into_any_element(),
                );
            }
        }

        if rows.is_empty() {
            rows.push(
                div()
                    .p_2()
                    .child(
                        Label::new("No Missions yet. Create one to get started.")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .into_any_element(),
            );
        }

        v_flex()
            .id("mission-tree-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .children(rows)
    }

    fn render_context_section(&self) -> impl IntoElement {
        let content: gpui::AnyElement = match &self.context_state {
            MissionContextState::NoSelection => empty_state_label("Select a Mission to see its context."),
            MissionContextState::Loading => empty_state_label("Loading context…"),
            MissionContextState::Unavailable => {
                empty_state_label("Shared context is unavailable for this Mission.")
            }
            MissionContextState::Empty => {
                empty_state_label("No decisions, artifacts, or evidence recorded yet.")
            }
            MissionContextState::Populated(context) => v_flex()
                .gap_2()
                .child(render_context_group(
                    "Decisions",
                    context.decisions.iter().map(|decision| {
                        (decision.author.clone(), decision.value.clone(), decision.created_at)
                    }),
                ))
                .child(render_context_group(
                    "Artifacts",
                    context.artifacts.iter().map(|artifact| {
                        (
                            artifact.author.clone(),
                            format!("{}: {}", artifact.path, artifact.change_summary),
                            artifact.created_at,
                        )
                    }),
                ))
                .child(render_context_group(
                    "Evidence",
                    context.evidence.iter().map(|evidence| {
                        (evidence.author.clone(), evidence.command.clone(), evidence.created_at)
                    }),
                ))
                .into_any_element(),
        };

        v_flex()
            .id("mission-context-panel")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .gap_2()
            .p_2()
            .child(Label::new("Context").size(LabelSize::Small).color(Color::Muted))
            .child(content)
    }
}

fn empty_state_label(text: &'static str) -> gpui::AnyElement {
    div()
        .p_2()
        .child(Label::new(text).size(LabelSize::Small).color(Color::Muted))
        .into_any_element()
}

fn render_context_group(
    title: &'static str,
    entries: impl Iterator<Item = (String, String, chrono::DateTime<chrono::Utc>)>,
) -> impl IntoElement {
    let mut entries: Vec<_> = entries.collect();
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.2));

    let mut section = v_flex().gap_1().child(Label::new(title).size(LabelSize::Small));
    if entries.is_empty() {
        section = section.child(
            Label::new("None")
                .size(LabelSize::Small)
                .color(Color::Muted),
        );
    } else {
        for (author, summary, created_at) in entries {
            section = section.child(
                v_flex()
                    .px_2()
                    .child(
                        h_flex()
                            .gap_1()
                            .child(Label::new(author).size(LabelSize::Small).color(Color::Muted))
                            .child(
                                Label::new(created_at.format("%Y-%m-%d %H:%M").to_string())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    )
                    .child(Label::new(summary).size(LabelSize::Small)),
            );
        }
    }
    section
}

fn mission_state_badge(state: MissionState) -> impl IntoElement {
    let (label, color) = match state {
        MissionState::Created => ("Created", Color::Muted),
        MissionState::Running => ("Running", Color::Info),
        MissionState::Waiting => ("Waiting", Color::Warning),
        MissionState::Completed => ("Completed", Color::Success),
        MissionState::Failed => ("Failed", Color::Error),
    };
    Label::new(label).size(LabelSize::XSmall).color(color)
}

fn thread_state_badge(state: MissionThreadState) -> impl IntoElement {
    let (label, color) = match state {
        MissionThreadState::Created => ("created", Color::Muted),
        MissionThreadState::Running => ("running", Color::Info),
        MissionThreadState::Waiting => ("waiting", Color::Warning),
        MissionThreadState::Completed => ("completed", Color::Success),
        MissionThreadState::Failed => ("failed", Color::Error),
    };
    Label::new(label).size(LabelSize::XSmall).color(color)
}

impl EventEmitter<PanelEvent> for MissionPanel {}

impl Focusable for MissionPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for MissionPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("mission-panel")
            .key_context("MissionPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(self.render_header(cx))
            .child(self.render_tree(cx))
            .child(self.render_context_section())
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

    fn set_position(&mut self, position: DockPosition, _window: &mut Window, cx: &mut Context<Self>) {
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
mod tests {
    use super::*;
    use crate::test_support;
    use acp_thread::StubAgentConnection;
    use gpui::{TestAppContext, VisualTestContext};
    use crate::thread_metadata_store::WorktreePaths;
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
            group_a.threads.iter().map(|t| t.thread_id).collect::<Vec<_>>(),
            vec![thread_a1.thread_id, thread_a2.thread_id]
        );
        let group_b = tree
            .groups
            .iter()
            .find(|group| group.mission.id == mission_b.id)
            .unwrap();
        assert_eq!(
            group_b.threads.iter().map(|t| t.thread_id).collect::<Vec<_>>(),
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

    async fn setup_workspace_with_two_threads(
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

        test_support::open_thread_with_connection(&agent_panel, StubAgentConnection::new(), &mut cx);
        let thread_a = agent_panel.read_with(&cx, |panel, cx| {
            let thread_id = panel.active_thread_id(cx).unwrap();
            ThreadMetadataStore::global(cx)
                .read(cx)
                .entry(thread_id)
                .unwrap()
                .clone()
        });

        test_support::open_thread_with_connection(&agent_panel, StubAgentConnection::new(), &mut cx);
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
