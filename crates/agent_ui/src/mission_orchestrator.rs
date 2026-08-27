use std::path::{Path, PathBuf};
use std::sync::Arc;

use acp_thread::{AcpThread, AgentThreadEntry, ThreadStatus, ToolCallStatus};
use agent_client_protocol::schema::v1 as acp;
use collections::HashSet;
use editor::Editor;
use fs::Fs;
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString,
    WeakEntity, Window,
};
use project::AgentServerStore;
use settings::{
    ContextServerSettingsContent, ProjectSettingsContent, update_settings_file_with_completion,
};
use ui::{
    Button, Checkbox, Color, KeyBinding, Label, LabelSize, ListItem, ListItemSpacing, Modal,
    ModalFooter, ModalHeader, Section, ToggleState, prelude::*,
};
use workspace::{ModalView, Workspace};

use crate::{
    Agent, AgentInitialContent, AgentPanel, AgentThreadSource, CreateThreadOptions,
    thread_metadata_store::{Mission, MissionId, ThreadId, ThreadMetadataStore},
};

const SHARED_CONTEXT_SERVER_ID: &str = "shared-context";
const SHARED_CONTEXT_SERVER_BINARY: &str = "shared-context-mcp";

/// Absolute path to the `shared-context-mcp` binary we ship.
///
/// Deliberately not a bare command name. A bare name is resolved against
/// `PATH` at spawn time, which in a packaged build fails --- the binary lives
/// inside the bundle, not on `PATH` --- and, worse, succeeds against anything
/// else on `PATH` that happens to share the name, which Zed would then run with
/// the user's privileges. Every candidate below is a location the bundle
/// scripts put the binary in themselves (see `script/bundle-*`).
fn shared_context_server_path(cx: &App) -> Option<PathBuf> {
    // macOS bundle: `Contents/MacOS`, beside `zed`, `cli` and `git`. Asking
    // NSBundle is what `zed::main` does to find the bundled git binary.
    if cfg!(target_os = "macos") {
        if let Ok(path) = cx.path_for_auxiliary_executable(SHARED_CONTEXT_SERVER_BINARY) {
            if path.exists() {
                return Some(path);
            }
        }
    }

    // Everywhere else, and for `cargo run`: beside the running executable
    // (Linux installs put both under `libexec/`, dev builds under `target/`),
    // then `bin/` below it, which is the Windows installer's layout.
    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?;
    let file_name = format!(
        "{SHARED_CONTEXT_SERVER_BINARY}{}",
        std::env::consts::EXE_SUFFIX
    );
    [
        exe_dir.join(&file_name),
        exe_dir.join("bin").join(&file_name),
    ]
    .into_iter()
    .find(|candidate| candidate.exists())
}

/// Aggregated state of all threads assigned to a Mission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MissionState {
    Created,
    Running,
    Waiting,
    Completed,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MissionThreadState {
    Created,
    Running,
    Waiting,
    Completed,
    Failed,
}

/// Aggregates thread states with failures first, then user-blocked work, active work,
/// unstarted work, and finally all-completed work.
pub fn aggregate_mission_state(
    thread_states: impl IntoIterator<Item = MissionThreadState>,
) -> MissionState {
    let thread_states = thread_states.into_iter().collect::<Vec<_>>();
    if thread_states.is_empty() || thread_states.contains(&MissionThreadState::Created) {
        return if thread_states.is_empty() {
            MissionState::Created
        } else if thread_states.contains(&MissionThreadState::Failed) {
            MissionState::Failed
        } else if thread_states.contains(&MissionThreadState::Waiting) {
            MissionState::Waiting
        } else if thread_states.contains(&MissionThreadState::Running) {
            MissionState::Running
        } else {
            MissionState::Created
        };
    }
    if thread_states.contains(&MissionThreadState::Failed) {
        MissionState::Failed
    } else if thread_states.contains(&MissionThreadState::Waiting) {
        MissionState::Waiting
    } else if thread_states.contains(&MissionThreadState::Running) {
        MissionState::Running
    } else {
        MissionState::Completed
    }
}

/// Derives a Mission state from the live `AcpThread` entities held by an `AgentPanel`.
pub fn mission_state(panel: &AgentPanel, mission_id: MissionId, cx: &App) -> MissionState {
    let Some(metadata_store) = ThreadMetadataStore::try_global(cx) else {
        return MissionState::Created;
    };

    let thread_states = metadata_store
        .read(cx)
        .entries()
        .filter(|metadata| metadata.mission_id == Some(mission_id))
        .map(|metadata| thread_mission_state(panel, metadata.thread_id, cx));

    aggregate_mission_state(thread_states)
}

/// Derives a single thread's state from its live `AcpThread` entity, if the
/// `AgentPanel` currently holds one for it. Used both by `mission_state`
/// (aggregated across a Mission's threads) and directly by UI that shows a
/// per-thread status alongside a Mission's aggregate, so callers don't need
/// to re-derive `thread_state`'s logic themselves.
pub fn thread_mission_state(
    panel: &AgentPanel,
    thread_id: ThreadId,
    cx: &App,
) -> MissionThreadState {
    let Some(view) = panel.conversation_view_for_id(&thread_id, cx) else {
        return MissionThreadState::Created;
    };
    let Some(thread) = view.read(cx).root_thread(cx) else {
        return MissionThreadState::Created;
    };
    thread_state(thread.read(cx))
}

/// A thread's history accumulates every turn it has ever run, but a failed
/// or rejected tool call from a turn that has since completed successfully
/// says nothing about the worker's current state. Scoping the failure scan
/// to entries at or after the last `UserMessage` keeps `Failed` tied to the
/// turn actually in progress (or most recently finished), matching
/// `AcpThread::had_error`, which `run_turn` resets at the start of each turn.
fn thread_state(thread: &AcpThread) -> MissionThreadState {
    let entries = thread.entries();
    let current_turn_start = entries
        .iter()
        .rposition(|entry| matches!(entry, AgentThreadEntry::UserMessage(_)))
        .unwrap_or(0);
    let current_turn_failed = entries[current_turn_start..].iter().any(|entry| {
        matches!(
            entry,
            AgentThreadEntry::ToolCall(call)
                if matches!(call.status, ToolCallStatus::Failed | ToolCallStatus::Rejected)
        )
    });

    if thread.had_error() || current_turn_failed {
        MissionThreadState::Failed
    } else if thread.is_waiting_for_confirmation() {
        MissionThreadState::Waiting
    } else if thread.status() == ThreadStatus::Generating {
        MissionThreadState::Running
    } else if thread.entries().is_empty() || thread.is_draft_thread() {
        MissionThreadState::Created
    } else {
        MissionThreadState::Completed
    }
}

pub(crate) fn ensure_shared_context_server(
    settings: &mut ProjectSettingsContent,
    server_path: &Path,
    db_path: &Path,
) -> bool {
    if let Some(ContextServerSettingsContent::Stdio {
        enabled,
        remote,
        command,
    }) = settings.context_servers.get_mut(SHARED_CONTEXT_SERVER_ID)
    {
        let legacy_command = *enabled
            && !*remote
            && command.args.is_empty()
            && command.timeout.is_none()
            && command
                .env
                .as_ref()
                .is_none_or(collections::HashMap::is_empty)
            && command.path.file_name().is_some_and(|name| {
                name == SHARED_CONTEXT_SERVER_BINARY
                    && (command.path.is_relative() || command.path == server_path)
            });
        if legacy_command {
            command.path = server_path.to_path_buf();
            command.env = Some(collections::HashMap::from_iter([(
                shared_context::DB_PATH_ENV_VAR.to_string(),
                db_path.to_string_lossy().into_owned(),
            )]));
            return true;
        }
        return false;
    }

    settings.context_servers.insert(
        SHARED_CONTEXT_SERVER_ID.into(),
        ContextServerSettingsContent::Stdio {
            enabled: true,
            remote: false,
            command: context_server::ContextServerCommand {
                path: server_path.to_path_buf(),
                args: Vec::new(),
                // Pin the child to the same database this Zed reads. Without
                // it, a Zed started with `--user-data-dir` and the MCP servers
                // it spawns open different files and the two halves of a
                // Mission stop seeing each other; see
                // `shared_context::DB_PATH_ENV_VAR`.
                env: Some(collections::HashMap::from_iter([(
                    shared_context::DB_PATH_ENV_VAR.to_string(),
                    db_path.to_string_lossy().into_owned(),
                )])),
                timeout: None,
            },
        },
    );
    true
}

fn register_shared_context_server(
    fs: Arc<dyn Fs>,
    cx: &mut App,
) -> futures::channel::oneshot::Receiver<anyhow::Result<()>> {
    let server_path = shared_context_server_path(cx);
    let db_path = shared_context::default_db_path();
    update_settings_file_with_completion(fs, cx, move |settings, _| {
        let Some(server_path) = server_path.as_deref() else {
            // A user's existing custom or disabled configuration must remain
            // usable even when this build does not contain the bundled helper.
            // A missing entry cannot be made valid without that binary, so
            // leave the user's settings untouched rather than writing a
            // command that is guaranteed to fail.
            if !settings
                .project
                .context_servers
                .contains_key(SHARED_CONTEXT_SERVER_ID)
            {
                log::error!(
                    "could not locate the {SHARED_CONTEXT_SERVER_BINARY} binary beside Zed"
                );
            }
            return;
        };
        ensure_shared_context_server(&mut settings.project, server_path, &db_path);
    })
}

fn mission_prompt(mission: &Mission, role: &str) -> String {
    // `role` is repeated inside `shared_context` on purpose. The tool schemas
    // describe the argument, but the value only exists here, and a row recorded
    // without it cannot be attributed to this worker afterwards --- there is
    // nothing else on the row to recover it from.
    format!(
        r#"<zed-mission-context>
mission_id: {mission_id}
title: {title}
role: {role}
shared_context: "Use record_decision, record_artifact, record_evidence, and get_mission_context with this mission_id when sharing information across Harnesses. Pass role: '{role}' on every record_* call so your work is attributed to you, and an author naming yourself (e.g. 'claude-code')."
</zed-mission-context>"#,
        mission_id = mission.id.to_key_string(),
        title = mission.title,
        role = role,
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MissionThreadSpec {
    agent: Agent,
    role: String,
}

fn selected_thread_specs(entries: &[MissionAgentEntry], cx: &App) -> Vec<MissionThreadSpec> {
    entries
        .iter()
        .filter(|entry| entry.selected)
        .map(|entry| MissionThreadSpec {
            agent: entry.agent.clone(),
            role: entry.role_editor.read(cx).text(cx).trim().to_string(),
        })
        .filter(|entry| !entry.role.is_empty())
        .collect()
}

/// Roles are how a Mission's workers are matched up for conflict authorship,
/// Evidence, and Dashboard filtering (see `mission_panel::worker_label` and
/// friends), so two workers sharing one normalized role would silently merge
/// into a single identity in those views. Comparison ignores case and
/// surrounding whitespace, matching how `role`s are trimmed when saved.
fn normalized_role(role: &str) -> String {
    role.trim().to_lowercase()
}

/// The first role in `specs` that either repeats within `specs` itself, or
/// collides with a role already used by a live thread in `mission_id`
/// (relevant when adding workers to an existing Mission). `None` when every
/// role is unique.
fn duplicate_role(
    specs: &[MissionThreadSpec],
    mission_id: Option<MissionId>,
    cx: &App,
) -> Option<String> {
    let mut seen: HashSet<String> = HashSet::default();
    if let Some(mission_id) = mission_id
        && let Some(store) = ThreadMetadataStore::try_global(cx)
    {
        seen.extend(
            store
                .read(cx)
                .entries()
                .filter(|metadata| metadata.mission_id == Some(mission_id))
                .filter_map(|metadata| metadata.role.as_deref())
                .map(normalized_role),
        );
    }

    for spec in specs {
        let key = normalized_role(&spec.role);
        if !seen.insert(key) {
            return Some(spec.role.clone());
        }
    }
    None
}

#[derive(Clone)]
struct MissionAgentEntry {
    agent: Agent,
    label: SharedString,
    selected: bool,
    role_editor: Entity<Editor>,
}

pub struct MissionOrchestratorModal {
    focus_handle: FocusHandle,
    panel: WeakEntity<AgentPanel>,
    fs: Arc<dyn Fs>,
    /// Set when the modal is adding workers to a Mission that already exists,
    /// which is the "New worker…" path out of the Mission sidebar. `None` is
    /// the original flow: create the Mission and its first workers together.
    existing_mission: Option<Mission>,
    title_editor: Entity<Editor>,
    agents: Vec<MissionAgentEntry>,
    selected_index: Option<usize>,
    creating: bool,
    error: Option<SharedString>,
}

impl MissionOrchestratorModal {
    pub fn new(
        panel: WeakEntity<AgentPanel>,
        fs: Arc<dyn Fs>,
        agent_server_store: Entity<AgentServerStore>,
        existing_mission: Option<Mission>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let title_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Mission title", window, cx);
            if let Some(mission) = &existing_mission {
                editor.set_text(mission.title.clone(), window, cx);
                editor.set_read_only(true);
            }
            editor
        });

        let mut agents = vec![MissionAgentEntry {
            agent: Agent::NativeAgent,
            label: "Zed Agent".into(),
            selected: true,
            role_editor: cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_text("Zed Agent", window, cx);
                editor
            }),
        }];

        let external_agents = {
            let store = agent_server_store.read(cx);
            store
                .external_agents()
                .map(|agent_id| {
                    (
                        agent_id.clone(),
                        store
                            .agent_display_name(agent_id)
                            .unwrap_or_else(|| agent_id.0.clone()),
                    )
                })
                .collect::<Vec<_>>()
        };
        for (agent_id, label) in external_agents {
            let role_editor = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_text(label.clone(), window, cx);
                editor
            });
            agents.push(MissionAgentEntry {
                agent: Agent::Custom { id: agent_id },
                label,
                selected: false,
                role_editor,
            });
        }

        Self {
            focus_handle: cx.focus_handle(),
            panel,
            fs,
            existing_mission,
            title_editor,
            agents,
            selected_index: Some(0),
            creating: false,
            error: None,
        }
    }

    fn toggle_selected_agent(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.creating {
            return;
        }
        if let Some(entry) = self.agents.get_mut(index) {
            entry.selected = !entry.selected;
            cx.notify();
        }
    }

    fn select_next(&mut self, _: &menu::SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
        if self.agents.is_empty() {
            return;
        }
        self.selected_index = Some(match self.selected_index {
            Some(index) if index + 1 < self.agents.len() => index + 1,
            _ => 0,
        });
        cx.notify();
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.agents.is_empty() {
            return;
        }
        self.selected_index = Some(match self.selected_index {
            Some(index) if index > 0 => index - 1,
            _ => self.agents.len() - 1,
        });
        cx.notify();
    }

    fn confirm_selection(
        &mut self,
        _: &menu::Confirm,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(index) = self.selected_index {
            self.toggle_selected_agent(index, cx);
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn create_mission(
        &mut self,
        _: &menu::SecondaryConfirm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.creating {
            return;
        }

        let title = self.title_editor.read(cx).text(cx).trim().to_string();
        let specs = selected_thread_specs(&self.agents, cx);
        let existing_mission = self.existing_mission.clone();
        if existing_mission.is_none() && title.is_empty() {
            self.error = Some("Enter a Mission title.".into());
            cx.notify();
            return;
        }
        if specs.is_empty() {
            self.error = Some("Select at least one Harness and provide a role.".into());
            cx.notify();
            return;
        }
        if let Some(duplicate) = duplicate_role(
            &specs,
            existing_mission.as_ref().map(|mission| mission.id),
            cx,
        ) {
            self.error = Some(
                format!("\"{duplicate}\" is already a role in this Mission. Roles must be unique.")
                    .into(),
            );
            cx.notify();
            return;
        }

        let settings_completion = register_shared_context_server(self.fs.clone(), cx);
        self.creating = true;
        self.error = None;

        let create_task = existing_mission.is_none().then(|| {
            ThreadMetadataStore::global(cx)
                .read(cx)
                .create_mission(title, cx)
        });
        let panel = self.panel.clone();
        cx.spawn_in(window, async move |this, cx| {
            let settings_result = settings_completion
                .await
                .map_err(|error| anyhow::anyhow!("Could not update Mission settings: {error}"))
                .and_then(|result| result);
            if let Err(error) = settings_result {
                this.update(cx, |this, cx| {
                    this.creating = false;
                    this.error = Some(format!("Could not create Mission: {error:#}").into());
                    cx.notify();
                })?;
                return anyhow::Ok(());
            }

            let mission = match existing_mission {
                Some(mission) => mission,
                None => {
                    let Some(create_task) = create_task else {
                        return anyhow::Ok(());
                    };
                    match create_task.await {
                        Ok(mission) => mission,
                        Err(error) => {
                            this.update(cx, |this, cx| {
                                this.creating = false;
                                this.error =
                                    Some(format!("Could not create Mission: {error:#}").into());
                                cx.notify();
                            })?;
                            return anyhow::Ok(());
                        }
                    }
                }
            };

            this.update_in(cx, |_, window, cx| {
                let Some(panel) = panel.upgrade() else {
                    return;
                };
                panel.update(cx, |panel, cx| {
                    for spec in specs {
                        let prompt = mission_prompt(&mission, &spec.role);
                        let thread_id = panel.create_thread_with_options(
                            CreateThreadOptions {
                                title: Some(mission.title.clone().into()),
                                initial_content: Some(AgentInitialContent::ContentBlock {
                                    blocks: vec![acp::ContentBlock::Text(acp::TextContent::new(
                                        prompt,
                                    ))],
                                    auto_submit: true,
                                }),
                                agent: Some(spec.agent),
                                ..CreateThreadOptions::default()
                            },
                            AgentThreadSource::AgentPanel,
                            window,
                            cx,
                        );
                        // One call, no subscription. `set_thread_mission`
                        // parks the assignment when the thread's metadata entry
                        // does not exist yet (the usual case for an external
                        // Harness, whose connection is still coming up) and the
                        // store applies it atomically when it creates the row.
                        //
                        // This used to also re-assert the assignment on every
                        // `RootThreadUpdated` from a never-released
                        // subscription, which papered over the race at the cost
                        // of silently reverting any later reassignment --- so
                        // "move this worker to another Mission" could not be
                        // built on top of it.
                        ThreadMetadataStore::global(cx).update(cx, |store, cx| {
                            store.set_thread_mission(
                                thread_id,
                                Some(mission.id),
                                Some(spec.role),
                                cx,
                            );
                        });
                    }
                });
                cx.emit(DismissEvent);
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }
}

impl EventEmitter<DismissEvent> for MissionOrchestratorModal {}

impl Focusable for MissionOrchestratorModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for MissionOrchestratorModal {}

impl Render for MissionOrchestratorModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let adding_worker = self.existing_mission.is_some();
        let agent_rows = self
            .agents
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let checkbox_state = if entry.selected {
                    ToggleState::Selected
                } else {
                    ToggleState::Unselected
                };
                let focused = self.selected_index == Some(index);
                ListItem::new(("mission-agent", index))
                    .spacing(ListItemSpacing::Sparse)
                    .focused(focused)
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .child(Checkbox::new(
                                ("mission-agent-checkbox", index),
                                checkbox_state,
                            ))
                            .child(Label::new(entry.label.clone()).size(LabelSize::Small))
                            .child(div().flex_1().child(entry.role_editor.clone())),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_selected_agent(index, cx);
                    }))
            })
            .collect::<Vec<_>>();

        v_flex()
            .id("mission-orchestrator-modal")
            .key_context("MissionOrchestratorModal")
            .w(rems(40.))
            .elevation_3(cx)
            .overflow_hidden()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm_selection))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::create_mission))
            .child(
                Modal::new("create-mission", None)
                    .header(
                        ModalHeader::new()
                            .headline(if adding_worker {
                                "Add Worker"
                            } else {
                                "Create Mission"
                            })
                            .description(if adding_worker {
                                "Choose the Harnesses and roles to add to this Mission."
                            } else {
                                "Choose the Harnesses and roles for this Mission."
                            })
                            .show_dismiss_button(true),
                    )
                    .section(
                        Section::new()
                            .child(Label::new("Mission title").size(LabelSize::Small))
                            .child(
                                div()
                                    .mt_1()
                                    .border_1()
                                    .rounded_md()
                                    .child(self.title_editor.clone()),
                            ),
                    )
                    .section(
                        Section::new()
                            .child(Label::new("Harnesses and roles").size(LabelSize::Small))
                            .child(
                                v_flex()
                                    .id("mission-agent-list")
                                    .mt_1()
                                    .max_h(rems_from_px(320.0_f32))
                                    .overflow_y_scroll()
                                    .children(agent_rows),
                            ),
                    )
                    .when_some(self.error.clone(), |this, error| {
                        this.section(
                            Section::new().child(
                                Label::new(error).size(LabelSize::Small).color(Color::Error),
                            ),
                        )
                    })
                    .footer(
                        ModalFooter::new().end_slot(
                            Button::new(
                                "create-mission",
                                if adding_worker {
                                    "Add Worker"
                                } else {
                                    "Create Mission"
                                },
                            )
                            .loading(self.creating)
                            .disabled(self.creating)
                            .key_binding(
                                KeyBinding::for_action(&menu::SecondaryConfirm, cx)
                                    .map(|binding| binding.size(rems_from_px(12.0_f32))),
                            )
                            .on_click(cx.listener(
                                |this, _, window, cx| {
                                    this.create_mission(&menu::SecondaryConfirm, window, cx);
                                },
                            )),
                        ),
                    ),
            )
    }
}

pub fn show_create_mission_modal(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    show_mission_modal(workspace, None, window, cx);
}

/// Opens the orchestrator so the user can add another Harness to a Mission
/// that already exists. Reached from the Mission sidebar's "New worker…".
pub fn add_worker_to_mission(
    mission: Mission,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    workspace
        .update(cx, |workspace, cx| {
            show_mission_modal(workspace, Some(mission), window, cx);
        })
        .ok();
}

fn show_mission_modal(
    workspace: &mut Workspace,
    existing_mission: Option<Mission>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
        return;
    };
    let project = workspace.project().clone();
    let agent_server_store = project.read(cx).agent_server_store().clone();
    let fs = project.read(cx).fs().clone();
    workspace.toggle_modal(window, cx, |window, cx| {
        MissionOrchestratorModal::new(
            panel.downgrade(),
            fs,
            agent_server_store,
            existing_mission,
            window,
            cx,
        )
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use settings::ProjectSettingsContent;

    #[test]
    fn mission_state_aggregation_prioritizes_actionable_states() {
        assert_eq!(aggregate_mission_state([]), MissionState::Created);
        assert_eq!(
            aggregate_mission_state([MissionThreadState::Completed, MissionThreadState::Running]),
            MissionState::Running
        );
        assert_eq!(
            aggregate_mission_state([MissionThreadState::Completed, MissionThreadState::Waiting]),
            MissionState::Waiting
        );
        assert_eq!(
            aggregate_mission_state([MissionThreadState::Completed, MissionThreadState::Failed]),
            MissionState::Failed
        );
        assert_eq!(
            aggregate_mission_state([MissionThreadState::Completed, MissionThreadState::Created]),
            MissionState::Created
        );
        assert_eq!(
            aggregate_mission_state([MissionThreadState::Completed, MissionThreadState::Completed]),
            MissionState::Completed
        );
    }

    #[test]
    fn shared_context_server_is_added_only_when_missing() {
        let mut settings = ProjectSettingsContent::default();
        let server_path = std::env::temp_dir().join(SHARED_CONTEXT_SERVER_BINARY);
        let db_path = std::env::temp_dir().join("zed-shared-context.sqlite");
        assert!(ensure_shared_context_server(
            &mut settings,
            &server_path,
            &db_path
        ));
        assert!(!ensure_shared_context_server(
            &mut settings,
            &server_path,
            &db_path
        ));
        assert_eq!(settings.context_servers.len(), 1);

        // The two properties that actually matter, and that a bare command
        // name silently lost: an absolute path (so the bundled binary is what
        // runs, not whatever `PATH` offers) and an explicit database path (so
        // the child cannot end up on a different `shared_context.sqlite` than
        // the Zed that spawned it).
        let Some(ContextServerSettingsContent::Stdio { command, .. }) =
            settings.context_servers.get(SHARED_CONTEXT_SERVER_ID)
        else {
            panic!("shared context server was not registered as a stdio server");
        };
        assert!(
            command.path.is_absolute(),
            "expected an absolute command path, got {:?}",
            command.path
        );
        assert_eq!(command.path, server_path);
        assert_eq!(
            command
                .env
                .as_ref()
                .and_then(|env| env.get(shared_context::DB_PATH_ENV_VAR))
                .map(String::as_str),
            Some(&*db_path.to_string_lossy()),
        );
    }

    #[test]
    fn legacy_shared_context_server_is_upgraded_but_custom_and_disabled_are_preserved() {
        let server_path = std::env::temp_dir().join(SHARED_CONTEXT_SERVER_BINARY);
        let db_path = std::env::temp_dir().join("zed-shared-context.sqlite");

        let mut legacy = ProjectSettingsContent::default();
        legacy.context_servers.insert(
            SHARED_CONTEXT_SERVER_ID.into(),
            ContextServerSettingsContent::Stdio {
                enabled: true,
                remote: false,
                command: context_server::ContextServerCommand {
                    path: SHARED_CONTEXT_SERVER_BINARY.into(),
                    args: Vec::new(),
                    env: None,
                    timeout: None,
                },
            },
        );
        assert!(ensure_shared_context_server(
            &mut legacy,
            &server_path,
            &db_path
        ));
        let Some(ContextServerSettingsContent::Stdio { command, .. }) =
            legacy.context_servers.get(SHARED_CONTEXT_SERVER_ID)
        else {
            panic!("legacy shared context server should remain stdio");
        };
        assert_eq!(command.path, server_path);
        assert!(command.env.as_ref().is_some_and(|env| {
            env.get(shared_context::DB_PATH_ENV_VAR)
                == Some(&db_path.to_string_lossy().into_owned())
        }));

        let mut disabled = ProjectSettingsContent::default();
        disabled.context_servers.insert(
            SHARED_CONTEXT_SERVER_ID.into(),
            ContextServerSettingsContent::Stdio {
                enabled: false,
                remote: false,
                command: context_server::ContextServerCommand {
                    path: SHARED_CONTEXT_SERVER_BINARY.into(),
                    args: Vec::new(),
                    env: None,
                    timeout: None,
                },
            },
        );
        assert!(!ensure_shared_context_server(
            &mut disabled,
            &server_path,
            &db_path
        ));
        let Some(ContextServerSettingsContent::Stdio { command, .. }) =
            disabled.context_servers.get(SHARED_CONTEXT_SERVER_ID)
        else {
            panic!("disabled shared context server should remain stdio");
        };
        assert_eq!(command.path, Path::new(SHARED_CONTEXT_SERVER_BINARY));

        let custom_path = std::env::temp_dir().join("my-shared-context-mcp");
        let mut custom = ProjectSettingsContent::default();
        custom.context_servers.insert(
            SHARED_CONTEXT_SERVER_ID.into(),
            ContextServerSettingsContent::Stdio {
                enabled: true,
                remote: false,
                command: context_server::ContextServerCommand {
                    path: custom_path.clone(),
                    args: vec!["--custom".into()],
                    env: None,
                    timeout: None,
                },
            },
        );
        assert!(!ensure_shared_context_server(
            &mut custom,
            &server_path,
            &db_path
        ));
        let Some(ContextServerSettingsContent::Stdio { command, .. }) =
            custom.context_servers.get(SHARED_CONTEXT_SERVER_ID)
        else {
            panic!("custom shared context server should remain stdio");
        };
        assert_eq!(command.path, custom_path);
        assert_eq!(command.args, vec!["--custom".to_string()]);
    }

    #[test]
    fn mission_prompt_carries_the_identity_and_role() {
        let mission = Mission {
            id: MissionId::new(),
            title: "Ship the feature".to_string(),
            created_at: chrono::Utc::now(),
        };
        let prompt = mission_prompt(&mission, "coding");
        assert!(prompt.contains(&mission.id.to_key_string()));
        assert!(prompt.contains("title: Ship the feature"));
        assert!(prompt.contains("role: coding"));
        assert!(prompt.contains("get_mission_context"));
        // The role has to be repeated as an instruction, not just declared:
        // a row recorded without it cannot be attributed to this worker later.
        assert!(prompt.contains("Pass role: 'coding'"));
        assert!(
            !prompt.contains('\\'),
            "the prompt is a raw string; a stray backslash means an escape was \
             written that Rust did not process: {prompt}"
        );
    }
}
