use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::mem;
use std::process::Command;

use anyhow::{anyhow, Context, Result};

use crate::command::{
    apply_completion, complete_command, parse_project_command, AgentRuntime, CommandCompletion,
    CommandContext, CommandProjectKind, CompletionState, ProjectCommand,
};
use crate::daemon::protocol::ClientMessage;
use crate::input::AppAction;
use crate::projects::{
    build_project_entries, config_path_from_home, discover_projects_in,
    load_repo_sources_from_config_at, normalize_pane_cwd, parse_project_status_output,
    read_branch_name, AgentMonitorState, GitProjectProbe, ProjectAgentMonitor, ProjectEntry,
    ProjectKind, ProjectStatusSummary,
};
use crate::wezterm::{
    find_pane, listable_panes, tui_pane_id_from_env, tui_tab_layout, PaneInfo, SpawnCommand,
    SplitDirection, TuiTabLayout, WeztermClient,
};
use crate::workspace::WorkspaceSnapshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Normal,
    Forwarding,
}

#[derive(Debug, Clone)]
pub struct PaneRow {
    pub pane: PaneInfo,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct CommandInputState {
    command: String,
    completions: Vec<CommandCompletion>,
    selected_completion_index: usize,
}

#[derive(Debug, Clone)]
enum InputMode {
    Command(CommandInputState),
    Search {
        query: String,
        restore_cwd: Option<String>,
    },
}

#[derive(Debug)]
pub struct App {
    rows: Vec<PaneRow>,
    workspace: WorkspaceSnapshot,
    projects: Vec<ProjectEntry>,
    project_agent_monitors: Vec<Vec<ProjectAgentMonitor>>,
    launched_agents: BTreeMap<u64, AgentRuntime>,
    selected_project_id: Option<String>,
    outbox: Vec<ClientMessage>,
    tui_pane_id: u64,
    attached_pane_id: Option<u64>,
    mode: Mode,
    input_mode: Option<InputMode>,
    follow_mode: bool,
    follow_queue: VecDeque<u64>,
    follow_pending_handoff_from_pane_id: Option<u64>,
    last_monitor_states: BTreeMap<u64, AgentMonitorState>,
    status_message: String,
    last_error: Option<String>,
    should_quit: bool,
}

impl App {
    fn default_local_state() -> Self {
        Self {
            rows: Vec::new(),
            workspace: WorkspaceSnapshot::default(),
            projects: Vec::new(),
            project_agent_monitors: Vec::new(),
            launched_agents: BTreeMap::new(),
            selected_project_id: None,
            outbox: Vec::new(),
            tui_pane_id: tui_pane_id_from_env().unwrap_or(0),
            attached_pane_id: None,
            mode: Mode::Normal,
            input_mode: None,
            follow_mode: false,
            follow_queue: VecDeque::new(),
            follow_pending_handoff_from_pane_id: None,
            last_monitor_states: BTreeMap::new(),
            status_message: String::new(),
            last_error: None,
            should_quit: false,
        }
    }

    #[cfg(test)]
    fn load_with_projects<W: WeztermClient>(
        wezterm: &mut W,
        projects: Vec<ProjectEntry>,
    ) -> Result<Self> {
        let tui_pane_id = tui_pane_id_from_env()?;
        let panes = wezterm.list_panes()?;

        let mut app = Self::from_snapshot(workspace_snapshot_from_projects(&projects));
        app.tui_pane_id = tui_pane_id;
        app.replace_rows(panes)?;
        app.set_status(format!("Loaded {} projects", app.projects.len()));
        Ok(app)
    }

    pub fn from_snapshot(snapshot: WorkspaceSnapshot) -> Self {
        let selected_project_id = snapshot.project_order.first().cloned();
        let project_agent_monitors = snapshot_agent_monitors(&snapshot);
        Self {
            projects: project_entries_from_snapshot(&snapshot),
            project_agent_monitors,
            workspace: snapshot,
            selected_project_id,
            ..Self::default_local_state()
        }
    }

    pub fn replace_workspace(&mut self, snapshot: WorkspaceSnapshot) {
        let previous = self.selected_project_id.clone();
        self.workspace = snapshot;
        self.projects = project_entries_from_snapshot(&self.workspace);
        self.project_agent_monitors = vec![Vec::new(); self.projects.len()];
        self.selected_project_id = previous.filter(|id| self.workspace.projects.contains_key(id));
        if self.selected_project_id.is_none() {
            self.selected_project_id = self.workspace.project_order.first().cloned();
        }
        self.refresh_project_agent_monitors();
    }

    pub fn projects(&self) -> &[ProjectEntry] {
        &self.projects
    }

    pub fn project_agent_monitors(&self, project_index: usize) -> &[ProjectAgentMonitor] {
        self.project_agent_monitors
            .get(project_index)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn project_stale_reason(&self, project_index: usize) -> Option<&str> {
        let project_id = self.workspace.project_order.get(project_index)?;
        self.workspace
            .projects
            .get(project_id)?
            .freshness
            .stale_reason
            .as_deref()
    }

    pub fn project_operation_text(&self, project_index: usize) -> Option<String> {
        let project_id = self.workspace.project_order.get(project_index)?;
        let operation = &self.workspace.projects.get(project_id)?.operation;
        if operation.kind.is_empty() {
            return None;
        }

        if operation.message.is_empty() {
            Some(format!("op:{}", operation.kind))
        } else {
            Some(format!("op:{}[{}]", operation.kind, operation.message))
        }
    }

    pub fn selected_project_index(&self) -> usize {
        self.selected_project_id
            .as_deref()
            .and_then(|selected| {
                self.workspace
                    .project_order
                    .iter()
                    .position(|project_id| project_id == selected)
            })
            .unwrap_or(0)
    }

    pub fn selected_project_id(&self) -> Option<&str> {
        self.selected_project_id.as_deref()
    }

    pub fn drain_outbox(&mut self) -> Vec<ClientMessage> {
        mem::take(&mut self.outbox)
    }

    pub fn selected_project_cwd(&self) -> Option<&str> {
        self.selected_project().map(|project| project.cwd.as_str())
    }

    pub fn selected_project_name(&self) -> Option<&str> {
        self.selected_project().map(|project| project.name.as_str())
    }

    pub fn is_input_active(&self) -> bool {
        self.input_mode.is_some()
    }

    pub fn is_command_active(&self) -> bool {
        matches!(self.input_mode, Some(InputMode::Command(_)))
    }

    pub fn is_forwarding(&self) -> bool {
        self.mode == Mode::Forwarding
    }

    pub fn is_follow_mode(&self) -> bool {
        self.follow_mode
    }

    pub fn follow_queue_len(&self) -> usize {
        self.follow_queue.len()
    }

    pub fn is_search_active(&self) -> bool {
        matches!(self.input_mode, Some(InputMode::Search { .. }))
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub fn tick<W: WeztermClient>(&mut self, wezterm: &mut W) -> Result<()> {
        self.refresh(wezterm)?;
        self.reconcile_follow_mode(wezterm)?;
        self.last_monitor_states = self.monitor_states_by_pane_id();
        Ok(())
    }

    pub fn status_line(&self) -> String {
        let message = self
            .last_error
            .as_deref()
            .unwrap_or(self.status_message.as_str());
        let prefix = if self.last_error.is_some() {
            "ERROR"
        } else {
            "STATUS"
        };

        let mut line = format!("{prefix}: {message}");
        if let Some(warning) = self.workspace.warnings.last() {
            line.push_str(" | WARN: ");
            line.push_str(warning);
        }
        line
    }

    pub fn input_line(&self) -> String {
        match self.input_mode.as_ref() {
            Some(InputMode::Command(command_state)) => format!(":{}", command_state.command),
            Some(InputMode::Search { query, .. }) => format!("/{query}"),
            None if self.mode == Mode::Forwarding => match self.attached_project_agent_position() {
                Some((project_index, agent_index)) => {
                    let project_name = self
                        .projects
                        .get(project_index)
                        .map(|project| project.name.as_str())
                        .unwrap_or("-");
                    let monitor =
                        self.project_agent_monitors(project_index)[agent_index].display_text();
                    if self.follow_mode {
                        format!(
                            "Follow mode: forwarding to {monitor} for {project_name} (Esc stops forwarding, Ctrl-f turns follow off)"
                        )
                    } else {
                        format!(
                            "Forwarding keys to {monitor} for {project_name} (Left/Right to switch, Esc to return)"
                        )
                    }
                }
                None => match self.attached_pane_id {
                    Some(pane_id) => {
                        if self.follow_mode {
                            format!(
                                "Follow mode: forwarding to pane {pane_id} (Esc stops forwarding, Ctrl-f turns follow off)"
                            )
                        } else {
                            format!("Forwarding keys to pane {pane_id} (Esc to return)")
                        }
                    }
                    None => {
                        if self.follow_mode {
                            "Follow mode is active globally (Ctrl-f to turn it off)".to_string()
                        } else {
                            ": command on selected project, e.g. wt add <branch>".to_string()
                        }
                    }
                },
            },
            None => {
                if self.follow_mode {
                    "Follow mode is active globally (Ctrl-f to turn it off)".to_string()
                } else {
                    ": command on selected project, e.g. wt add <branch>".to_string()
                }
            }
        }
    }

    pub fn record_error(&mut self, error: impl Into<String>) {
        self.last_error = Some(error.into());
    }

    pub fn command_completions(&self) -> &[CommandCompletion] {
        match self.input_mode.as_ref() {
            Some(InputMode::Command(state)) => state.completions.as_slice(),
            _ => &[],
        }
    }

    pub fn selected_command_completion_index(&self) -> Option<usize> {
        match self.input_mode.as_ref() {
            Some(InputMode::Command(state)) if !state.completions.is_empty() => {
                Some(state.selected_completion_index)
            }
            _ => None,
        }
    }

    pub fn apply<W: WeztermClient>(&mut self, action: AppAction, wezterm: &mut W) -> Result<()> {
        match action {
            AppAction::ProjectMoveUp => {
                self.project_move_up();
                Ok(())
            }
            AppAction::ProjectMoveDown => {
                self.project_move_down();
                Ok(())
            }
            AppAction::StartCommandInput => self.start_command_input(),
            AppAction::StartSearchInput => {
                self.start_search_input();
                Ok(())
            }
            AppAction::ConfirmInput => self.confirm_input(wezterm),
            AppAction::CancelInput => {
                self.cancel_input();
                Ok(())
            }
            AppAction::EditInput(c) => self.edit_input(c),
            AppAction::DeleteInputChar => self.delete_input_char(),
            AppAction::NextSearchMatch => {
                self.advance_search_match(1);
                Ok(())
            }
            AppAction::PreviousSearchMatch => {
                self.advance_search_match(-1);
                Ok(())
            }
            AppAction::NextCommandCompletion => {
                self.advance_command_completion(1);
                Ok(())
            }
            AppAction::PreviousCommandCompletion => {
                self.advance_command_completion(-1);
                Ok(())
            }
            AppAction::AcceptCommandCompletion => self.accept_command_completion(),
            AppAction::AttachProjectAgent => self.attach_project_agent(wezterm),
            AppAction::ToggleFollowMode => self.toggle_follow_mode(wezterm),
            AppAction::OpenProjectIdea => self.open_project_idea(),
            AppAction::OpenProjectTerminal => {
                self.open_project_in_other_pane(wezterm, SpawnCommand::shell())
            }
            AppAction::OpenProjectEditor => {
                if self.selected_project().is_none() {
                    self.record_error("No projects found");
                }
                Ok(())
            }
            AppAction::SelectPreviousProjectAgent => self.switch_project_agent(wezterm, -1),
            AppAction::SelectNextProjectAgent => self.switch_project_agent(wezterm, 1),
            AppAction::ExitForwarding => {
                self.mode = Mode::Normal;
                self.set_status("Stopped forwarding keys");
                Ok(())
            }
            AppAction::Forward(text) => self.forward_text(wezterm, &text),
            AppAction::Quit => {
                self.should_quit = true;
                self.set_status("Quit");
                Ok(())
            }
        }
    }

    fn replace_rows(&mut self, panes: Vec<PaneInfo>) -> Result<()> {
        let layout = tui_tab_layout(&panes, self.tui_pane_id)?;
        self.attached_pane_id = match layout {
            TuiTabLayout::Attached(pane) => Some(pane.pane_id),
            _ => None,
        };

        let mut rows = listable_panes(&panes, self.tui_pane_id)
            .into_iter()
            .map(|pane| PaneRow { pane })
            .collect::<Vec<_>>();
        rows.sort_by_key(|row| (row.pane.window_id, row.pane.tab_id, row.pane.pane_id));
        self.rows = rows;
        self.launched_agents
            .retain(|pane_id, _| self.rows.iter().any(|row| row.pane.pane_id == *pane_id));
        if self.attached_pane_id.is_none() {
            self.mode = Mode::Normal;
        }

        self.refresh_project_agent_monitors();
        Ok(())
    }

    fn selected_project(&self) -> Option<&ProjectEntry> {
        self.projects.get(self.selected_project_index())
    }

    fn project_move_up(&mut self) {
        let selected_project_index = self.selected_project_index();
        if !self.projects.is_empty() && selected_project_index > 0 {
            self.set_selected_project_index(selected_project_index - 1);
        }
    }

    fn project_move_down(&mut self) {
        let selected_project_index = self.selected_project_index();
        if !self.projects.is_empty() && selected_project_index + 1 < self.projects.len() {
            self.set_selected_project_index(selected_project_index + 1);
        }
    }

    fn refresh_command_input_state(&mut self) -> Result<()> {
        let context_kind = self.selected_project().map(|project| match project.kind {
            ProjectKind::Root => CommandProjectKind::Root,
            ProjectKind::Worktree => CommandProjectKind::Worktree,
        });
        let root_cwd = self
            .selected_project()
            .map(|project| project.root_cwd.clone());
        let Some(InputMode::Command(state)) = self.input_mode.as_mut() else {
            return Ok(());
        };

        let context = CommandContext {
            project_kind: context_kind,
            root_cwd: root_cwd.as_deref(),
        };
        let CompletionState { items } = complete_command(&state.command, &context)?;
        state.completions = items;
        if state.completions.is_empty() {
            state.selected_completion_index = 0;
        } else if state.selected_completion_index >= state.completions.len() {
            state.selected_completion_index = state.completions.len() - 1;
        }
        Ok(())
    }

    fn start_command_input(&mut self) -> Result<()> {
        self.input_mode = Some(InputMode::Command(CommandInputState::default()));
        self.refresh_command_input_state()?;
        Ok(())
    }

    fn start_search_input(&mut self) {
        self.input_mode = Some(InputMode::Search {
            query: String::new(),
            restore_cwd: self.selected_project().map(|project| project.cwd.clone()),
        });
    }

    fn edit_input(&mut self, c: char) -> Result<()> {
        let mut refresh_search = false;
        let Some(input_mode) = self.input_mode.as_mut() else {
            return Ok(());
        };

        match input_mode {
            InputMode::Command(state) => state.command.push(c),
            InputMode::Search { query, .. } => {
                query.push(c);
                refresh_search = true;
            }
        }
        self.last_error = None;
        if refresh_search {
            self.refresh_search_selection();
        } else {
            self.refresh_command_input_state()?;
        }
        Ok(())
    }

    fn delete_input_char(&mut self) -> Result<()> {
        let mut refresh_search = false;
        let Some(input_mode) = self.input_mode.as_mut() else {
            return Ok(());
        };

        match input_mode {
            InputMode::Command(state) => {
                state.command.pop();
            }
            InputMode::Search { query, .. } => {
                query.pop();
                refresh_search = true;
            }
        }
        self.last_error = None;
        if refresh_search {
            self.refresh_search_selection();
        } else {
            self.refresh_command_input_state()?;
        }
        Ok(())
    }

    fn cancel_input(&mut self) {
        match self.input_mode.take() {
            Some(InputMode::Search { restore_cwd, .. }) => {
                self.restore_search_selection(restore_cwd.as_deref());
                self.set_status("Cancelled search");
            }
            Some(InputMode::Command(_)) => {
                self.set_status("Cancelled input");
            }
            None => {}
        }
    }

    fn confirm_input<W: WeztermClient>(&mut self, wezterm: &mut W) -> Result<()> {
        let Some(input_mode) = self.input_mode.clone() else {
            return Ok(());
        };

        match input_mode {
            InputMode::Command(_) => self.confirm_command_input(wezterm),
            InputMode::Search { .. } => {
                self.input_mode = None;
                self.set_status("Selected project from search");
                Ok(())
            }
        }
    }

    fn confirm_command_input<W: WeztermClient>(&mut self, wezterm: &mut W) -> Result<()> {
        if self.try_apply_selected_command_completion_on_confirm()? {
            let command = self.command_text().unwrap_or_default().trim().to_string();
            if parse_project_command(&command).is_err() {
                return Ok(());
            }
        }

        let Some(command) = self.command_text().map(str::to_string) else {
            return Ok(());
        };
        self.input_mode = None;
        self.execute_project_command(&command, wezterm)
    }

    fn command_text(&self) -> Option<&str> {
        match self.input_mode.as_ref() {
            Some(InputMode::Command(state)) => Some(state.command.as_str()),
            _ => None,
        }
    }

    fn try_apply_selected_command_completion_on_confirm(&mut self) -> Result<bool> {
        let Some(InputMode::Command(state)) = self.input_mode.as_ref() else {
            return Ok(false);
        };
        let Some(completion) = state
            .completions
            .get(state.selected_completion_index)
            .cloned()
        else {
            return Ok(false);
        };
        let Some(_) = completion.insert_text.as_deref() else {
            return Ok(false);
        };

        if !should_apply_completion_on_confirm(state, &completion) {
            return Ok(false);
        }

        self.accept_command_completion()?;
        Ok(true)
    }

    fn refresh_search_selection(&mut self) {
        let Some((query, restore_cwd)) = self.search_state() else {
            return;
        };
        if query.trim().is_empty() {
            self.restore_search_selection(restore_cwd.as_deref());
            self.last_error = None;
            return;
        }

        let matches = self.search_match_indices(&query);
        if matches.is_empty() {
            self.record_error(format!("No projects match /{query}"));
            return;
        }
        if matches.contains(&self.selected_project_index()) {
            self.last_error = None;
            return;
        }

        let restore_index = restore_cwd
            .as_deref()
            .and_then(|cwd| self.project_index_by_cwd(cwd))
            .unwrap_or(0);
        let target = matches
            .iter()
            .copied()
            .find(|index| *index >= restore_index)
            .unwrap_or(matches[0]);
        self.set_selected_project_index(target);
        self.last_error = None;
    }

    fn advance_search_match(&mut self, offset: isize) {
        let Some((query, restore_cwd)) = self.search_state() else {
            return;
        };
        if query.trim().is_empty() {
            return;
        }

        let matches = self.search_match_indices(&query);
        if matches.is_empty() {
            self.record_error(format!("No projects match /{query}"));
            return;
        }

        let current_position = matches
            .iter()
            .position(|index| *index == self.selected_project_index())
            .or_else(|| {
                let restore_index = restore_cwd
                    .as_deref()
                    .and_then(|cwd| self.project_index_by_cwd(cwd))
                    .unwrap_or(0);
                matches.iter().position(|index| *index >= restore_index)
            })
            .unwrap_or(0);
        let target_position = if offset.is_negative() {
            current_position.checked_sub(1).unwrap_or(matches.len() - 1)
        } else {
            (current_position + 1) % matches.len()
        };

        self.set_selected_project_index(matches[target_position]);
        self.last_error = None;
    }

    fn search_state(&self) -> Option<(String, Option<String>)> {
        match self.input_mode.as_ref() {
            Some(InputMode::Search { query, restore_cwd }) => {
                Some((query.clone(), restore_cwd.clone()))
            }
            _ => None,
        }
    }

    fn search_match_indices(&self, query: &str) -> Vec<usize> {
        let query = query.trim();
        if query.is_empty() {
            return Vec::new();
        }

        let query = query.to_lowercase();
        self.projects
            .iter()
            .enumerate()
            .filter_map(|(index, project)| {
                project
                    .search_label()
                    .to_lowercase()
                    .contains(&query)
                    .then_some(index)
            })
            .collect()
    }

    fn restore_search_selection(&mut self, restore_cwd: Option<&str>) {
        if let Some(index) = restore_cwd.and_then(|cwd| self.project_index_by_cwd(cwd)) {
            self.set_selected_project_index(index);
        }
    }

    fn project_index_by_cwd(&self, cwd: &str) -> Option<usize> {
        self.projects.iter().position(|project| project.cwd == cwd)
    }

    fn execute_project_command<W: WeztermClient>(
        &mut self,
        command: &str,
        wezterm: &mut W,
    ) -> Result<()> {
        let command = command.trim();
        if command.is_empty() {
            self.set_status("Cancelled command");
            return Ok(());
        }

        let parsed = parse_project_command(command)?;
        if let Some(operation) = parsed.operation_name() {
            let Some(project) = self.selected_project().cloned() else {
                self.record_error("No projects found");
                return Ok(());
            };
            self.outbox.push(ClientMessage::RunOperation {
                project_id: self.selected_project_id().unwrap_or_default().to_string(),
                operation: operation.to_string(),
                command: command.to_string(),
            });
            self.set_status(format!("Queued {operation} for {}", project.name));
            return Ok(());
        }

        match parsed {
            ProjectCommand::Agent { runtime } => {
                if let Err(error) = self.open_project_agent_tab(wezterm, runtime) {
                    self.record_error(error.to_string());
                    return Err(error);
                }
                Ok(())
            }
            ProjectCommand::Add { .. }
            | ProjectCommand::Remove
            | ProjectCommand::Merge { .. }
            | ProjectCommand::Pr { .. }
            | ProjectCommand::Land { .. }
            | ProjectCommand::GitSwitch { .. }
            | ProjectCommand::GitPull => Ok(()),
        }
    }

    fn advance_command_completion(&mut self, offset: isize) {
        let Some(InputMode::Command(state)) = self.input_mode.as_mut() else {
            return;
        };
        if state.completions.is_empty() {
            return;
        }

        let count = state.completions.len();
        state.selected_completion_index = if offset.is_negative() {
            state
                .selected_completion_index
                .checked_sub(offset.unsigned_abs())
                .unwrap_or(count - 1)
        } else {
            (state.selected_completion_index + offset as usize) % count
        };
    }

    fn accept_command_completion(&mut self) -> Result<()> {
        let Some(InputMode::Command(state)) = self.input_mode.as_mut() else {
            return Ok(());
        };
        let Some(completion) = state
            .completions
            .get(state.selected_completion_index)
            .cloned()
        else {
            return Ok(());
        };

        state.command = apply_completion(&state.command, &completion);
        self.refresh_command_input_state()
    }

    fn attach_project_agent<W: WeztermClient>(&mut self, wezterm: &mut W) -> Result<()> {
        let project_index = self.selected_project_index();
        let Some(agent_index) = self.preferred_project_agent_index(project_index) else {
            self.record_error("Start an agent for this project first");
            return Ok(());
        };

        self.attach_project_agent_at_index(wezterm, project_index, agent_index)
    }

    fn attach_project_agent_at_index<W: WeztermClient>(
        &mut self,
        wezterm: &mut W,
        project_index: usize,
        agent_index: usize,
    ) -> Result<()> {
        self.attach_project_agent_at_index_with_status(wezterm, project_index, agent_index, false)
    }

    fn attach_project_agent_at_index_with_status<W: WeztermClient>(
        &mut self,
        wezterm: &mut W,
        project_index: usize,
        agent_index: usize,
        from_follow_mode: bool,
    ) -> Result<()> {
        let project = match self.projects.get(project_index) {
            Some(project) => project.clone(),
            None => {
                self.record_error("No projects found");
                return Ok(());
            }
        };

        let Some((pane_id, monitor_label)) = self
            .project_agent_monitors(project_index)
            .get(agent_index)
            .map(|monitor| (monitor.pane_id, monitor.display_text()))
        else {
            self.record_error("Start an agent for this project first");
            return Ok(());
        };

        if !self.attach_pane_into_tui(wezterm, pane_id, true)? {
            return Ok(());
        }
        self.mode = Mode::Forwarding;
        if from_follow_mode {
            self.set_status(format!(
                "Follow attached {monitor_label} for {}",
                project.name
            ));
        } else {
            self.set_status(format!(
                "Attached {monitor_label} for {} and forwarding keys",
                project.name
            ));
        }
        Ok(())
    }

    fn preferred_project_agent_index(&self, project_index: usize) -> Option<usize> {
        let monitors = self.project_agent_monitors(project_index);
        if monitors.is_empty() {
            return None;
        }

        monitors
            .iter()
            .position(|monitor| monitor.state == AgentMonitorState::NeedsInput)
            .or(Some(0))
    }

    fn best_attachable_agent(&self) -> Option<(usize, usize)> {
        let mut first_available = None;

        for (project_index, monitors) in self.project_agent_monitors.iter().enumerate() {
            if let Some(agent_index) = monitors
                .iter()
                .position(|monitor| monitor.state == AgentMonitorState::NeedsInput)
            {
                return Some((project_index, agent_index));
            }

            if first_available.is_none() && !monitors.is_empty() {
                first_available = Some((project_index, 0));
            }
        }

        first_available
    }

    fn attached_project_agent_position(&self) -> Option<(usize, usize)> {
        let pane_id = self.attached_pane_id?;

        self.project_agent_monitors
            .iter()
            .enumerate()
            .find_map(|(project_index, monitors)| {
                monitors
                    .iter()
                    .position(|monitor| monitor.pane_id == pane_id)
                    .map(|agent_index| (project_index, agent_index))
            })
    }

    fn switch_project_agent<W: WeztermClient>(
        &mut self,
        wezterm: &mut W,
        offset: isize,
    ) -> Result<()> {
        let Some((project_index, current_agent_index)) = self.attached_project_agent_position()
        else {
            self.record_error("Attach an agent first");
            return Ok(());
        };

        let agent_count = self.project_agent_monitors(project_index).len();
        if agent_count <= 1 {
            return Ok(());
        }

        let target_agent_index = if offset.is_negative() {
            current_agent_index.saturating_sub(offset.unsigned_abs())
        } else {
            current_agent_index
                .saturating_add(offset as usize)
                .min(agent_count - 1)
        };

        if target_agent_index == current_agent_index {
            return Ok(());
        }

        self.attach_project_agent_at_index(wezterm, project_index, target_agent_index)
    }

    fn toggle_follow_mode<W: WeztermClient>(&mut self, wezterm: &mut W) -> Result<()> {
        self.follow_mode = !self.follow_mode;
        self.follow_pending_handoff_from_pane_id = None;
        self.follow_queue.clear();

        if !self.follow_mode {
            self.set_status("Follow mode OFF");
            return Ok(());
        }

        self.refresh(wezterm)?;
        self.seed_follow_queue_from_current_monitors();
        self.reconcile_follow_mode(wezterm)?;
        self.last_monitor_states = self.monitor_states_by_pane_id();
        if self.attached_pane_id.is_some() {
            return Ok(());
        }
        self.set_status("Follow mode ON");
        Ok(())
    }

    fn seed_follow_queue_from_current_monitors(&mut self) {
        self.follow_queue.clear();
        for (project_index, agent_index) in self.all_agent_positions() {
            let monitor = &self.project_agent_monitors[project_index][agent_index];
            if monitor.state == AgentMonitorState::NeedsInput {
                self.follow_queue.push_back(monitor.pane_id);
            }
        }
    }

    fn reconcile_follow_mode<W: WeztermClient>(&mut self, wezterm: &mut W) -> Result<()> {
        if !self.follow_mode {
            return Ok(());
        }

        let current_states = self.monitor_states_by_pane_id();
        self.prune_follow_queue(&current_states);
        self.enqueue_new_follow_candidates();

        if let Some(pane_id) = self.attached_pane_id {
            if current_states.get(&pane_id) == Some(&AgentMonitorState::NeedsInput) {
                self.follow_queue.retain(|queued| *queued != pane_id);
                self.follow_queue.push_front(pane_id);
            }
        }

        if let Some(pane_id) = self.follow_pending_handoff_from_pane_id {
            if current_states.get(&pane_id) == Some(&AgentMonitorState::NeedsInput) {
                return Ok(());
            }
            self.follow_pending_handoff_from_pane_id = None;
            self.follow_queue.retain(|queued| *queued != pane_id);
        }

        if matches!(
            self.attached_pane_id
                .and_then(|pane_id| current_states.get(&pane_id)),
            Some(AgentMonitorState::NeedsInput)
        ) {
            return Ok(());
        }

        let Some(next_pane_id) = self.follow_queue.front().copied() else {
            return Ok(());
        };
        if self.attached_pane_id == Some(next_pane_id) {
            return Ok(());
        }
        let Some((project_index, agent_index)) = self.agent_position_by_pane_id(next_pane_id)
        else {
            self.follow_queue.pop_front();
            return Ok(());
        };

        self.attach_project_agent_at_index_with_status(wezterm, project_index, agent_index, true)
    }

    fn prune_follow_queue(&mut self, current_states: &BTreeMap<u64, AgentMonitorState>) {
        self.follow_queue
            .retain(|pane_id| current_states.get(pane_id) == Some(&AgentMonitorState::NeedsInput));
    }

    fn enqueue_new_follow_candidates(&mut self) {
        for (project_index, agent_index) in self.all_agent_positions() {
            let monitor = &self.project_agent_monitors[project_index][agent_index];
            if monitor.state != AgentMonitorState::NeedsInput {
                continue;
            }
            if self.last_monitor_states.get(&monitor.pane_id)
                == Some(&AgentMonitorState::NeedsInput)
            {
                continue;
            }
            if self.follow_queue.contains(&monitor.pane_id) {
                continue;
            }
            self.follow_queue.push_back(monitor.pane_id);
        }
    }

    fn all_agent_positions(&self) -> Vec<(usize, usize)> {
        let mut positions = Vec::new();
        for (project_index, monitors) in self.project_agent_monitors.iter().enumerate() {
            for agent_index in 0..monitors.len() {
                positions.push((project_index, agent_index));
            }
        }
        positions
    }

    fn agent_position_by_pane_id(&self, pane_id: u64) -> Option<(usize, usize)> {
        self.project_agent_monitors
            .iter()
            .enumerate()
            .find_map(|(project_index, monitors)| {
                monitors
                    .iter()
                    .position(|monitor| monitor.pane_id == pane_id)
                    .map(|agent_index| (project_index, agent_index))
            })
    }

    fn monitor_states_by_pane_id(&self) -> BTreeMap<u64, AgentMonitorState> {
        let mut states = BTreeMap::new();
        for monitors in &self.project_agent_monitors {
            for monitor in monitors {
                states.insert(monitor.pane_id, monitor.state);
            }
        }
        states
    }

    fn attach_pane_into_tui<W: WeztermClient>(
        &mut self,
        wezterm: &mut W,
        pane_id: u64,
        refocus_tui: bool,
    ) -> Result<bool> {
        let panes = wezterm.list_panes()?;
        let _selected = find_pane(&panes, pane_id)?;
        let layout = tui_tab_layout(&panes, self.tui_pane_id)?;

        match layout {
            TuiTabLayout::Unsupported { .. } => {
                self.record_error("unsupported layout");
                return Ok(false);
            }
            TuiTabLayout::Attached(attached) if attached.pane_id == pane_id => {
                self.attached_pane_id = Some(pane_id);
                if refocus_tui {
                    wezterm.activate_pane(self.tui_pane_id)?;
                } else {
                    wezterm.activate_pane(pane_id)?;
                }
                self.refresh(wezterm)?;
                return Ok(true);
            }
            TuiTabLayout::Attached(attached) => {
                wezterm.move_pane_to_new_tab(attached.pane_id)?;
            }
            TuiTabLayout::Solo => {}
        }

        wezterm.split_pane(self.tui_pane_id, pane_id, SplitDirection::Right)?;
        if refocus_tui {
            wezterm.activate_pane(self.tui_pane_id)?;
        } else {
            wezterm.activate_pane(pane_id)?;
        }

        self.attached_pane_id = Some(pane_id);
        self.refresh(wezterm)?;

        Ok(true)
    }

    fn open_project_idea(&mut self) -> Result<()> {
        let project = match self.selected_project() {
            Some(project) => project.clone(),
            None => {
                self.record_error("No projects found");
                return Ok(());
            }
        };

        Command::new("idea")
            .arg(&project.cwd)
            .spawn()
            .with_context(|| format!("failed to launch IntelliJ IDEA for {}", project.cwd))?;
        self.set_status(format!("Opened idea for {}", project.name));
        Ok(())
    }

    fn open_project_in_other_pane<W: WeztermClient>(
        &mut self,
        wezterm: &mut W,
        command: SpawnCommand,
    ) -> Result<()> {
        let project = match self.selected_project() {
            Some(project) => project.clone(),
            None => {
                self.record_error("No projects found");
                return Ok(());
            }
        };

        let pane_id = wezterm.spawn_new_tab(self.tui_pane_id, &project.cwd, &command)?;
        if !self.attach_pane_into_tui(wezterm, pane_id, false)? {
            return Ok(());
        }
        self.set_status(format!(
            "Opened {} pane for {}",
            command.label(),
            project.name
        ));
        Ok(())
    }

    fn forward_text<W: WeztermClient>(&mut self, wezterm: &mut W, text: &str) -> Result<()> {
        let attached_pane_id = self
            .attached_pane_id
            .ok_or_else(|| anyhow!("cannot forward keys without an attached pane"))?;
        wezterm.send_text(attached_pane_id, text)?;
        if self.follow_mode
            && self.monitor_states_by_pane_id().get(&attached_pane_id)
                == Some(&AgentMonitorState::NeedsInput)
        {
            self.follow_pending_handoff_from_pane_id = Some(attached_pane_id);
        }
        self.last_error = None;
        Ok(())
    }

    fn open_project_agent_tab<W: WeztermClient>(
        &mut self,
        wezterm: &mut W,
        runtime: AgentRuntime,
    ) -> Result<()> {
        let project = match self.selected_project() {
            Some(project) => project.clone(),
            None => {
                self.record_error("No projects found");
                return Ok(());
            }
        };

        let pane_id =
            wezterm.spawn_new_tab(self.tui_pane_id, &project.cwd, &runtime.spawn_command())?;
        self.launched_agents.insert(pane_id, runtime);
        wezterm.activate_pane(self.tui_pane_id)?;
        self.refresh(wezterm)?;
        self.set_status(format!(
            "Opened {} tab for {}",
            runtime.label(),
            project.name
        ));
        Ok(())
    }

    fn refresh<W: WeztermClient>(&mut self, wezterm: &mut W) -> Result<()> {
        let panes = wezterm.list_panes()?;
        self.replace_rows(panes)?;
        Ok(())
    }

    fn set_selected_project_index(&mut self, index: usize) {
        self.selected_project_id = self.workspace.project_order.get(index).cloned();
    }

    fn refresh_project_agent_monitors(&mut self) {
        let mut monitors = snapshot_agent_monitors(&self.workspace);
        let mut monitored_pane_ids = BTreeSet::new();

        for project_monitors in &monitors {
            for monitor in project_monitors {
                monitored_pane_ids.insert(monitor.pane_id);
            }
        }

        for row in &self.rows {
            let pane_id = row.pane.pane_id;
            let Some(runtime) = self.launched_agents.get(&pane_id).copied() else {
                continue;
            };
            if monitored_pane_ids.contains(&pane_id) {
                continue;
            }

            let Some(cwd) = normalize_pane_cwd(&row.pane.cwd) else {
                continue;
            };
            let Some(project_index) = project_index_for_cwd(&self.projects, &cwd) else {
                continue;
            };

            monitors[project_index].push(ProjectAgentMonitor {
                pane_id,
                runtime,
                state: AgentMonitorState::Starting,
            });
        }

        for project_monitors in &mut monitors {
            project_monitors.sort_by_key(|monitor| {
                (
                    monitor.runtime.short_label().to_string(),
                    monitor.pane_id,
                    monitor.state.short_code(),
                )
            });
        }

        self.project_agent_monitors = monitors;
    }

    fn set_status(&mut self, status: impl Into<String>) {
        self.status_message = status.into();
        self.last_error = None;
    }
}

fn project_index_for_cwd(projects: &[ProjectEntry], cwd: &str) -> Option<usize> {
    projects
        .iter()
        .enumerate()
        .filter_map(|(index, project)| {
            let project_cwd = project.cwd.trim_end_matches('/');
            if cwd == project_cwd {
                return Some((index, project_cwd.len()));
            }

            cwd.strip_prefix(project_cwd)
                .filter(|suffix| suffix.starts_with('/'))
                .map(|_| (index, project_cwd.len()))
        })
        .max_by_key(|(_, match_len)| *match_len)
        .map(|(index, _)| index)
}

fn should_apply_completion_on_confirm(
    state: &CommandInputState,
    completion: &CommandCompletion,
) -> bool {
    let Some(insert_text) = completion.insert_text.as_deref() else {
        return false;
    };
    let current_token = current_command_token(&state.command);
    if current_token == insert_text {
        return false;
    }
    if current_token.is_empty() {
        return true;
    }

    !state
        .completions
        .iter()
        .filter_map(|item| item.insert_text.as_deref())
        .any(|item| item == current_token)
}

fn current_command_token(command: &str) -> &str {
    if command.ends_with(char::is_whitespace) {
        return "";
    }

    command.split_whitespace().last().unwrap_or("")
}

fn normalize_worktree_branch_input(input: &str) -> Option<String> {
    let branch = input.trim();
    (!branch.is_empty()).then(|| branch.to_string())
}

fn project_entries_from_snapshot(snapshot: &WorkspaceSnapshot) -> Vec<ProjectEntry> {
    snapshot
        .project_order
        .iter()
        .filter_map(|project_id| {
            let project = snapshot.projects.get(project_id)?;
            let root = snapshot.projects.get(&project.root_id).unwrap_or(project);

            Some(ProjectEntry {
                name: project.name.clone(),
                cwd: project.cwd.clone(),
                branch: project.git.branch.clone(),
                status_summary: project.git.status_summary.clone(),
                root_name: root.name.clone(),
                root_cwd: root.cwd.clone(),
                kind: project.kind.clone(),
            })
        })
        .collect()
}

fn snapshot_agent_monitors(snapshot: &WorkspaceSnapshot) -> Vec<Vec<ProjectAgentMonitor>> {
    let mut monitors = vec![Vec::new(); snapshot.project_order.len()];

    for (project_index, project_id) in snapshot.project_order.iter().enumerate() {
        let Some(project) = snapshot.projects.get(project_id) else {
            continue;
        };

        for agent in &project.agents {
            let Some(pane_id) = agent.pane_id else {
                continue;
            };
            let Some(runtime) = AgentRuntime::from_state_name(&agent.runtime) else {
                continue;
            };
            let Some(state) = snapshot_agent_monitor_state(&agent.status) else {
                continue;
            };

            monitors[project_index].push(ProjectAgentMonitor {
                pane_id,
                runtime,
                state,
            });
        }

        monitors[project_index].sort_by_key(|monitor| {
            (
                monitor.runtime.short_label().to_string(),
                monitor.pane_id,
                monitor.state.short_code(),
            )
        });
    }

    monitors
}

fn snapshot_agent_monitor_state(status: &str) -> Option<AgentMonitorState> {
    match status.trim().to_ascii_lowercase().as_str() {
        "starting" => Some(AgentMonitorState::Starting),
        "working" | "busy" | "running" | "retry" => Some(AgentMonitorState::Working),
        "needs_input" | "needs-input" | "input" | "awaiting_user" | "awaiting-user" => {
            Some(AgentMonitorState::NeedsInput)
        }
        "done" | "idle" | "complete" | "completed" => Some(AgentMonitorState::Done),
        "error" | "failed" => Some(AgentMonitorState::Error),
        _ => None,
    }
}

fn workspace_snapshot_from_projects(projects: &[ProjectEntry]) -> WorkspaceSnapshot {
    let mut snapshot = WorkspaceSnapshot::default();
    snapshot.protocol_version = 1;
    snapshot.project_order = projects.iter().map(|project| project.cwd.clone()).collect();
    for project in projects {
        snapshot.projects.insert(
            project.cwd.clone(),
            crate::workspace::ProjectSnapshot {
                id: project.cwd.clone(),
                name: project.name.clone(),
                cwd: project.cwd.clone(),
                root_id: project.root_cwd.clone(),
                kind: project.kind.clone(),
                git: crate::workspace::ProjectGitState {
                    branch: project.branch.clone(),
                    status: project.status_summary.display_text(),
                    status_summary: project.status_summary.clone(),
                },
                ..crate::workspace::ProjectSnapshot::default()
            },
        );
    }
    snapshot
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    use anyhow::Result;

    use super::{
        build_project_entries, parse_project_command, parse_project_status_output, App,
        GitProjectProbe, Mode, ProjectEntry, ProjectKind, ProjectStatusSummary,
    };
    use crate::daemon::protocol::ClientMessage;
    use crate::input::AppAction;
    use crate::projects::{run_git_branch_delete, run_git_worktree_remove};
    use crate::wezterm::{PaneInfo, SpawnCommand, SplitDirection, WeztermClient};
    use crate::workspace::{AgentSnapshot, WorkspaceSnapshot};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        ListPanes,
        MovePaneToNewTab(u64),
        SplitPane {
            host_pane_id: u64,
            move_pane_id: u64,
            direction: SplitDirection,
        },
        ActivatePane(u64),
        SendText {
            pane_id: u64,
            text: String,
        },
        SpawnNewTab {
            pane_id: u64,
            cwd: String,
            command: SpawnCommand,
        },
    }

    #[derive(Debug)]
    struct FakeWezterm {
        snapshots: VecDeque<Vec<PaneInfo>>,
        calls: Vec<Call>,
        next_spawned_pane_id: u64,
    }

    impl FakeWezterm {
        fn new(snapshots: Vec<Vec<PaneInfo>>) -> Self {
            let next_spawned_pane_id = snapshots
                .iter()
                .flatten()
                .map(|pane| pane.pane_id)
                .max()
                .unwrap_or(0)
                + 1;
            Self {
                snapshots: snapshots.into(),
                calls: Vec::new(),
                next_spawned_pane_id,
            }
        }

        fn current_snapshot(&self) -> Vec<PaneInfo> {
            self.snapshots
                .front()
                .expect("at least one snapshot is required")
                .clone()
        }

        fn next_tab_id(&self) -> u64 {
            self.snapshots
                .iter()
                .flatten()
                .map(|pane| pane.tab_id)
                .max()
                .unwrap_or(0)
                + 1
        }

        fn move_pane_to_tab(&mut self, pane_id: u64, tab_id: u64, window_id: u64) {
            for snapshot in &mut self.snapshots {
                if let Some(pane) = snapshot.iter_mut().find(|pane| pane.pane_id == pane_id) {
                    pane.tab_id = tab_id;
                    pane.window_id = window_id;
                }
            }
        }

        fn set_active_pane(&mut self, pane_id: u64) {
            for snapshot in &mut self.snapshots {
                for pane in snapshot {
                    pane.is_active = pane.pane_id == pane_id;
                }
            }
        }
    }

    impl WeztermClient for FakeWezterm {
        fn list_panes(&mut self) -> Result<Vec<PaneInfo>> {
            self.calls.push(Call::ListPanes);
            let snapshot = self.current_snapshot();
            if self.snapshots.len() > 1 {
                self.snapshots.pop_front();
            }
            Ok(snapshot)
        }

        fn move_pane_to_new_tab(&mut self, pane_id: u64) -> Result<()> {
            self.calls.push(Call::MovePaneToNewTab(pane_id));
            let window_id = self
                .current_snapshot()
                .iter()
                .find(|pane| pane.pane_id == pane_id)
                .map(|pane| pane.window_id)
                .expect("pane should exist when moved to a new tab");
            let tab_id = self.next_tab_id();
            self.move_pane_to_tab(pane_id, tab_id, window_id);
            Ok(())
        }

        fn split_pane(
            &mut self,
            host_pane_id: u64,
            move_pane_id: u64,
            direction: SplitDirection,
        ) -> Result<()> {
            self.calls.push(Call::SplitPane {
                host_pane_id,
                move_pane_id,
                direction,
            });
            let host = self
                .current_snapshot()
                .iter()
                .find(|pane| pane.pane_id == host_pane_id)
                .cloned()
                .expect("host pane should exist when splitting");
            self.move_pane_to_tab(move_pane_id, host.tab_id, host.window_id);
            Ok(())
        }

        fn activate_pane(&mut self, pane_id: u64) -> Result<()> {
            self.calls.push(Call::ActivatePane(pane_id));
            self.set_active_pane(pane_id);
            Ok(())
        }

        fn send_text(&mut self, pane_id: u64, text: &str) -> Result<()> {
            self.calls.push(Call::SendText {
                pane_id,
                text: text.to_string(),
            });
            Ok(())
        }

        fn spawn_new_tab(
            &mut self,
            pane_id: u64,
            cwd: &str,
            command: &SpawnCommand,
        ) -> Result<u64> {
            self.calls.push(Call::SpawnNewTab {
                pane_id,
                cwd: cwd.to_string(),
                command: command.clone(),
            });
            let spawned_pane_id = self.next_spawned_pane_id;
            self.next_spawned_pane_id += 1;
            let host = self
                .current_snapshot()
                .iter()
                .find(|pane| pane.pane_id == pane_id)
                .cloned()
                .expect("host pane should exist when spawning a tab");
            let new_tab_id = self.next_tab_id();

            for snapshot in &mut self.snapshots {
                let mut spawned = pane(spawned_pane_id, new_tab_id, host.window_id);
                spawned.cwd = format!("file://{cwd}");
                snapshot.push(spawned);
            }

            Ok(spawned_pane_id)
        }
    }

    fn test_projects() -> Vec<ProjectEntry> {
        vec![
            ProjectEntry {
                name: "alpha".to_string(),
                cwd: "/tmp/repos/alpha".to_string(),
                branch: "main".to_string(),
                status_summary: ProjectStatusSummary::default(),
                root_name: "alpha".to_string(),
                root_cwd: "/tmp/repos/alpha".to_string(),
                kind: ProjectKind::Root,
            },
            ProjectEntry {
                name: "beta".to_string(),
                cwd: "/tmp/repos/beta".to_string(),
                branch: "feature".to_string(),
                status_summary: ProjectStatusSummary::default(),
                root_name: "beta".to_string(),
                root_cwd: "/tmp/repos/beta".to_string(),
                kind: ProjectKind::Root,
            },
        ]
    }

    fn test_snapshot_with_agents(
        agent_specs: Vec<(&str, Vec<(&str, u64, &str)>)>,
    ) -> WorkspaceSnapshot {
        let mut snapshot = super::workspace_snapshot_from_projects(&test_projects());

        for (project_name, agents) in agent_specs {
            let project = test_projects()
                .into_iter()
                .find(|project| project.name == project_name)
                .expect("test project should exist");
            let project_snapshot = snapshot
                .projects
                .get_mut(&project.cwd)
                .expect("project snapshot should exist");
            project_snapshot.agents = agents
                .into_iter()
                .map(|(runtime, pane_id, status)| AgentSnapshot {
                    runtime: runtime.to_string(),
                    pane_id: Some(pane_id),
                    status: status.to_string(),
                })
                .collect();
        }

        snapshot.generated_at_ms = 1;
        snapshot
    }

    fn load_test_app_with_agents(
        wezterm: &mut FakeWezterm,
        agent_specs: Vec<(&str, Vec<(&str, u64, &str)>)>,
    ) -> App {
        let mut app = App::load_with_projects(wezterm, test_projects()).expect("app should load");
        app.replace_workspace(test_snapshot_with_agents(agent_specs));
        app
    }

    fn search_test_projects() -> Vec<ProjectEntry> {
        vec![
            ProjectEntry {
                name: "alpha".to_string(),
                cwd: "/tmp/repos/alpha".to_string(),
                branch: "main".to_string(),
                status_summary: ProjectStatusSummary::default(),
                root_name: "alpha".to_string(),
                root_cwd: "/tmp/repos/alpha".to_string(),
                kind: ProjectKind::Root,
            },
            ProjectEntry {
                name: "beta".to_string(),
                cwd: "/tmp/repos/beta".to_string(),
                branch: "feature/root-branch".to_string(),
                status_summary: ProjectStatusSummary::default(),
                root_name: "beta".to_string(),
                root_cwd: "/tmp/repos/beta".to_string(),
                kind: ProjectKind::Root,
            },
            ProjectEntry {
                name: "repo".to_string(),
                cwd: "/tmp/repos/repo".to_string(),
                branch: "main".to_string(),
                status_summary: ProjectStatusSummary::default(),
                root_name: "repo".to_string(),
                root_cwd: "/tmp/repos/repo".to_string(),
                kind: ProjectKind::Root,
            },
            ProjectEntry {
                name: "feature/build".to_string(),
                cwd: "/tmp/repos/repo.feature-build".to_string(),
                branch: "feature/build".to_string(),
                status_summary: ProjectStatusSummary::default(),
                root_name: "repo".to_string(),
                root_cwd: "/tmp/repos/repo".to_string(),
                kind: ProjectKind::Worktree,
            },
            ProjectEntry {
                name: "feature/search".to_string(),
                cwd: "/tmp/repos/repo.feature-search".to_string(),
                branch: "feature/search".to_string(),
                status_summary: ProjectStatusSummary::default(),
                root_name: "repo".to_string(),
                root_cwd: "/tmp/repos/repo".to_string(),
                kind: ProjectKind::Worktree,
            },
        ]
    }

    fn pane(pane_id: u64, tab_id: u64, window_id: u64) -> PaneInfo {
        PaneInfo {
            window_id,
            tab_id,
            pane_id,
            workspace: "default".to_string(),
            size: crate::wezterm::PaneSize {
                rows: 44,
                cols: 80,
                pixel_width: 800,
                pixel_height: 600,
                dpi: 96,
            },
            title: format!("pane-{pane_id}"),
            cwd: format!("file:///tmp/{pane_id}"),
            cursor_x: 0,
            cursor_y: 0,
            cursor_shape: "Default".to_string(),
            cursor_visibility: "Visible".to_string(),
            left_col: 0,
            top_row: 0,
            tab_title: String::new(),
            window_title: "window".to_string(),
            is_active: false,
            is_zoomed: false,
            tty_name: Some(format!("/dev/pts/{pane_id}")),
        }
    }

    fn pane_with_cwd(pane_id: u64, tab_id: u64, window_id: u64, cwd: &str) -> PaneInfo {
        let mut pane = pane(pane_id, tab_id, window_id);
        pane.cwd = cwd.to_string();
        pane
    }

    fn set_wezterm_pane() {
        unsafe {
            std::env::set_var("WEZTERM_PANE", "10");
        }
    }

    #[test]
    fn loads_projects_view_by_default() {
        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![pane(10, 1, 1), pane(20, 2, 1)]]);
        let app = App::load_with_projects(&mut wezterm, test_projects()).expect("app should load");

        assert_eq!(app.selected_project_index(), 0);
        assert_eq!(app.projects().len(), 2);
        assert!(app.input_line().contains("selected project"));
    }

    #[test]
    fn project_navigation_changes_selected_project() {
        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![pane(10, 1, 1), pane(20, 2, 1)]]);
        let mut app =
            App::load_with_projects(&mut wezterm, test_projects()).expect("app should load");

        app.apply(AppAction::ProjectMoveDown, &mut wezterm)
            .expect("move should succeed");
        app.apply(AppAction::ProjectMoveUp, &mut wezterm)
            .expect("move should succeed");

        assert_eq!(app.selected_project_index(), 0);
        assert_eq!(wezterm.calls, vec![Call::ListPanes]);
    }

    #[test]
    fn starting_command_input_sets_input_state() {
        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![pane(10, 1, 1), pane(20, 2, 1)]]);
        let mut app =
            App::load_with_projects(&mut wezterm, test_projects()).expect("app should load");

        app.apply(AppAction::StartCommandInput, &mut wezterm)
            .expect("command input should start");
        app.apply(AppAction::EditInput('r'), &mut wezterm)
            .expect("input should accept text");

        assert!(app.is_input_active());
        assert!(app.is_command_active());
        assert_eq!(app.input_line(), ":r");
        assert_eq!(app.command_completions().len(), 0);
    }

    #[test]
    fn command_errors_persist_across_background_refresh() {
        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![pane(10, 1, 1), pane(20, 2, 1)]]);
        let mut app =
            App::load_with_projects(&mut wezterm, test_projects()).expect("app should load");

        app.execute_project_command("git switch missing-branch", &mut wezterm)
            .expect("git switch should queue");
        assert!(app.status_line().contains("Queued git_switch for alpha"));

        app.tick(&mut wezterm)
            .expect("background refresh should succeed");

        assert!(app.status_line().contains("Queued git_switch for alpha"));
    }

    #[test]
    fn command_input_completions_are_context_aware_and_tab_applies_them() {
        let fixture = create_worktree_fixture("command-completions-context");
        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![pane(10, 1, 1), pane(20, 2, 1)]]);
        let mut app = App::load_with_projects(&mut wezterm, fixture.projects.clone())
            .expect("app should load");

        app.apply(AppAction::StartCommandInput, &mut wezterm)
            .expect("command input should start");
        assert_eq!(
            app.command_completions()
                .iter()
                .map(|item| item.label.as_str())
                .collect::<Vec<_>>(),
            vec!["agent", "wt", "git"]
        );

        app.apply(AppAction::EditInput('g'), &mut wezterm)
            .expect("command input should accept text");
        app.apply(AppAction::EditInput('i'), &mut wezterm)
            .expect("command input should narrow top-level command");
        app.apply(AppAction::AcceptCommandCompletion, &mut wezterm)
            .expect("tab completion should apply top-level command");
        app.apply(AppAction::EditInput('s'), &mut wezterm)
            .expect("command input should accept text");
        app.apply(AppAction::AcceptCommandCompletion, &mut wezterm)
            .expect("tab completion should apply subcommand");

        assert!(app.input_line().contains(":git switch "));

        app.apply(AppAction::CancelInput, &mut wezterm)
            .expect("command input should cancel");
        app.apply(AppAction::ProjectMoveDown, &mut wezterm)
            .expect("selection should move to worktree");

        app.apply(AppAction::StartCommandInput, &mut wezterm)
            .expect("command input should start for worktree");
        assert_eq!(
            app.command_completions()
                .iter()
                .map(|item| item.label.as_str())
                .collect::<Vec<_>>(),
            vec!["agent", "wt"]
        );
        app.apply(AppAction::EditInput('w'), &mut wezterm)
            .expect("command input should accept text");
        app.apply(AppAction::EditInput('t'), &mut wezterm)
            .expect("command input should accept text");
        app.apply(AppAction::AcceptCommandCompletion, &mut wezterm)
            .expect("tab completion should apply worktree command");

        assert_eq!(
            app.command_completions()
                .iter()
                .map(|item| item.label.as_str())
                .collect::<Vec<_>>(),
            vec!["add", "remove", "merge", "pr", "land"]
        );
    }

    #[test]
    fn confirm_input_applies_selected_completion_before_executing() {
        let sandbox = test_sandbox("confirm-input-applies-completion");
        let root = create_repo_in(&sandbox, "repo");
        git(&root, &["switch", "-c", "feature/confirm"]);
        git(&root, &["switch", "main"]);
        let projects = super::discover_projects_in(std::slice::from_ref(&sandbox))
            .expect("projects should be discovered");

        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![pane(10, 1, 1), pane(20, 2, 1)]]);
        let mut app = App::load_with_projects(&mut wezterm, projects).expect("app should load");

        app.apply(AppAction::StartCommandInput, &mut wezterm)
            .expect("command input should start");
        for c in "gi".chars() {
            app.apply(AppAction::EditInput(c), &mut wezterm)
                .expect("command should accept text");
        }
        app.apply(AppAction::ConfirmInput, &mut wezterm)
            .expect("enter should apply top-level completion");
        assert_eq!(app.input_line(), ":git ");

        app.apply(AppAction::EditInput('s'), &mut wezterm)
            .expect("command should accept text");
        app.apply(AppAction::ConfirmInput, &mut wezterm)
            .expect("enter should apply subcommand completion");
        assert_eq!(app.input_line(), ":git switch ");

        app.apply(AppAction::EditInput('c'), &mut wezterm)
            .expect("command should accept text");
        app.apply(AppAction::ConfirmInput, &mut wezterm)
            .expect("enter should apply branch completion and execute");

        assert!(!app.is_command_active());
        assert_eq!(
            app.drain_outbox(),
            vec![ClientMessage::RunOperation {
                project_id: root_as_str(&root).to_string(),
                operation: "git_switch".to_string(),
                command: "git switch feature/confirm".to_string(),
            }]
        );
        assert_eq!(
            super::read_branch_name(&root).expect("branch should be readable"),
            "main"
        );
        assert!(app.status_line().contains("Queued git_switch for repo"));
    }

    #[test]
    fn command_completion_navigation_wraps_with_ctrl_n_and_ctrl_p() {
        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![pane(10, 1, 1), pane(20, 2, 1)]]);
        let mut app =
            App::load_with_projects(&mut wezterm, test_projects()).expect("app should load");

        app.apply(AppAction::StartCommandInput, &mut wezterm)
            .expect("command input should start");
        assert_eq!(app.selected_command_completion_index(), Some(0));

        app.apply(AppAction::PreviousCommandCompletion, &mut wezterm)
            .expect("previous completion should wrap");
        assert_eq!(app.selected_command_completion_index(), Some(2));

        app.apply(AppAction::NextCommandCompletion, &mut wezterm)
            .expect("next completion should wrap");
        assert_eq!(app.selected_command_completion_index(), Some(0));
    }

    #[test]
    fn search_mode_selects_matching_projects_and_ignores_branch_column() {
        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![pane(10, 1, 1), pane(20, 2, 1)]]);
        let mut app =
            App::load_with_projects(&mut wezterm, search_test_projects()).expect("app should load");

        app.apply(AppAction::StartSearchInput, &mut wezterm)
            .expect("search input should start");
        for c in "feature".chars() {
            app.apply(AppAction::EditInput(c), &mut wezterm)
                .expect("search should accept text");
        }

        assert!(app.is_search_active());
        assert_eq!(app.selected_project_name(), Some("feature/build"));
    }

    #[test]
    fn search_mode_cycles_matches_and_escape_restores_selection() {
        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![pane(10, 1, 1), pane(20, 2, 1)]]);
        let mut app =
            App::load_with_projects(&mut wezterm, search_test_projects()).expect("app should load");

        app.apply(AppAction::StartSearchInput, &mut wezterm)
            .expect("search input should start");
        for c in "feature".chars() {
            app.apply(AppAction::EditInput(c), &mut wezterm)
                .expect("search should accept text");
        }
        app.apply(AppAction::NextSearchMatch, &mut wezterm)
            .expect("next match should succeed");
        assert_eq!(app.selected_project_name(), Some("feature/search"));

        app.apply(AppAction::PreviousSearchMatch, &mut wezterm)
            .expect("previous match should succeed");
        assert_eq!(app.selected_project_name(), Some("feature/build"));

        app.apply(AppAction::CancelInput, &mut wezterm)
            .expect("cancel search should succeed");
        assert!(!app.is_search_active());
        assert_eq!(app.selected_project_name(), Some("alpha"));
    }

    #[test]
    fn search_mode_enter_keeps_selected_match() {
        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![pane(10, 1, 1), pane(20, 2, 1)]]);
        let mut app =
            App::load_with_projects(&mut wezterm, search_test_projects()).expect("app should load");

        app.apply(AppAction::StartSearchInput, &mut wezterm)
            .expect("search input should start");
        for c in "search".chars() {
            app.apply(AppAction::EditInput(c), &mut wezterm)
                .expect("search should accept text");
        }

        assert_eq!(app.selected_project_name(), Some("feature/search"));

        app.apply(AppAction::ConfirmInput, &mut wezterm)
            .expect("confirm search should succeed");

        assert!(!app.is_search_active());
        assert_eq!(app.selected_project_name(), Some("feature/search"));
    }

    #[test]
    fn remove_command_rejects_root_projects() {
        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![pane(10, 1, 1), pane(20, 2, 1)]]);
        let mut app =
            App::load_with_projects(&mut wezterm, test_projects()).expect("app should load");

        app.apply(AppAction::StartCommandInput, &mut wezterm)
            .expect("command input should start");
        for c in "wt remove".chars() {
            app.apply(AppAction::EditInput(c), &mut wezterm)
                .expect("input should accept text");
        }

        app.apply(AppAction::ConfirmInput, &mut wezterm)
            .expect("root remove should queue");
        assert_eq!(
            app.drain_outbox(),
            vec![ClientMessage::RunOperation {
                project_id: "/tmp/repos/alpha".to_string(),
                operation: "wt_remove".to_string(),
                command: "wt remove".to_string(),
            }]
        );
    }

    #[test]
    fn parses_projects_commands_with_optional_target() {
        assert_eq!(
            parse_project_command("wt add feature/BOOST-3432").expect("wt add should parse"),
            super::ProjectCommand::Add {
                branch: "feature/BOOST-3432".to_string()
            }
        );
        assert_eq!(
            parse_project_command("wt merge").expect("wt merge should parse"),
            super::ProjectCommand::Merge { target: None }
        );
        assert_eq!(
            parse_project_command("wt merge main").expect("wt merge target should parse"),
            super::ProjectCommand::Merge {
                target: Some("main".to_string())
            }
        );
        assert_eq!(
            parse_project_command("wt pr main").expect("wt pr target should parse"),
            super::ProjectCommand::Pr {
                target: Some("main".to_string())
            }
        );
        assert_eq!(
            parse_project_command("wt land").expect("wt land should parse"),
            super::ProjectCommand::Land { target: None }
        );
        assert_eq!(
            parse_project_command("wt remove").expect("wt remove should parse"),
            super::ProjectCommand::Remove
        );
        assert_eq!(
            parse_project_command("agent claude").expect("agent should parse"),
            super::ProjectCommand::Agent {
                runtime: super::AgentRuntime::Claude,
            }
        );
        assert_eq!(
            parse_project_command("git switch feature/root-branch")
                .expect("git switch should parse"),
            super::ProjectCommand::GitSwitch {
                branch: "feature/root-branch".to_string()
            }
        );
        assert_eq!(
            parse_project_command("git pull").expect("git pull should parse"),
            super::ProjectCommand::GitPull
        );
        assert!(parse_project_command("remove").is_err());
        assert!(parse_project_command("merge").is_err());
        assert!(parse_project_command("pr main").is_err());
        assert!(parse_project_command("land").is_err());
        assert!(parse_project_command("wt add").is_err());
        assert!(parse_project_command("wt remove main").is_err());
        assert!(parse_project_command("git switch").is_err());
    }

    #[test]
    fn git_switch_command_queues_daemon_operation_for_selected_root() {
        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![pane(10, 1, 1), pane(20, 2, 1)]]);
        let mut app =
            App::load_with_projects(&mut wezterm, test_projects()).expect("app should load");

        app.execute_project_command("git switch feature/switch-test", &mut wezterm)
            .expect("git switch should queue");

        assert_eq!(
            app.drain_outbox(),
            vec![ClientMessage::RunOperation {
                project_id: "/tmp/repos/alpha".to_string(),
                operation: "git_switch".to_string(),
                command: "git switch feature/switch-test".to_string(),
            }]
        );
        assert!(app.status_line().contains("Queued git_switch for alpha"));
    }

    #[test]
    fn git_pull_command_queues_daemon_operation_for_selected_root() {
        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![pane(10, 1, 1), pane(20, 2, 1)]]);
        let mut app =
            App::load_with_projects(&mut wezterm, test_projects()).expect("app should load");

        app.execute_project_command("git pull", &mut wezterm)
            .expect("git pull should queue");

        assert_eq!(
            app.drain_outbox(),
            vec![ClientMessage::RunOperation {
                project_id: "/tmp/repos/alpha".to_string(),
                operation: "git_pull".to_string(),
                command: "git pull".to_string(),
            }]
        );
        assert!(app.status_line().contains("Queued git_pull for alpha"));
    }

    #[test]
    fn worktree_command_queues_daemon_operation_for_selected_worktree() {
        let fixture = create_worktree_fixture("git-switch-worktree-reject");
        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![pane(10, 1, 1), pane(20, 2, 1)]]);
        let mut app = App::load_with_projects(&mut wezterm, fixture.projects.clone())
            .expect("app should load");
        app.apply(AppAction::ProjectMoveDown, &mut wezterm)
            .expect("selection should move to worktree");

        app.execute_project_command("wt merge", &mut wezterm)
            .expect("merge should queue");

        assert_eq!(
            app.drain_outbox(),
            vec![ClientMessage::RunOperation {
                project_id: root_as_str(&fixture.worktree).to_string(),
                operation: "wt_merge".to_string(),
                command: "wt merge".to_string(),
            }]
        );
        assert!(app.status_line().contains("Queued wt_merge for feature"));
    }

    #[test]
    fn wt_add_command_queues_daemon_operation() {
        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![pane(10, 1, 1), pane(20, 2, 1)]]);
        let mut app =
            App::load_with_projects(&mut wezterm, test_projects()).expect("app should load");

        app.execute_project_command("wt add feature/BOOST-3432", &mut wezterm)
            .expect("worktree creation should queue");

        assert_eq!(
            app.drain_outbox(),
            vec![ClientMessage::RunOperation {
                project_id: "/tmp/repos/alpha".to_string(),
                operation: "wt_add".to_string(),
                command: "wt add feature/BOOST-3432".to_string(),
            }]
        );
        assert!(app.status_line().contains("Queued wt_add for alpha"));
    }

    #[test]
    fn worktree_remove_and_land_commands_queue_daemon_operations() {
        let fixture = create_worktree_fixture("merge-command");
        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![pane(10, 1, 1), pane(20, 2, 1)]]);
        let mut app = App::load_with_projects(&mut wezterm, fixture.projects.clone())
            .expect("app should load");
        app.apply(AppAction::ProjectMoveDown, &mut wezterm)
            .expect("selection should move to worktree");

        app.execute_project_command("wt remove", &mut wezterm)
            .expect("remove should queue");
        assert_eq!(
            app.drain_outbox(),
            vec![ClientMessage::RunOperation {
                project_id: root_as_str(&fixture.worktree).to_string(),
                operation: "wt_remove".to_string(),
                command: "wt remove".to_string(),
            }]
        );

        app.execute_project_command("wt land", &mut wezterm)
            .expect("land should queue");
        assert_eq!(
            app.drain_outbox(),
            vec![ClientMessage::RunOperation {
                project_id: root_as_str(&fixture.worktree).to_string(),
                operation: "wt_land".to_string(),
                command: "wt land".to_string(),
            }]
        );
    }

    #[test]
    fn bootstraps_default_repo_config_and_loads_default_source() {
        let home = test_sandbox("config-bootstrap");
        let repo_source = home.join("repos");
        let config_path = super::config_path_from_home(&home);

        fs::create_dir_all(&repo_source).expect("default repo source should be created");

        let repo_sources = super::load_repo_sources_from_config_at(&config_path, &home)
            .expect("default config should load");

        assert_eq!(
            repo_sources,
            vec![repo_source.canonicalize().expect("path should resolve")]
        );
        assert_eq!(
            fs::read_to_string(config_path).expect("config should be written"),
            "repo_sources = [\"~/repos\"]\n"
        );
    }

    #[test]
    fn repo_config_rejects_missing_source() {
        let home = test_sandbox("config-missing-source");
        let config_path = super::config_path_from_home(&home);

        fs::create_dir_all(
            config_path
                .parent()
                .expect("config path should have a parent directory"),
        )
        .expect("config directory should be created");
        write_file(&config_path, "repo_sources = [\"~/missing\"]\n");

        let error = super::load_repo_sources_from_config_at(&config_path, &home)
            .expect_err("missing repo source should fail");
        assert!(error
            .to_string()
            .contains("configured repo source does not exist"));
    }

    #[test]
    fn discovers_projects_across_multiple_repo_sources() {
        let left = test_sandbox("discover-multi-left");
        let right = test_sandbox("discover-multi-right");

        create_repo_in(&left, "alpha");
        create_repo_in(&right, "beta");

        let projects = super::discover_projects_in(&[left, right])
            .expect("projects should be discovered across both sources");

        let names = projects
            .iter()
            .filter(|project| project.kind == ProjectKind::Root)
            .map(|project| project.name.clone())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[test]
    fn pr_command_queues_daemon_operation() {
        let fixture = create_worktree_fixture("pr-command");
        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![pane(10, 1, 1), pane(20, 2, 1)]]);
        let mut app = App::load_with_projects(&mut wezterm, fixture.projects.clone())
            .expect("app should load");
        app.apply(AppAction::ProjectMoveDown, &mut wezterm)
            .expect("selection should move to worktree");

        app.execute_project_command("wt pr", &mut wezterm)
            .expect("pr should queue");

        assert_eq!(
            app.drain_outbox(),
            vec![ClientMessage::RunOperation {
                project_id: root_as_str(&fixture.worktree).to_string(),
                operation: "wt_pr".to_string(),
                command: "wt pr".to_string(),
            }]
        );
        assert!(app.status_line().contains("Queued wt_pr for feature"));
    }

    #[test]
    fn attach_project_agent_requires_existing_agent() {
        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![pane(10, 1, 1), pane(20, 2, 1)]]);
        let mut app =
            App::load_with_projects(&mut wezterm, test_projects()).expect("app should load");

        app.apply(AppAction::AttachProjectAgent, &mut wezterm)
            .expect("attach should not error");

        assert!(app
            .status_line()
            .contains("Start an agent for this project first"));
        assert_eq!(wezterm.calls, vec![Call::ListPanes]);
    }

    #[test]
    fn replacing_workspace_keeps_selection_by_project_id() {
        use crate::workspace::{ProjectSnapshot, WorkspaceSnapshot};
        use std::collections::BTreeMap;

        let mut first_projects = BTreeMap::new();
        first_projects.insert(
            "root:alpha".to_string(),
            ProjectSnapshot {
                id: "root:alpha".to_string(),
                name: "alpha".to_string(),
                ..ProjectSnapshot::default()
            },
        );
        first_projects.insert(
            "root:beta".to_string(),
            ProjectSnapshot {
                id: "root:beta".to_string(),
                name: "beta".to_string(),
                ..ProjectSnapshot::default()
            },
        );

        let mut app = App::from_snapshot(WorkspaceSnapshot {
            protocol_version: 1,
            generated_at_ms: 1,
            projects: first_projects,
            project_order: vec!["root:alpha".to_string(), "root:beta".to_string()],
            warnings: Vec::new(),
        });
        app.project_move_down();

        let mut second_projects = BTreeMap::new();
        second_projects.insert(
            "root:beta".to_string(),
            ProjectSnapshot {
                id: "root:beta".to_string(),
                name: "beta".to_string(),
                ..ProjectSnapshot::default()
            },
        );
        second_projects.insert(
            "root:alpha".to_string(),
            ProjectSnapshot {
                id: "root:alpha".to_string(),
                name: "alpha".to_string(),
                ..ProjectSnapshot::default()
            },
        );

        app.replace_workspace(WorkspaceSnapshot {
            protocol_version: 1,
            generated_at_ms: 2,
            projects: second_projects,
            project_order: vec!["root:beta".to_string(), "root:alpha".to_string()],
            warnings: Vec::new(),
        });

        assert_eq!(app.selected_project_id(), Some("root:beta"));
    }

    #[test]
    fn replacing_workspace_prefers_agent_that_needs_input() {
        use crate::workspace::{AgentSnapshot, ProjectSnapshot, WorkspaceSnapshot};
        use std::collections::BTreeMap;

        let mut first_projects = BTreeMap::new();
        first_projects.insert(
            "root:alpha".to_string(),
            ProjectSnapshot {
                id: "root:alpha".to_string(),
                name: "alpha".to_string(),
                agents: vec![
                    AgentSnapshot {
                        runtime: "claude".to_string(),
                        pane_id: Some(10),
                        status: "working".to_string(),
                    },
                    AgentSnapshot {
                        runtime: "opencode".to_string(),
                        pane_id: Some(11),
                        status: "working".to_string(),
                    },
                ],
                ..ProjectSnapshot::default()
            },
        );

        let mut app = App::from_snapshot(WorkspaceSnapshot {
            protocol_version: 1,
            generated_at_ms: 1,
            projects: first_projects,
            project_order: vec!["root:alpha".to_string()],
            warnings: Vec::new(),
        });

        assert_eq!(app.best_attachable_agent(), Some((0, 0)));

        let mut second_projects = BTreeMap::new();
        second_projects.insert(
            "root:alpha".to_string(),
            ProjectSnapshot {
                id: "root:alpha".to_string(),
                name: "alpha".to_string(),
                agents: vec![
                    AgentSnapshot {
                        runtime: "claude".to_string(),
                        pane_id: Some(10),
                        status: "working".to_string(),
                    },
                    AgentSnapshot {
                        runtime: "opencode".to_string(),
                        pane_id: Some(11),
                        status: "needs_input".to_string(),
                    },
                ],
                ..ProjectSnapshot::default()
            },
        );

        app.replace_workspace(WorkspaceSnapshot {
            protocol_version: 1,
            generated_at_ms: 2,
            projects: second_projects,
            project_order: vec!["root:alpha".to_string()],
            warnings: Vec::new(),
        });

        let (project_index, agent_index) = app.best_attachable_agent().unwrap();
        assert_eq!((project_index, agent_index), (0, 1));
    }

    #[test]
    fn toggle_follow_mode_updates_global_indicator() {
        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![pane(10, 1, 1), pane(20, 2, 1)]]);
        let mut app =
            App::load_with_projects(&mut wezterm, test_projects()).expect("app should load");

        app.apply(AppAction::ToggleFollowMode, &mut wezterm)
            .expect("follow mode should enable");

        assert!(app.is_follow_mode());
        assert_eq!(app.follow_queue_len(), 0);
        assert!(app.input_line().contains("Follow mode is active globally"));
        assert!(app.status_line().contains("Follow mode ON"));

        app.apply(AppAction::ToggleFollowMode, &mut wezterm)
            .expect("follow mode should disable");

        assert!(!app.is_follow_mode());
        assert!(app.status_line().contains("Follow mode OFF"));
    }

    #[test]
    fn follow_mode_attaches_global_needs_input_agent() {
        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![
            pane_with_cwd(10, 1, 1, "file:///tmp/repos/nerve_center"),
            pane_with_cwd(20, 2, 1, "file:///tmp/repos/alpha"),
            pane_with_cwd(30, 3, 1, "file:///tmp/repos/beta"),
        ]]);
        let mut app = load_test_app_with_agents(
            &mut wezterm,
            vec![
                ("alpha", vec![("claude", 20, "working")]),
                ("beta", vec![("opencode", 30, "needs_input")]),
            ],
        );

        let result = app.apply(AppAction::ToggleFollowMode, &mut wezterm);
        result.expect("follow mode should attach the global needs-input agent");

        assert!(app.is_follow_mode());
        assert_eq!(app.attached_pane_id, Some(30));
        assert_eq!(app.follow_queue_len(), 1);
        assert!(app
            .input_line()
            .contains("Follow mode: forwarding to oc:30[i] for beta"));
        assert!(app
            .status_line()
            .contains("Follow attached oc:30[i] for beta"));
    }

    #[test]
    fn follow_mode_advances_to_next_waiting_agent_after_input() {
        set_wezterm_pane();
        let snapshot = vec![
            pane_with_cwd(10, 1, 1, "file:///tmp/repos/nerve_center"),
            pane_with_cwd(20, 2, 1, "file:///tmp/repos/alpha"),
            pane_with_cwd(30, 3, 1, "file:///tmp/repos/beta"),
        ];
        let mut wezterm = FakeWezterm::new(vec![snapshot.clone(), snapshot.clone(), snapshot]);
        let mut app = load_test_app_with_agents(
            &mut wezterm,
            vec![
                ("alpha", vec![("claude", 20, "needs_input")]),
                ("beta", vec![("opencode", 30, "needs_input")]),
            ],
        );

        let result = (|| -> Result<()> {
            app.apply(AppAction::ToggleFollowMode, &mut wezterm)?;
            assert_eq!(app.attached_pane_id, Some(20));
            assert_eq!(app.follow_queue_len(), 2);

            app.apply(AppAction::Forward("answer".to_string()), &mut wezterm)?;
            app.replace_workspace(test_snapshot_with_agents(vec![
                ("alpha", vec![("claude", 20, "working")]),
                ("beta", vec![("opencode", 30, "needs_input")]),
            ]));
            app.tick(&mut wezterm)?;

            assert_eq!(app.attached_pane_id, Some(30));
            assert_eq!(app.follow_queue_len(), 1);
            assert!(app
                .status_line()
                .contains("Follow attached oc:30[i] for beta"));

            app.apply(AppAction::ExitForwarding, &mut wezterm)?;
            assert!(app.is_follow_mode());
            assert!(!app.is_forwarding());
            Ok(())
        })();
        result.expect("follow mode should advance to the next waiting agent");
    }

    #[test]
    fn attach_project_agent_and_refocuses_tui() {
        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![
            pane_with_cwd(10, 1, 1, "file:///tmp/repos/nerve_center"),
            pane_with_cwd(20, 2, 1, "file:///tmp/repos/alpha"),
        ]]);
        let mut app = load_test_app_with_agents(
            &mut wezterm,
            vec![("alpha", vec![("claude", 20, "working")])],
        );

        let result = app.apply(AppAction::AttachProjectAgent, &mut wezterm);
        result.expect("attach should succeed");

        assert_eq!(app.attached_pane_id, Some(20));
        assert_eq!(
            wezterm.calls,
            vec![
                Call::ListPanes,
                Call::ListPanes,
                Call::SplitPane {
                    host_pane_id: 10,
                    move_pane_id: 20,
                    direction: SplitDirection::Right,
                },
                Call::ActivatePane(10),
                Call::ListPanes,
            ]
        );
    }

    #[test]
    fn attach_project_agent_prefers_agent_that_needs_input() {
        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![
            pane_with_cwd(10, 1, 1, "file:///tmp/repos/nerve_center"),
            pane_with_cwd(20, 2, 1, "file:///tmp/repos/alpha"),
            pane_with_cwd(30, 3, 1, "file:///tmp/repos/alpha"),
        ]]);
        let mut app = load_test_app_with_agents(
            &mut wezterm,
            vec![(
                "alpha",
                vec![("claude", 20, "working"), ("opencode", 30, "needs_input")],
            )],
        );

        let result = app.apply(AppAction::AttachProjectAgent, &mut wezterm);
        result.expect("attach should succeed");

        assert_eq!(app.attached_pane_id, Some(30));
        assert!(app.input_line().contains("oc:30[i]"));
        assert_eq!(
            wezterm.calls,
            vec![
                Call::ListPanes,
                Call::ListPanes,
                Call::SplitPane {
                    host_pane_id: 10,
                    move_pane_id: 30,
                    direction: SplitDirection::Right,
                },
                Call::ActivatePane(10),
                Call::ListPanes,
            ]
        );
    }

    #[test]
    fn attach_project_agent_starts_forwarding_and_escape_stops_it() {
        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![
            pane_with_cwd(10, 1, 1, "file:///tmp/repos/nerve_center"),
            pane_with_cwd(20, 2, 1, "file:///tmp/repos/alpha"),
        ]]);
        let mut app = load_test_app_with_agents(
            &mut wezterm,
            vec![("alpha", vec![("claude", 20, "working")])],
        );

        let result = app.apply(AppAction::AttachProjectAgent, &mut wezterm);
        result.expect("attach should succeed");
        assert_eq!(app.mode, Mode::Forwarding);
        assert!(app
            .input_line()
            .contains("Forwarding keys to cc:20[w] for alpha"));

        app.apply(AppAction::Forward("i".to_string()), &mut wezterm)
            .expect("forward should succeed");
        app.apply(AppAction::ExitForwarding, &mut wezterm)
            .expect("exit forwarding should succeed");

        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(
            wezterm.calls,
            vec![
                Call::ListPanes,
                Call::ListPanes,
                Call::SplitPane {
                    host_pane_id: 10,
                    move_pane_id: 20,
                    direction: SplitDirection::Right,
                },
                Call::ActivatePane(10),
                Call::ListPanes,
                Call::SendText {
                    pane_id: 20,
                    text: "i".to_string(),
                },
            ]
        );
    }

    #[test]
    fn forwarding_mode_switches_between_project_agents_without_wrapping() {
        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![
            pane_with_cwd(10, 1, 1, "file:///tmp/repos/nerve_center"),
            pane_with_cwd(20, 2, 1, "file:///tmp/repos/alpha"),
            pane_with_cwd(30, 3, 1, "file:///tmp/repos/alpha"),
            pane_with_cwd(40, 4, 1, "file:///tmp/repos/alpha"),
        ]]);
        let mut app = load_test_app_with_agents(
            &mut wezterm,
            vec![(
                "alpha",
                vec![
                    ("claude", 20, "working"),
                    ("opencode", 30, "needs_input"),
                    ("opencode", 40, "done"),
                ],
            )],
        );

        let result = (|| -> Result<()> {
            app.apply(AppAction::AttachProjectAgent, &mut wezterm)?;
            assert_eq!(app.attached_pane_id, Some(30));

            app.apply(AppAction::SelectNextProjectAgent, &mut wezterm)?;
            assert_eq!(app.attached_pane_id, Some(40));
            assert!(app.input_line().contains("oc:40[d]"));

            let call_count_at_right_edge = wezterm.calls.len();
            app.apply(AppAction::SelectNextProjectAgent, &mut wezterm)?;
            assert_eq!(app.attached_pane_id, Some(40));
            assert_eq!(wezterm.calls.len(), call_count_at_right_edge);

            app.apply(AppAction::SelectPreviousProjectAgent, &mut wezterm)?;
            assert_eq!(app.attached_pane_id, Some(30));

            app.apply(AppAction::SelectPreviousProjectAgent, &mut wezterm)?;
            assert_eq!(app.attached_pane_id, Some(20));
            assert!(app.input_line().contains("cc:20[w]"));

            let call_count_at_left_edge = wezterm.calls.len();
            app.apply(AppAction::SelectPreviousProjectAgent, &mut wezterm)?;
            assert_eq!(app.attached_pane_id, Some(20));
            assert_eq!(wezterm.calls.len(), call_count_at_left_edge);

            Ok(())
        })();
        result.expect("agent switching should succeed");
    }

    #[test]
    fn unsupported_layout_does_not_run_project_attach_commands() {
        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![
            pane(10, 1, 1),
            pane(20, 1, 1),
            pane(30, 1, 1),
            pane_with_cwd(40, 2, 1, "file:///tmp/repos/alpha"),
        ]]);
        let mut app = load_test_app_with_agents(
            &mut wezterm,
            vec![("alpha", vec![("claude", 40, "working")])],
        );

        let result = app.apply(AppAction::AttachProjectAgent, &mut wezterm);
        result.expect("unsupported layout should not error");

        assert!(app.status_line().contains("unsupported layout"));
        assert_eq!(wezterm.calls, vec![Call::ListPanes, Call::ListPanes]);
    }

    #[test]
    fn project_terminal_open_spawns_and_switches_without_refocusing_tui() {
        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![pane(10, 1, 1), pane(20, 1, 1)]]);
        let mut app =
            App::load_with_projects(&mut wezterm, test_projects()).expect("app should load");

        app.apply(AppAction::OpenProjectTerminal, &mut wezterm)
            .expect("open should succeed");

        assert_eq!(
            wezterm.calls,
            vec![
                Call::ListPanes,
                Call::SpawnNewTab {
                    pane_id: 10,
                    cwd: "/tmp/repos/alpha".to_string(),
                    command: SpawnCommand::shell(),
                },
                Call::ListPanes,
                Call::MovePaneToNewTab(20),
                Call::SplitPane {
                    host_pane_id: 10,
                    move_pane_id: 21,
                    direction: SplitDirection::Right,
                },
                Call::ActivatePane(21),
                Call::ListPanes,
            ]
        );
        assert_eq!(app.attached_pane_id, Some(21));
    }

    #[test]
    fn agent_command_spawns_agent_tab_and_refocuses_tui() {
        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![pane(10, 1, 1), pane(20, 2, 1)]]);
        let mut app =
            App::load_with_projects(&mut wezterm, test_projects()).expect("app should load");

        app.execute_project_command("agent claude", &mut wezterm)
            .expect("agent command should succeed");

        assert_eq!(
            wezterm.calls,
            vec![
                Call::ListPanes,
                Call::SpawnNewTab {
                    pane_id: 10,
                    cwd: "/tmp/repos/alpha".to_string(),
                    command: SpawnCommand::new("claude", vec!["claude".to_string()]),
                },
                Call::ActivatePane(10),
                Call::ListPanes,
            ]
        );
        assert!(app.status_line().contains("Opened claude tab for alpha"));
        assert_eq!(
            app.project_agent_monitors(0)
                .iter()
                .map(|monitor| monitor.display_text())
                .collect::<Vec<_>>(),
            vec!["cc:21[s]"]
        );
    }

    #[test]
    fn attach_project_agent_works_for_agent_started_in_this_session() {
        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![pane(10, 1, 1), pane(20, 2, 1)]]);
        let mut app =
            App::load_with_projects(&mut wezterm, test_projects()).expect("app should load");

        app.execute_project_command("agent claude", &mut wezterm)
            .expect("agent command should succeed");
        app.apply(AppAction::AttachProjectAgent, &mut wezterm)
            .expect("attach should succeed");

        assert_eq!(
            wezterm.calls,
            vec![
                Call::ListPanes,
                Call::SpawnNewTab {
                    pane_id: 10,
                    cwd: "/tmp/repos/alpha".to_string(),
                    command: SpawnCommand::new("claude", vec!["claude".to_string()]),
                },
                Call::ActivatePane(10),
                Call::ListPanes,
                Call::ListPanes,
                Call::SplitPane {
                    host_pane_id: 10,
                    move_pane_id: 21,
                    direction: SplitDirection::Right,
                },
                Call::ActivatePane(10),
                Call::ListPanes,
            ]
        );
        assert_eq!(app.attached_pane_id, Some(21));
    }

    #[test]
    fn project_editor_action_does_not_change_wezterm_layout() {
        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![pane(10, 1, 1), pane(20, 2, 1)]]);
        let mut app =
            App::load_with_projects(&mut wezterm, test_projects()).expect("app should load");
        app.apply(AppAction::ProjectMoveDown, &mut wezterm)
            .expect("move should succeed");

        app.apply(AppAction::OpenProjectEditor, &mut wezterm)
            .expect("open should succeed");

        assert_eq!(wezterm.calls, vec![Call::ListPanes]);
        assert_eq!(app.attached_pane_id, None);
    }

    #[test]
    fn project_idea_open_uses_selected_project_path() {
        let _env_lock = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let fake_bin = test_sandbox("fake-idea-bin");
        let idea_path = fake_bin.join("idea");
        let log_path = fake_bin.join("idea.log");
        write_file(
            &idea_path,
            &format!(
                "#!/bin/sh
printf '%s\n' \"$1\" > '{}'
exit 0
",
                log_path.display()
            ),
        );
        chmod_executable(&idea_path);
        let original_path = env::var("PATH").unwrap_or_default();
        let patched_path = format!("{}:{}", fake_bin.display(), original_path);
        unsafe {
            env::set_var("PATH", patched_path);
        }

        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![pane(10, 1, 1), pane(20, 2, 1)]]);
        let mut app =
            App::load_with_projects(&mut wezterm, test_projects()).expect("app should load");
        app.apply(AppAction::ProjectMoveDown, &mut wezterm)
            .expect("move should succeed");

        let result = app.apply(AppAction::OpenProjectIdea, &mut wezterm);
        std::thread::sleep(std::time::Duration::from_millis(50));

        unsafe {
            env::set_var("PATH", original_path);
        }
        result.expect("idea launch should succeed");

        assert_eq!(
            fs::read_to_string(log_path)
                .expect("idea log should exist")
                .trim(),
            "/tmp/repos/beta"
        );
        assert_eq!(wezterm.calls, vec![Call::ListPanes]);
    }

    #[test]
    fn loads_multiple_agent_monitors_for_one_project() {
        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![
            pane_with_cwd(10, 1, 1, "file:///tmp/repos/nerve_center"),
            pane_with_cwd(20, 2, 1, "file:///tmp/repos/alpha/src"),
            pane_with_cwd(30, 3, 1, "file:///tmp/repos/alpha"),
        ]]);

        let app = load_test_app_with_agents(
            &mut wezterm,
            vec![(
                "alpha",
                vec![("claude", 20, "working"), ("opencode", 30, "needs_input")],
            )],
        );

        let alpha_monitors = app
            .project_agent_monitors(0)
            .iter()
            .map(|monitor| monitor.display_text())
            .collect::<Vec<_>>();
        assert_eq!(alpha_monitors, vec!["cc:20[w]", "oc:30[i]"]);
        assert!(app.project_agent_monitors(1).is_empty());
    }

    #[test]
    fn hook_state_replaces_provisional_agent_monitor() {
        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![
            pane_with_cwd(10, 1, 1, "file:///tmp/repos/nerve_center"),
            pane_with_cwd(20, 2, 1, "file:///tmp/repos/alpha"),
        ]]);
        let mut app =
            App::load_with_projects(&mut wezterm, test_projects()).expect("app should load");

        app.execute_project_command("agent claude", &mut wezterm)
            .expect("agent command should succeed");
        assert_eq!(
            app.project_agent_monitors(0)
                .iter()
                .map(|monitor| monitor.display_text())
                .collect::<Vec<_>>(),
            vec!["cc:21[s]"]
        );

        app.replace_workspace(test_snapshot_with_agents(vec![(
            "alpha",
            vec![("claude", 21, "working")],
        )]));
        app.refresh(&mut wezterm).expect("refresh should succeed");

        assert_eq!(
            app.project_agent_monitors(0)
                .iter()
                .map(|monitor| monitor.display_text())
                .collect::<Vec<_>>(),
            vec!["cc:21[w]"]
        );
    }

    #[test]
    fn refresh_ignores_local_pane_state_files_without_snapshot_agents() {
        let _env_lock = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let state_dir = test_sandbox("refresh-ignores-local-pane-state");
        write_file(
            &state_dir.join("20"),
            r#"{"source":"claude","effective_state":"working"}"#,
        );

        unsafe {
            env::set_var("NERVE_CENTER_DATA_DIR", root_as_str(&state_dir));
        }

        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![
            pane_with_cwd(10, 1, 1, "file:///tmp/repos/nerve_center"),
            pane_with_cwd(20, 2, 1, "file:///tmp/repos/alpha"),
        ]]);
        let mut app =
            App::load_with_projects(&mut wezterm, test_projects()).expect("app should load");

        let result = app.refresh(&mut wezterm);

        unsafe {
            env::remove_var("NERVE_CENTER_DATA_DIR");
        }

        result.expect("refresh should succeed");
        assert!(app.project_agent_monitors(0).is_empty());
    }

    #[test]
    fn builds_project_tree_with_root_first_and_worktrees_nested() {
        let projects = build_project_entries(vec![
            GitProjectProbe {
                name: "codex-hooks".to_string(),
                cwd: "/home/test/repos/nerve_center.codex-hooks".to_string(),
                branch: "codex-hooks".to_string(),
                status_summary: ProjectStatusSummary::default(),
                root_name: "nerve_center".to_string(),
                root_cwd: "/home/test/repos/nerve_center".to_string(),
                common_dir: "/home/test/repos/nerve_center/.git".to_string(),
                is_root: false,
            },
            GitProjectProbe {
                name: "alpha".to_string(),
                cwd: "/home/test/repos/alpha".to_string(),
                branch: "main".to_string(),
                status_summary: ProjectStatusSummary::default(),
                root_name: "alpha".to_string(),
                root_cwd: "/home/test/repos/alpha".to_string(),
                common_dir: "/home/test/repos/alpha/.git".to_string(),
                is_root: true,
            },
            GitProjectProbe {
                name: "nerve_center".to_string(),
                cwd: "/home/test/repos/nerve_center".to_string(),
                branch: "main".to_string(),
                status_summary: ProjectStatusSummary::default(),
                root_name: "nerve_center".to_string(),
                root_cwd: "/home/test/repos/nerve_center".to_string(),
                common_dir: "/home/test/repos/nerve_center/.git".to_string(),
                is_root: true,
            },
        ]);

        assert_eq!(
            projects,
            vec![
                ProjectEntry {
                    name: "alpha".to_string(),
                    cwd: "/home/test/repos/alpha".to_string(),
                    branch: "main".to_string(),
                    status_summary: ProjectStatusSummary::default(),
                    root_name: "alpha".to_string(),
                    root_cwd: "/home/test/repos/alpha".to_string(),
                    kind: ProjectKind::Root,
                },
                ProjectEntry {
                    name: "nerve_center".to_string(),
                    cwd: "/home/test/repos/nerve_center".to_string(),
                    branch: "main".to_string(),
                    status_summary: ProjectStatusSummary::default(),
                    root_name: "nerve_center".to_string(),
                    root_cwd: "/home/test/repos/nerve_center".to_string(),
                    kind: ProjectKind::Root,
                },
                ProjectEntry {
                    name: "codex-hooks".to_string(),
                    cwd: "/home/test/repos/nerve_center.codex-hooks".to_string(),
                    branch: "codex-hooks".to_string(),
                    status_summary: ProjectStatusSummary::default(),
                    root_name: "nerve_center".to_string(),
                    root_cwd: "/home/test/repos/nerve_center".to_string(),
                    kind: ProjectKind::Worktree,
                },
            ]
        );
        assert_eq!(projects[1].name, "nerve_center");
        assert_eq!(projects[2].branch, "codex-hooks");
    }

    #[test]
    fn parses_ascii_project_status_summary_from_porcelain_v2() {
        let status = parse_project_status_output(
            "# branch.oid abcdef0123456789\n\
# branch.head feature/test\n\
# branch.upstream origin/feature/test\n\
# branch.ab +2 -1\n\
1 M. N... 100644 100644 100644 abcdef1 abcdef2 tracked.txt\n\
1 .M N... 100644 100644 100644 abcdef1 abcdef2 dirty.txt\n\
1 D. N... 100644 000000 000000 abcdef1 0000000 removed.txt\n\
u UU N... 100644 100644 100644 100644 abcdef1 abcdef2 abcdef3 conflict.txt\n\
? new.txt\n",
        )
        .expect("status should parse");

        assert_eq!(status.branch, "feature/test");
        assert_eq!(
            status.status_summary,
            ProjectStatusSummary {
                staged: 2,
                modified: 1,
                deleted: 1,
                untracked: 1,
                conflicts: 1,
                ahead: 2,
                behind: 1,
            }
        );
        assert_eq!(status.status_summary.display_text(), "S2 M1 D1 ?1 U1 ^2 v1");
    }

    #[test]
    fn project_entries_from_snapshot_uses_structured_status_summary() {
        let snapshot = crate::workspace::WorkspaceSnapshot {
            protocol_version: 1,
            generated_at_ms: 1,
            project_order: vec!["/tmp/repos/alpha".to_string()],
            projects: std::collections::BTreeMap::from([(
                "/tmp/repos/alpha".to_string(),
                crate::workspace::ProjectSnapshot {
                    id: "/tmp/repos/alpha".to_string(),
                    name: "alpha".to_string(),
                    cwd: "/tmp/repos/alpha".to_string(),
                    root_id: "/tmp/repos/alpha".to_string(),
                    kind: ProjectKind::Root,
                    git: crate::workspace::ProjectGitState {
                        branch: "main".to_string(),
                        status: "clean ^1".to_string(),
                        status_summary: ProjectStatusSummary {
                            ahead: 1,
                            ..ProjectStatusSummary::default()
                        },
                    },
                    ..crate::workspace::ProjectSnapshot::default()
                },
            )]),
            warnings: Vec::new(),
        };

        let projects = super::project_entries_from_snapshot(&snapshot);

        assert_eq!(projects[0].status_summary.ahead, 1);
    }

    #[test]
    fn status_line_surfaces_latest_workspace_warning() {
        let snapshot = crate::workspace::WorkspaceSnapshot {
            protocol_version: 1,
            generated_at_ms: 1,
            project_order: Vec::new(),
            projects: std::collections::BTreeMap::new(),
            warnings: vec!["operation git_pull failed for /tmp/repos/alpha".to_string()],
        };

        let app = App::from_snapshot(snapshot);

        assert!(app
            .status_line()
            .contains("WARN: operation git_pull failed for /tmp/repos/alpha"));
    }

    #[test]
    fn project_operation_text_formats_operation_state() {
        let snapshot = crate::workspace::WorkspaceSnapshot {
            protocol_version: 1,
            generated_at_ms: 1,
            project_order: vec!["/tmp/repos/alpha".to_string()],
            projects: std::collections::BTreeMap::from([(
                "/tmp/repos/alpha".to_string(),
                crate::workspace::ProjectSnapshot {
                    id: "/tmp/repos/alpha".to_string(),
                    name: "alpha".to_string(),
                    cwd: "/tmp/repos/alpha".to_string(),
                    root_id: "/tmp/repos/alpha".to_string(),
                    kind: ProjectKind::Root,
                    operation: crate::workspace::ProjectOperationState {
                        kind: "git_pull".to_string(),
                        message: "running".to_string(),
                    },
                    ..crate::workspace::ProjectSnapshot::default()
                },
            )]),
            warnings: Vec::new(),
        };

        let app = App::from_snapshot(snapshot);

        assert_eq!(
            app.project_operation_text(0).as_deref(),
            Some("op:git_pull[running]")
        );
    }

    #[test]
    fn trims_worktree_branch_input_without_renaming_it() {
        assert_eq!(
            super::normalize_worktree_branch_input(" feature/BOOST-3432 "),
            Some("feature/BOOST-3432".to_string())
        );
        assert_eq!(super::normalize_worktree_branch_input("   "), None);
    }

    #[test]
    fn removing_dirty_worktree_fails_before_branch_delete() {
        let sandbox = test_sandbox("remove-dirty-worktree");
        let root = sandbox.join("root");
        let worktree = sandbox.join("root.review");

        git(
            &sandbox,
            &["init", "--initial-branch=main", root_as_str(&root)],
        );
        write_file(&root.join("tracked.txt"), "hello\n");
        git(
            &root,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "add",
                "tracked.txt",
            ],
        );
        git(
            &root,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                "init",
            ],
        );
        git(
            &root,
            &["worktree", "add", "-b", "review", root_as_str(&worktree)],
        );
        write_file(&worktree.join("dirty.txt"), "dirty\n");

        let remove_error = run_git_worktree_remove(root_as_str(&root), root_as_str(&worktree))
            .expect_err("dirty worktree removal should fail");
        assert!(remove_error
            .to_string()
            .contains("git worktree remove failed"));

        run_git_branch_delete(root_as_str(&root), "review")
            .expect_err("branch should still be checked out in dirty worktree");
    }

    #[derive(Debug)]
    struct WorktreeFixture {
        worktree: PathBuf,
        projects: Vec<ProjectEntry>,
    }

    fn create_worktree_fixture(name: &str) -> WorktreeFixture {
        let sandbox = test_sandbox(name);
        let root = sandbox.join("repo");
        let worktree = sandbox.join("repo.feature");
        let remote = sandbox.join("remote.git");

        git(
            &sandbox,
            &["init", "--initial-branch=main", root_as_str(&root)],
        );
        write_file(&root.join("tracked.txt"), "hello\n");
        git_commit_all(&root, "init");
        git(
            &root,
            &["worktree", "add", "-b", "feature", root_as_str(&worktree)],
        );
        write_file(&worktree.join("tracked.txt"), "hello\nfeature\n");
        git_commit_all(&worktree, "feature change");
        git(&sandbox, &["init", "--bare", root_as_str(&remote)]);

        let projects = super::discover_projects_in(std::slice::from_ref(&sandbox))
            .expect("projects should be discovered");
        WorktreeFixture { worktree, projects }
    }

    fn create_repo_in(parent: &Path, name: &str) -> PathBuf {
        let root = parent.join(name);
        git(
            parent,
            &["init", "--initial-branch=main", root_as_str(&root)],
        );
        write_file(&root.join("tracked.txt"), "hello\n");
        git_commit_all(&root, "init");
        root
    }

    fn test_sandbox(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should move forward")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("nerve-center-{name}-{unique}"));
        fs::create_dir_all(&dir).expect("sandbox should be created");
        dir
    }

    fn git(workdir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(workdir)
            .output()
            .expect("git command should start");
        if output.status.success() {
            return;
        }

        panic!(
            "git command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    fn git_commit_all(workdir: &Path, message: &str) {
        git(
            workdir,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "add",
                ".",
            ],
        );
        git(
            workdir,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                message,
            ],
        );
    }

    fn chmod_executable(path: &Path) {
        let output = Command::new("chmod")
            .args(["+x", root_as_str(path)])
            .output()
            .expect("chmod should start");
        assert!(output.status.success(), "chmod should succeed");
    }

    fn write_file(path: &Path, content: &str) {
        fs::write(path, content).expect("file should be written");
    }

    fn root_as_str(path: &Path) -> &str {
        path.to_str().expect("path should be valid utf-8")
    }
}
