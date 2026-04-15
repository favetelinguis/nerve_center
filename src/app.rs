use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};

use crate::input::AppAction;
use crate::wezterm::{
    NewTabCommand, PaneInfo, SplitDirection, TuiTabLayout, WeztermClient, find_pane,
    listable_panes, tui_pane_id_from_env, tui_tab_layout,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Normal,
    Insert,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AppTab {
    #[default]
    Projects,
    Panes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectKind {
    Root,
    Worktree,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectEntry {
    pub name: String,
    pub cwd: String,
    pub branch: String,
    pub status_summary: ProjectStatusSummary,
    pub root_name: String,
    pub root_cwd: String,
    pub kind: ProjectKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProjectStatusSummary {
    pub staged: usize,
    pub modified: usize,
    pub deleted: usize,
    pub untracked: usize,
    pub conflicts: usize,
    pub ahead: usize,
    pub behind: usize,
}

impl ProjectStatusSummary {
    fn has_local_changes(&self) -> bool {
        self.staged > 0
            || self.modified > 0
            || self.deleted > 0
            || self.untracked > 0
            || self.conflicts > 0
    }

    pub fn display_text(&self) -> String {
        let mut parts = Vec::new();

        if !self.has_local_changes() {
            parts.push("clean".to_string());
        } else {
            if self.staged > 0 {
                parts.push(format!("S{}", self.staged));
            }
            if self.modified > 0 {
                parts.push(format!("M{}", self.modified));
            }
            if self.deleted > 0 {
                parts.push(format!("D{}", self.deleted));
            }
            if self.untracked > 0 {
                parts.push(format!("?{}", self.untracked));
            }
            if self.conflicts > 0 {
                parts.push(format!("U{}", self.conflicts));
            }
        }

        if self.ahead > 0 {
            parts.push(format!("^{}", self.ahead));
        }
        if self.behind > 0 {
            parts.push(format!("v{}", self.behind));
        }

        parts.join(" ")
    }
}

#[derive(Debug, Clone)]
pub struct PaneRow {
    pub pane: PaneInfo,
}

impl PaneRow {
    pub fn is_attached(&self, attached_pane_id: Option<u64>) -> bool {
        attached_pane_id == Some(self.pane.pane_id)
    }
}

#[derive(Debug, Clone)]
enum InputMode {
    WorktreeBranch { branch: String },
    Command { tab: AppTab, command: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProjectCommand {
    Remove,
    Merge { target: Option<String> },
    Pr { target: Option<String> },
    Land { target: Option<String> },
}

#[derive(Debug)]
pub struct App {
    rows: Vec<PaneRow>,
    selected_index: usize,
    projects: Vec<ProjectEntry>,
    selected_project_index: usize,
    repos_root: String,
    active_tab: AppTab,
    mode: Mode,
    tui_pane_id: u64,
    attached_pane_id: Option<u64>,
    input_mode: Option<InputMode>,
    status_message: String,
    last_error: Option<String>,
    should_quit: bool,
}

impl App {
    pub fn load<W: WeztermClient>(wezterm: &mut W) -> Result<Self> {
        let repos_root = repos_root_from_env()?;
        let projects = discover_projects_in(&repos_root)?;
        Self::load_with_projects_in(wezterm, repos_root, projects)
    }

    #[cfg(test)]
    fn load_with_projects<W: WeztermClient>(
        wezterm: &mut W,
        projects: Vec<ProjectEntry>,
    ) -> Result<Self> {
        let repos_root = infer_repos_root(&projects).unwrap_or_else(|| "/tmp/repos".to_string());
        Self::load_with_projects_in(wezterm, repos_root, projects)
    }

    fn load_with_projects_in<W: WeztermClient>(
        wezterm: &mut W,
        repos_root: String,
        projects: Vec<ProjectEntry>,
    ) -> Result<Self> {
        let tui_pane_id = tui_pane_id_from_env()?;
        let panes = wezterm.list_panes()?;

        let mut app = Self {
            rows: Vec::new(),
            selected_index: 0,
            projects: Vec::new(),
            selected_project_index: 0,
            repos_root,
            active_tab: AppTab::Projects,
            mode: Mode::Normal,
            tui_pane_id,
            attached_pane_id: None,
            input_mode: None,
            status_message: String::new(),
            last_error: None,
            should_quit: false,
        };
        app.replace_projects(projects, None);
        app.replace_rows(panes)?;
        app.set_status(format!(
            "Loaded {} projects and {} panes",
            app.projects.len(),
            app.rows.len()
        ));
        Ok(app)
    }

    pub fn active_tab(&self) -> AppTab {
        self.active_tab
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn projects(&self) -> &[ProjectEntry] {
        &self.projects
    }

    pub fn selected_project_index(&self) -> usize {
        self.selected_project_index
    }

    pub fn is_input_active(&self) -> bool {
        self.input_mode.is_some()
    }

    pub fn rows(&self) -> &[PaneRow] {
        &self.rows
    }

    pub fn selected_index(&self) -> usize {
        self.selected_index
    }

    pub fn attached_pane_id(&self) -> Option<u64> {
        self.attached_pane_id
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
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
        format!("{prefix}: {message}")
    }

    pub fn input_line(&self) -> String {
        match self.input_mode.as_ref() {
            Some(InputMode::WorktreeBranch { branch }) => {
                let root_name = self
                    .selected_project()
                    .map(|project| project.root_name.as_str())
                    .unwrap_or("-");
                format!("Create worktree branch for {root_name}: {branch}")
            }
            Some(InputMode::Command { tab, command }) => {
                let scope = match tab {
                    AppTab::Projects => self
                        .selected_project()
                        .map(|project| project.name.as_str())
                        .unwrap_or("-"),
                    AppTab::Panes => "panes",
                };
                let tab = match tab {
                    AppTab::Projects => "Projects",
                    AppTab::Panes => "Panes",
                };
                format!("{tab} command for {scope}: :{command}")
            }
            None => match self.active_tab {
                AppTab::Projects => {
                    "Ctrl-W create worktree branch | : command on selected project".to_string()
                }
                AppTab::Panes => ": command on active tab".to_string(),
            },
        }
    }

    pub fn record_error(&mut self, error: impl Into<String>) {
        self.last_error = Some(error.into());
    }

    pub fn apply<W: WeztermClient>(&mut self, action: AppAction, wezterm: &mut W) -> Result<()> {
        match action {
            AppAction::SwitchToProjects => {
                self.active_tab = AppTab::Projects;
                self.set_status("Projects tab");
                Ok(())
            }
            AppAction::SwitchToPanes => {
                self.active_tab = AppTab::Panes;
                self.set_status("Panes tab");
                Ok(())
            }
            AppAction::MoveUp => {
                self.move_up();
                Ok(())
            }
            AppAction::MoveDown => {
                self.move_down();
                Ok(())
            }
            AppAction::ProjectMoveUp => {
                self.project_move_up();
                Ok(())
            }
            AppAction::ProjectMoveDown => {
                self.project_move_down();
                Ok(())
            }
            AppAction::StartCreateWorktreeInput => {
                self.start_worktree_input();
                Ok(())
            }
            AppAction::StartCommandInput => {
                self.start_command_input();
                Ok(())
            }
            AppAction::ConfirmInput => self.confirm_input(),
            AppAction::CancelInput => {
                self.cancel_input();
                Ok(())
            }
            AppAction::EditInput(c) => {
                self.edit_input(c);
                Ok(())
            }
            AppAction::DeleteInputChar => {
                self.delete_input_char();
                Ok(())
            }
            AppAction::AttachSelected => self.attach_selected(wezterm),
            AppAction::OpenProjectShell => self.open_project_tab(wezterm, NewTabCommand::Shell),
            AppAction::OpenProjectEditor => self.open_project_tab(wezterm, NewTabCommand::Nvim),
            AppAction::OpenProjectGit => self.open_project_tab(wezterm, NewTabCommand::Lazygit),
            AppAction::Quit => {
                self.should_quit = true;
                self.set_status("Quit");
                Ok(())
            }
            AppAction::ExitInsert => {
                self.mode = Mode::Normal;
                self.set_status("Exited insert mode");
                Ok(())
            }
            AppAction::Forward(text) => self.forward_text(wezterm, &text),
        }
    }

    fn replace_rows(&mut self, panes: Vec<PaneInfo>) -> Result<()> {
        let previous_selection = self.selected_row().map(|row| row.pane.pane_id);
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

        self.selected_index = previous_selection
            .and_then(|pane_id| self.rows.iter().position(|row| row.pane.pane_id == pane_id))
            .unwrap_or(0);

        if self.selected_index >= self.rows.len() {
            self.selected_index = self.rows.len().saturating_sub(1);
        }

        if self.rows.is_empty() {
            self.mode = Mode::Normal;
        }

        Ok(())
    }

    fn selected_row(&self) -> Option<&PaneRow> {
        self.rows.get(self.selected_index)
    }

    fn selected_pane_id(&self) -> Option<u64> {
        self.selected_row().map(|row| row.pane.pane_id)
    }

    fn selected_project(&self) -> Option<&ProjectEntry> {
        self.projects.get(self.selected_project_index)
    }

    fn move_up(&mut self) {
        if !self.rows.is_empty() && self.selected_index > 0 {
            self.selected_index -= 1;
        }
    }

    fn move_down(&mut self) {
        if !self.rows.is_empty() && self.selected_index + 1 < self.rows.len() {
            self.selected_index += 1;
        }
    }

    fn project_move_up(&mut self) {
        if !self.projects.is_empty() && self.selected_project_index > 0 {
            self.selected_project_index -= 1;
        }
    }

    fn project_move_down(&mut self) {
        if !self.projects.is_empty() && self.selected_project_index + 1 < self.projects.len() {
            self.selected_project_index += 1;
        }
    }

    fn start_worktree_input(&mut self) {
        let Some(project) = self.selected_project() else {
            self.record_error("No projects found");
            return;
        };
        let root_name = project.root_name.clone();

        self.input_mode = Some(InputMode::WorktreeBranch {
            branch: String::new(),
        });
        self.set_status(format!("Create worktree branch for {root_name}"));
    }

    fn start_command_input(&mut self) {
        self.input_mode = Some(InputMode::Command {
            tab: self.active_tab,
            command: String::new(),
        });
        self.set_status("Command input");
    }

    fn edit_input(&mut self, c: char) {
        let Some(input_mode) = self.input_mode.as_mut() else {
            return;
        };

        match input_mode {
            InputMode::WorktreeBranch { branch } => branch.push(c),
            InputMode::Command { command, .. } => command.push(c),
        }
        self.last_error = None;
    }

    fn delete_input_char(&mut self) {
        let Some(input_mode) = self.input_mode.as_mut() else {
            return;
        };

        match input_mode {
            InputMode::WorktreeBranch { branch } => {
                branch.pop();
            }
            InputMode::Command { command, .. } => {
                command.pop();
            }
        }
        self.last_error = None;
    }

    fn cancel_input(&mut self) {
        self.input_mode = None;
        self.set_status("Cancelled input");
    }

    fn confirm_input(&mut self) -> Result<()> {
        let Some(input_mode) = self.input_mode.clone() else {
            return Ok(());
        };

        match input_mode {
            InputMode::WorktreeBranch { branch } => self.confirm_worktree_input(&branch),
            InputMode::Command { tab, command } => {
                self.input_mode = None;
                self.execute_command(tab, &command)
            }
        }
    }

    fn confirm_worktree_input(&mut self, branch: &str) -> Result<()> {
        let Some(project) = self.selected_project() else {
            self.record_error("No projects found");
            return Ok(());
        };

        let Some(branch) = normalize_worktree_branch_input(branch) else {
            self.record_error("Worktree branch is empty");
            return Ok(());
        };

        let root_cwd = project.root_cwd.clone();
        let target_cwd = generate_worktree_cwd(&self.repos_root)?;

        run_git_worktree_add(&root_cwd, &branch, &target_cwd)?;

        self.input_mode = None;
        self.reload_projects(Some(&target_cwd))?;
        let worktree_name = Path::new(&target_cwd)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| target_cwd.clone());
        self.set_status(format!("Created worktree {worktree_name} on {branch}"));
        Ok(())
    }

    fn execute_command(&mut self, tab: AppTab, command: &str) -> Result<()> {
        let command = command.trim();
        if command.is_empty() {
            self.set_status("Cancelled command");
            return Ok(());
        }

        match tab {
            AppTab::Projects => self.execute_project_command(command),
            AppTab::Panes => bail!("no commands are implemented for the panes tab"),
        }
    }

    fn execute_project_command(&mut self, command: &str) -> Result<()> {
        match parse_project_command(command)? {
            ProjectCommand::Remove => self.remove_selected_worktree(),
            ProjectCommand::Merge { target } => {
                self.merge_selected_worktree(target.as_deref())?;
                Ok(())
            }
            ProjectCommand::Pr { target } => {
                self.create_pull_request_for_selected_worktree(target.as_deref())?;
                Ok(())
            }
            ProjectCommand::Land { target } => {
                self.merge_selected_worktree(target.as_deref())?;
                self.remove_selected_worktree()
            }
        }
    }

    fn selected_linked_worktree(&self) -> Result<ProjectEntry> {
        let project = self
            .selected_project()
            .cloned()
            .ok_or_else(|| anyhow!("No projects found"))?;

        if project.kind != ProjectKind::Worktree {
            bail!("command only works on linked worktrees");
        }
        if matches!(project.branch.as_str(), "DETACHED" | "N/A") {
            bail!("command requires a branch-backed worktree");
        }

        Ok(project)
    }

    fn resolve_target_branch(
        &self,
        project: &ProjectEntry,
        explicit_target: Option<&str>,
    ) -> Result<String> {
        match explicit_target
            .map(str::trim)
            .filter(|target| !target.is_empty())
        {
            Some(target) => Ok(target.to_string()),
            None => default_target_branch(&project.root_cwd, &self.projects),
        }
    }

    fn merge_destination_cwd(&self, project: &ProjectEntry, target: &str) -> String {
        self.projects
            .iter()
            .find(|candidate| candidate.root_cwd == project.root_cwd && candidate.branch == target)
            .map(|candidate| candidate.cwd.clone())
            .unwrap_or_else(|| project.root_cwd.clone())
    }

    fn merge_selected_worktree(&mut self, explicit_target: Option<&str>) -> Result<()> {
        let project = self.selected_linked_worktree()?;
        let target = self.resolve_target_branch(&project, explicit_target)?;
        if target == project.branch {
            bail!("target branch matches selected worktree branch");
        }

        ensure_clean_worktree(&project.cwd, "selected worktree")?;

        let merge_cwd = self.merge_destination_cwd(&project, &target);
        ensure_clean_worktree(&merge_cwd, "target worktree")?;

        switch_to_branch(&merge_cwd, &target)?;
        fast_forward_target_from_remote(&merge_cwd, &target)?;
        fast_forward_merge_branch(&merge_cwd, &project.branch, &target)?;

        self.reload_projects(Some(&project.cwd))?;
        self.set_status(format!("Merged {} into {}", project.branch, target));
        Ok(())
    }

    fn create_pull_request_for_selected_worktree(
        &mut self,
        explicit_target: Option<&str>,
    ) -> Result<()> {
        let project = self.selected_linked_worktree()?;
        let target = self.resolve_target_branch(&project, explicit_target)?;
        if target == project.branch {
            bail!("target branch matches selected worktree branch");
        }

        ensure_clean_worktree(&project.cwd, "selected worktree")?;

        let remote = branch_remote(&project.cwd, &project.branch)?
            .ok_or_else(|| anyhow!("no git remote configured for {}", project.branch))?;
        push_branch_to_remote(&project.cwd, &remote, &project.branch)?;
        let pr_url = ensure_pull_request(&project.cwd, &project.branch, &target)?;

        self.set_status(format!(
            "PR ready for {} -> {}: {}",
            project.branch, target, pr_url
        ));
        Ok(())
    }

    fn remove_selected_worktree(&mut self) -> Result<()> {
        let project = self.selected_linked_worktree().map_err(|error| {
            if error
                .to_string()
                .contains("command only works on linked worktrees")
            {
                anyhow!("remove only works on linked worktrees")
            } else if error
                .to_string()
                .contains("command requires a branch-backed worktree")
            {
                anyhow!("remove requires a branch-backed worktree")
            } else {
                error
            }
        })?;

        run_git_worktree_remove(&project.root_cwd, &project.cwd)?;
        run_git_branch_delete(&project.root_cwd, &project.branch)?;

        self.reload_projects(Some(&project.root_cwd))?;
        self.set_status(format!(
            "Removed worktree {} and branch {}",
            project.name, project.branch
        ));
        Ok(())
    }

    fn attach_selected<W: WeztermClient>(&mut self, wezterm: &mut W) -> Result<()> {
        let selected_pane_id = match self.selected_pane_id() {
            Some(pane_id) => pane_id,
            None => {
                self.record_error("No selectable panes");
                return Ok(());
            }
        };

        let panes = wezterm.list_panes()?;
        let _selected = find_pane(&panes, selected_pane_id)?;
        let layout = tui_tab_layout(&panes, self.tui_pane_id)?;

        match layout {
            TuiTabLayout::Unsupported { .. } => {
                self.record_error("unsupported layout");
                return Ok(());
            }
            TuiTabLayout::Attached(attached) if attached.pane_id == selected_pane_id => {
                self.mode = Mode::Insert;
                self.attached_pane_id = Some(selected_pane_id);
                self.set_status(format!("Insert mode for pane {selected_pane_id}"));
                return Ok(());
            }
            TuiTabLayout::Attached(attached) => {
                wezterm.move_pane_to_new_tab(attached.pane_id)?;
            }
            TuiTabLayout::Solo => {}
        }

        wezterm.split_pane(self.tui_pane_id, selected_pane_id, SplitDirection::Right)?;
        wezterm.activate_pane(self.tui_pane_id)?;

        self.attached_pane_id = Some(selected_pane_id);
        self.mode = Mode::Insert;
        self.set_status(format!("Insert mode for pane {selected_pane_id}"));
        self.refresh(wezterm)?;

        Ok(())
    }

    fn forward_text<W: WeztermClient>(&mut self, wezterm: &mut W, text: &str) -> Result<()> {
        let attached_pane_id = self
            .attached_pane_id
            .ok_or_else(|| anyhow!("cannot forward keys without an attached pane"))?;
        wezterm.send_text(attached_pane_id, text)?;
        self.last_error = None;
        Ok(())
    }

    fn open_project_tab<W: WeztermClient>(
        &mut self,
        wezterm: &mut W,
        command: NewTabCommand,
    ) -> Result<()> {
        let project = match self.selected_project() {
            Some(project) => project,
            None => {
                self.record_error("No projects found");
                return Ok(());
            }
        };

        wezterm.spawn_new_tab(self.tui_pane_id, &project.cwd, command)?;
        wezterm.activate_pane(self.tui_pane_id)?;
        self.set_status(format!(
            "Opened {} tab for {}",
            command.label(),
            project.name
        ));
        Ok(())
    }

    fn refresh<W: WeztermClient>(&mut self, wezterm: &mut W) -> Result<()> {
        let panes = wezterm.list_panes()?;
        self.replace_rows(panes)?;
        self.last_error = None;
        Ok(())
    }

    fn reload_projects(&mut self, selected_cwd: Option<&str>) -> Result<()> {
        let projects = discover_projects_in(&self.repos_root)?;
        self.replace_projects(projects, selected_cwd);
        Ok(())
    }

    fn replace_projects(&mut self, projects: Vec<ProjectEntry>, selected_cwd: Option<&str>) {
        let previous_selection = selected_cwd
            .map(str::to_string)
            .or_else(|| self.selected_project().map(|project| project.cwd.clone()));
        self.projects = projects;
        self.selected_project_index = previous_selection
            .as_deref()
            .and_then(|cwd| self.projects.iter().position(|project| project.cwd == cwd))
            .unwrap_or(0);

        if self.selected_project_index >= self.projects.len() {
            self.selected_project_index = self.projects.len().saturating_sub(1);
        }
    }

    fn set_status(&mut self, status: impl Into<String>) {
        self.status_message = status.into();
        self.last_error = None;
    }
}

fn repos_root_from_env() -> Result<String> {
    let home = env::var("HOME").context("HOME is not set")?;
    Ok(format!("{home}/repos"))
}

#[cfg(test)]
fn infer_repos_root(projects: &[ProjectEntry]) -> Option<String> {
    projects.first().and_then(|project| {
        Path::new(&project.root_cwd)
            .parent()
            .map(|path| path.to_string_lossy().into_owned())
    })
}

fn discover_projects_in(repos_root: &str) -> Result<Vec<ProjectEntry>> {
    let mut probes = Vec::new();

    for entry in fs::read_dir(repos_root)
        .with_context(|| format!("failed to read repos root {repos_root}"))?
    {
        let entry = entry.with_context(|| format!("failed to read entry in {repos_root}"))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        if let Some(probe) = inspect_git_project(&path)? {
            probes.push(probe);
        }
    }

    Ok(build_project_entries(probes))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitProjectProbe {
    name: String,
    cwd: String,
    branch: String,
    status_summary: ProjectStatusSummary,
    root_name: String,
    root_cwd: String,
    common_dir: String,
    is_root: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitStatusProbe {
    branch: String,
    status_summary: ProjectStatusSummary,
}

fn inspect_git_project(path: &Path) -> Result<Option<GitProjectProbe>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args([
            "rev-parse",
            "--show-toplevel",
            "--git-dir",
            "--git-common-dir",
        ])
        .output()
        .with_context(|| format!("failed to inspect git metadata for {}", path.display()))?;

    if !output.status.success() {
        return Ok(None);
    }

    let stdout =
        String::from_utf8(output.stdout).context("git rev-parse stdout was not valid UTF-8")?;
    let mut lines = stdout.lines();
    let cwd = lines
        .next()
        .context("git rev-parse missing --show-toplevel output")?
        .trim()
        .to_string();
    let git_dir = lines
        .next()
        .context("git rev-parse missing --git-dir output")?
        .trim()
        .to_string();
    let common_dir = lines
        .next()
        .context("git rev-parse missing --git-common-dir output")?
        .trim()
        .to_string();

    let cwd_path = PathBuf::from(&cwd);
    let git_dir = resolve_git_path(&cwd_path, &git_dir);
    let common_dir = resolve_git_path(&cwd_path, &common_dir);
    let root_cwd = common_dir
        .parent()
        .context("git common dir did not have a parent directory")?
        .to_path_buf();
    let root_name = root_cwd
        .file_name()
        .context("git root did not have a final path component")?
        .to_string_lossy()
        .into_owned();
    let name = cwd_path
        .file_name()
        .context("project path did not have a final path component")?
        .to_string_lossy()
        .into_owned();
    let status = read_project_status(path)?;

    Ok(Some(GitProjectProbe {
        name,
        cwd,
        branch: status.branch,
        status_summary: status.status_summary,
        root_name,
        root_cwd: root_cwd.to_string_lossy().into_owned(),
        common_dir: common_dir.to_string_lossy().into_owned(),
        is_root: git_dir == common_dir,
    }))
}

fn resolve_git_path(base: &Path, git_path: &str) -> PathBuf {
    let path = Path::new(git_path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn read_branch_name(path: &Path) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["branch", "--show-current"])
        .output()
        .with_context(|| format!("failed to read branch for {}", path.display()))?;

    if !output.status.success() {
        return Ok("N/A".to_string());
    }

    let branch = String::from_utf8(output.stdout)
        .context("git branch stdout was not valid UTF-8")?
        .trim()
        .to_string();
    if branch.is_empty() {
        Ok("DETACHED".to_string())
    } else {
        Ok(branch)
    }
}

fn read_project_status(path: &Path) -> Result<GitStatusProbe> {
    let output = Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(path)
        .args(["status", "--porcelain=v2", "--branch"])
        .output()
        .with_context(|| format!("failed to read git status for {}", path.display()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "git status failed for {}: {}",
            path.display(),
            stderr.trim()
        );
    }

    let stdout =
        String::from_utf8(output.stdout).context("git status stdout was not valid UTF-8")?;
    parse_project_status_output(&stdout)
}

fn parse_project_status_output(stdout: &str) -> Result<GitStatusProbe> {
    let mut branch = "N/A".to_string();
    let mut status_summary = ProjectStatusSummary::default();

    for line in stdout.lines() {
        if let Some(head) = line.strip_prefix("# branch.head ") {
            branch = if head == "(detached)" {
                "DETACHED".to_string()
            } else {
                head.to_string()
            };
            continue;
        }

        if let Some(counts) = line.strip_prefix("# branch.ab +") {
            let (ahead, behind) = counts
                .split_once(" -")
                .context("git status branch.ab header was malformed")?;
            status_summary.ahead = ahead
                .parse()
                .context("git status ahead count was not a number")?;
            status_summary.behind = behind
                .parse()
                .context("git status behind count was not a number")?;
            continue;
        }

        if line.starts_with("u ") {
            status_summary.conflicts += 1;
            continue;
        }

        if line.starts_with("? ") {
            status_summary.untracked += 1;
            continue;
        }

        if !matches!(line.as_bytes().first(), Some(b'1' | b'2')) {
            continue;
        }

        let xy = line
            .split_whitespace()
            .nth(1)
            .context("git status record missing XY field")?;
        let mut xy_chars = xy.chars();
        let staged_status = xy_chars
            .next()
            .context("git status XY field was missing staged status")?;
        let worktree_status = xy_chars
            .next()
            .context("git status XY field was missing worktree status")?;
        if xy_chars.next().is_some() {
            bail!("git status XY field was longer than expected");
        }

        if staged_status != '.' {
            status_summary.staged += 1;
        }
        if staged_status == 'D' || worktree_status == 'D' {
            status_summary.deleted += 1;
        }
        if worktree_status != '.' && worktree_status != 'D' {
            status_summary.modified += 1;
        }
    }

    Ok(GitStatusProbe {
        branch,
        status_summary,
    })
}

fn build_project_entries(probes: Vec<GitProjectProbe>) -> Vec<ProjectEntry> {
    let mut groups = BTreeMap::<String, Vec<GitProjectProbe>>::new();
    for probe in probes {
        groups
            .entry(probe.common_dir.clone())
            .or_default()
            .push(probe);
    }

    let mut groups = groups.into_values().collect::<Vec<_>>();
    groups.sort_by(|left, right| left[0].root_name.cmp(&right[0].root_name));

    let mut projects = Vec::new();
    for mut group in groups {
        group.sort_by(|left, right| match (left.is_root, right.is_root) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => left.name.cmp(&right.name),
        });

        for probe in group {
            projects.push(ProjectEntry {
                name: probe.name,
                cwd: probe.cwd,
                branch: probe.branch,
                status_summary: probe.status_summary,
                root_name: probe.root_name,
                root_cwd: probe.root_cwd,
                kind: if probe.is_root {
                    ProjectKind::Root
                } else {
                    ProjectKind::Worktree
                },
            });
        }
    }

    projects
}

fn parse_project_command(command: &str) -> Result<ProjectCommand> {
    let mut parts = command.split_whitespace();
    let Some(name) = parts.next() else {
        bail!("empty projects command")
    };
    let target = parts.next().map(str::to_string);
    if parts.next().is_some() {
        bail!("too many arguments for projects command: {command}")
    }

    match name {
        "remove" => {
            if target.is_some() {
                bail!("remove does not take a target branch")
            }
            Ok(ProjectCommand::Remove)
        }
        "merge" => Ok(ProjectCommand::Merge { target }),
        "pr" => Ok(ProjectCommand::Pr { target }),
        "land" => Ok(ProjectCommand::Land { target }),
        _ => bail!("unknown projects command: {command}"),
    }
}

fn default_target_branch(root_cwd: &str, projects: &[ProjectEntry]) -> Result<String> {
    if let Some(branch) = remote_default_branch(root_cwd)? {
        return Ok(branch);
    }

    for branch in ["main", "master"] {
        if local_branch_exists(root_cwd, branch)? {
            return Ok(branch.to_string());
        }
    }

    if let Some(branch) = projects.iter().find_map(|project| {
        (project.root_cwd == root_cwd
            && project.kind == ProjectKind::Root
            && !matches!(project.branch.as_str(), "DETACHED" | "N/A"))
        .then(|| project.branch.clone())
    }) {
        return Ok(branch);
    }

    bail!("could not determine default branch for {root_cwd}")
}

fn remote_default_branch(root_cwd: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .args([
            "-C",
            root_cwd,
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ])
        .output()
        .with_context(|| format!("failed to inspect remote default branch for {root_cwd}"))?;

    if !output.status.success() {
        return Ok(None);
    }

    let branch = String::from_utf8(output.stdout)
        .context("git symbolic-ref stdout was not valid UTF-8")?
        .trim()
        .to_string();
    Ok(branch.rsplit('/').next().map(str::to_string))
}

fn local_branch_exists(root_cwd: &str, branch: &str) -> Result<bool> {
    let status = Command::new("git")
        .args([
            "-C",
            root_cwd,
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .status()
        .with_context(|| format!("failed to inspect branch {branch} in {root_cwd}"))?;
    Ok(status.success())
}

fn ensure_clean_worktree(cwd: &str, label: &str) -> Result<()> {
    let output = Command::new("git")
        .args(["-C", cwd, "status", "--porcelain"])
        .output()
        .with_context(|| format!("failed to read git status for {cwd}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git status failed for {cwd}: {}", stderr.trim());
    }

    let status =
        String::from_utf8(output.stdout).context("git status stdout was not valid UTF-8")?;
    if status.trim().is_empty() {
        return Ok(());
    }

    bail!("{label} has uncommitted or untracked changes")
}

fn switch_to_branch(cwd: &str, branch: &str) -> Result<()> {
    if read_branch_name(Path::new(cwd))? == branch {
        return Ok(());
    }

    run_git(
        cwd,
        &["switch", branch],
        &format!("failed to switch {cwd} to {branch}"),
    )
}

fn branch_remote(cwd: &str, branch: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .args([
            "-C",
            cwd,
            "config",
            "--get",
            &format!("branch.{branch}.remote"),
        ])
        .output()
        .with_context(|| format!("failed to inspect remote for branch {branch} in {cwd}"))?;

    if output.status.success() {
        let remote = String::from_utf8(output.stdout)
            .context("git config stdout was not valid UTF-8")?
            .trim()
            .to_string();
        if !remote.is_empty() {
            return Ok(Some(remote));
        }
    }

    let remotes = Command::new("git")
        .args(["-C", cwd, "remote"])
        .output()
        .with_context(|| format!("failed to list remotes for {cwd}"))?;
    if !remotes.status.success() {
        return Ok(None);
    }

    let stdout =
        String::from_utf8(remotes.stdout).context("git remote stdout was not valid UTF-8")?;
    Ok(stdout
        .lines()
        .find(|remote| *remote == "origin")
        .map(str::to_string))
}

fn remote_tracking_ref_exists(cwd: &str, remote: &str, branch: &str) -> Result<bool> {
    let status = Command::new("git")
        .args([
            "-C",
            cwd,
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/remotes/{remote}/{branch}"),
        ])
        .status()
        .with_context(|| format!("failed to inspect remote branch {remote}/{branch} in {cwd}"))?;
    Ok(status.success())
}

fn fast_forward_target_from_remote(cwd: &str, target: &str) -> Result<()> {
    let Some(remote) = branch_remote(cwd, target)? else {
        return Ok(());
    };

    run_git(
        cwd,
        &["fetch", &remote, target],
        &format!("failed to fetch {remote}/{target} for {cwd}"),
    )?;

    if !remote_tracking_ref_exists(cwd, &remote, target)? {
        return Ok(());
    }

    let remote_ref = format!("{remote}/{target}");
    run_git(
        cwd,
        &["merge", "--ff-only", &remote_ref],
        &format!("failed to fast-forward {target} from {remote_ref}"),
    )
}

fn fast_forward_merge_branch(cwd: &str, source_branch: &str, target: &str) -> Result<()> {
    run_git(
        cwd,
        &["merge", "--ff-only", source_branch],
        &format!("failed to fast-forward merge {source_branch} into {target}"),
    )
}

fn push_branch_to_remote(cwd: &str, remote: &str, branch: &str) -> Result<()> {
    run_git(
        cwd,
        &["push", "-u", remote, branch],
        &format!("failed to push {branch} to {remote}"),
    )
}

fn ensure_pull_request(cwd: &str, branch: &str, target: &str) -> Result<String> {
    if let Some(url) = existing_pull_request_url(cwd)? {
        return Ok(url);
    }

    run_gh_capture(
        cwd,
        &["pr", "create", "--base", target, "--head", branch, "--fill"],
        &format!("failed to create PR for {branch} -> {target}"),
    )
    .map(|url| url.trim().to_string())
}

fn existing_pull_request_url(cwd: &str) -> Result<Option<String>> {
    let output = Command::new("gh")
        .args(["pr", "view", "--json", "url", "--jq", ".url"])
        .current_dir(cwd)
        .output()
        .with_context(|| format!("failed to inspect PR state for {cwd}"))?;

    if !output.status.success() {
        return Ok(None);
    }

    let url = String::from_utf8(output.stdout)
        .context("gh pr view stdout was not valid UTF-8")?
        .trim()
        .to_string();
    if url.is_empty() {
        Ok(None)
    } else {
        Ok(Some(url))
    }
}

fn run_git(cwd: &str, args: &[&str], failure_context: &str) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .with_context(|| failure_context.to_string())?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!("{failure_context}: {}", stderr.trim())
}

fn run_gh_capture(cwd: &str, args: &[&str], failure_context: &str) -> Result<String> {
    let output = Command::new("gh")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| failure_context.to_string())?;

    if output.status.success() {
        return String::from_utf8(output.stdout)
            .context("gh stdout was not valid UTF-8")
            .map(|stdout| stdout.trim().to_string());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!("{failure_context}: {}", stderr.trim())
}

fn normalize_worktree_branch_input(input: &str) -> Option<String> {
    let branch = input.trim();
    (!branch.is_empty()).then(|| branch.to_string())
}

fn generate_worktree_cwd(repos_root: &str) -> Result<String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before unix epoch")?;
    let base = format!(
        "wt-{}-{:03}-{}",
        now.as_secs(),
        now.subsec_millis(),
        std::process::id()
    );

    for attempt in 0..1000 {
        let name = if attempt == 0 {
            base.clone()
        } else {
            format!("{base}-{attempt}")
        };
        let cwd = format!("{repos_root}/{name}");
        if !Path::new(&cwd).exists() {
            return Ok(cwd);
        }
    }

    bail!("failed to allocate a unique worktree directory name")
}

fn run_git_worktree_add(root_cwd: &str, slug: &str, target_cwd: &str) -> Result<()> {
    let output = Command::new("git")
        .args(["-C", root_cwd, "worktree", "add", "-b", slug, target_cwd])
        .output()
        .with_context(|| format!("failed to create worktree from {root_cwd}"))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!("git worktree add failed: {}", stderr.trim())
}

fn run_git_worktree_remove(root_cwd: &str, target_cwd: &str) -> Result<()> {
    let output = Command::new("git")
        .args(["-C", root_cwd, "worktree", "remove", target_cwd])
        .output()
        .with_context(|| format!("failed to remove worktree {target_cwd}"))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!("git worktree remove failed: {}", stderr.trim())
}

fn run_git_branch_delete(root_cwd: &str, branch: &str) -> Result<()> {
    let output = Command::new("git")
        .args(["-C", root_cwd, "branch", "-d", branch])
        .output()
        .with_context(|| format!("failed to delete branch {branch} from {root_cwd}"))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!("git branch -d failed: {}", stderr.trim())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    use anyhow::Result;

    use super::{
        App, AppTab, GitProjectProbe, Mode, ProjectEntry, ProjectKind, ProjectStatusSummary,
        build_project_entries, parse_project_command, parse_project_status_output,
        run_git_branch_delete, run_git_worktree_remove,
    };
    use crate::input::AppAction;
    use crate::wezterm::{NewTabCommand, PaneInfo, SplitDirection, WeztermClient};

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
            command: NewTabCommand,
        },
    }

    #[derive(Debug)]
    struct FakeWezterm {
        snapshots: VecDeque<Vec<PaneInfo>>,
        calls: Vec<Call>,
    }

    impl FakeWezterm {
        fn new(snapshots: Vec<Vec<PaneInfo>>) -> Self {
            Self {
                snapshots: snapshots.into(),
                calls: Vec::new(),
            }
        }

        fn current_snapshot(&self) -> Vec<PaneInfo> {
            self.snapshots
                .front()
                .expect("at least one snapshot is required")
                .clone()
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
            Ok(())
        }

        fn activate_pane(&mut self, pane_id: u64) -> Result<()> {
            self.calls.push(Call::ActivatePane(pane_id));
            Ok(())
        }

        fn send_text(&mut self, pane_id: u64, text: &str) -> Result<()> {
            self.calls.push(Call::SendText {
                pane_id,
                text: text.to_string(),
            });
            Ok(())
        }

        fn spawn_new_tab(&mut self, pane_id: u64, cwd: &str, command: NewTabCommand) -> Result<()> {
            self.calls.push(Call::SpawnNewTab {
                pane_id,
                cwd: cwd.to_string(),
                command,
            });
            Ok(())
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
            tty_name: format!("/dev/pts/{pane_id}"),
        }
    }

    fn set_wezterm_pane() {
        unsafe {
            std::env::set_var("WEZTERM_PANE", "10");
        }
    }

    #[test]
    fn split_pane_when_tui_is_alone() {
        set_wezterm_pane();
        let snapshots = vec![
            vec![pane(10, 1, 1), pane(20, 2, 1)],
            vec![pane(10, 1, 1), pane(20, 2, 1)],
            vec![pane(10, 1, 1), pane(20, 1, 1)],
        ];
        let mut wezterm = FakeWezterm::new(snapshots);
        let mut app =
            App::load_with_projects(&mut wezterm, test_projects()).expect("app should load");

        app.apply(AppAction::AttachSelected, &mut wezterm)
            .expect("attach should succeed");

        assert_eq!(app.mode(), Mode::Insert);
        assert_eq!(app.attached_pane_id(), Some(20));
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
    fn switching_panes_moves_old_neighbor_out_first() {
        set_wezterm_pane();
        let snapshots = vec![
            vec![pane(10, 1, 1), pane(20, 1, 1), pane(30, 2, 1)],
            vec![pane(10, 1, 1), pane(20, 1, 1), pane(30, 2, 1)],
            vec![pane(10, 1, 1), pane(30, 1, 1), pane(20, 3, 1)],
        ];
        let mut wezterm = FakeWezterm::new(snapshots);
        let mut app =
            App::load_with_projects(&mut wezterm, test_projects()).expect("app should load");
        app.apply(AppAction::MoveDown, &mut wezterm)
            .expect("selection should move");

        app.apply(AppAction::AttachSelected, &mut wezterm)
            .expect("switch should succeed");

        assert_eq!(app.mode(), Mode::Insert);
        assert_eq!(app.attached_pane_id(), Some(30));
        assert_eq!(
            wezterm.calls,
            vec![
                Call::ListPanes,
                Call::ListPanes,
                Call::MovePaneToNewTab(20),
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
    fn selecting_currently_attached_pane_skips_layout_mutation() {
        set_wezterm_pane();
        let snapshots = vec![vec![pane(10, 1, 1), pane(20, 1, 1), pane(30, 2, 1)]];
        let mut wezterm = FakeWezterm::new(snapshots);
        let mut app =
            App::load_with_projects(&mut wezterm, test_projects()).expect("app should load");

        app.apply(AppAction::AttachSelected, &mut wezterm)
            .expect("attach should succeed");

        assert_eq!(app.mode(), Mode::Insert);
        assert_eq!(app.attached_pane_id(), Some(20));
        assert_eq!(wezterm.calls, vec![Call::ListPanes, Call::ListPanes]);
    }

    #[test]
    fn unsupported_layout_does_not_run_commands() {
        set_wezterm_pane();
        let snapshots = vec![vec![
            pane(10, 1, 1),
            pane(20, 1, 1),
            pane(30, 1, 1),
            pane(40, 2, 1),
        ]];
        let mut wezterm = FakeWezterm::new(snapshots);
        let mut app =
            App::load_with_projects(&mut wezterm, test_projects()).expect("app should load");

        app.apply(AppAction::MoveDown, &mut wezterm)
            .expect("selection should move");
        app.apply(AppAction::AttachSelected, &mut wezterm)
            .expect("unsupported layout should not error");

        assert_eq!(app.mode(), Mode::Normal);
        assert!(app.status_line().contains("unsupported layout"));
        assert_eq!(wezterm.calls, vec![Call::ListPanes, Call::ListPanes]);
    }

    #[test]
    fn insert_mode_forwards_text_to_attached_pane() {
        set_wezterm_pane();
        let snapshots = vec![vec![pane(10, 1, 1), pane(20, 1, 1)]];
        let mut wezterm = FakeWezterm::new(snapshots);
        let mut app =
            App::load_with_projects(&mut wezterm, test_projects()).expect("app should load");
        app.apply(AppAction::AttachSelected, &mut wezterm)
            .expect("attach should succeed");

        app.apply(AppAction::Forward("x".to_string()), &mut wezterm)
            .expect("forward should succeed");

        assert_eq!(
            wezterm.calls,
            vec![
                Call::ListPanes,
                Call::ListPanes,
                Call::SendText {
                    pane_id: 20,
                    text: "x".to_string(),
                },
            ]
        );
    }

    #[test]
    fn loads_on_projects_tab_by_default() {
        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![pane(10, 1, 1), pane(20, 2, 1)]]);
        let app = App::load_with_projects(&mut wezterm, test_projects()).expect("app should load");

        assert_eq!(app.active_tab(), AppTab::Projects);
        assert_eq!(app.selected_project_index(), 0);
        assert_eq!(app.projects().len(), 2);
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
    fn starting_input_modes_sets_input_state() {
        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![pane(10, 1, 1), pane(20, 2, 1)]]);
        let mut app =
            App::load_with_projects(&mut wezterm, test_projects()).expect("app should load");

        app.apply(AppAction::StartCreateWorktreeInput, &mut wezterm)
            .expect("worktree input should start");
        app.apply(AppAction::EditInput('x'), &mut wezterm)
            .expect("input should accept text");

        assert!(app.is_input_active());
        assert!(
            app.input_line()
                .contains("Create worktree branch for alpha: x")
        );

        app.apply(AppAction::CancelInput, &mut wezterm)
            .expect("input should cancel");
        app.apply(AppAction::StartCommandInput, &mut wezterm)
            .expect("command input should start");
        app.apply(AppAction::EditInput('r'), &mut wezterm)
            .expect("input should accept text");

        assert!(app.is_input_active());
        assert!(app.input_line().contains("Projects command for alpha: :r"));
    }

    #[test]
    fn remove_command_rejects_root_projects() {
        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![pane(10, 1, 1), pane(20, 2, 1)]]);
        let mut app =
            App::load_with_projects(&mut wezterm, test_projects()).expect("app should load");

        app.apply(AppAction::StartCommandInput, &mut wezterm)
            .expect("command input should start");
        for c in "remove".chars() {
            app.apply(AppAction::EditInput(c), &mut wezterm)
                .expect("input should accept text");
        }

        let error = app
            .apply(AppAction::ConfirmInput, &mut wezterm)
            .expect_err("root remove should fail");
        assert!(
            error
                .to_string()
                .contains("remove only works on linked worktrees")
        );
    }

    #[test]
    fn parses_projects_commands_with_optional_target() {
        assert_eq!(
            parse_project_command("merge").expect("merge should parse"),
            super::ProjectCommand::Merge { target: None }
        );
        assert_eq!(
            parse_project_command("merge main").expect("merge target should parse"),
            super::ProjectCommand::Merge {
                target: Some("main".to_string())
            }
        );
        assert_eq!(
            parse_project_command("pr main").expect("pr target should parse"),
            super::ProjectCommand::Pr {
                target: Some("main".to_string())
            }
        );
        assert_eq!(
            parse_project_command("land").expect("land should parse"),
            super::ProjectCommand::Land { target: None }
        );
        assert!(parse_project_command("remove main").is_err());
    }

    #[test]
    fn merge_command_fast_forwards_main_and_keeps_worktree_selected() {
        let fixture = create_worktree_fixture("merge-command");
        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![pane(10, 1, 1), pane(20, 2, 1)]]);
        let mut app = App::load_with_projects(&mut wezterm, fixture.projects.clone())
            .expect("app should load");
        app.apply(AppAction::ProjectMoveDown, &mut wezterm)
            .expect("selection should move to worktree");

        app.execute_project_command("merge")
            .expect("merge should succeed");

        assert_eq!(head_message(&fixture.root), "feature change");
        assert_eq!(
            app.projects()[app.selected_project_index()].cwd,
            root_as_str(&fixture.worktree)
        );
        assert!(app.status_line().contains("Merged feature into main"));
    }

    #[test]
    fn ctrl_w_preserves_branch_names_with_slashes_and_generates_wt_directory() {
        let sandbox = test_sandbox("create-worktree-raw-branch");
        let root = sandbox.join("repo");

        git(
            &sandbox,
            &["init", "--initial-branch=main", root_as_str(&root)],
        );
        write_file(&root.join("tracked.txt"), "hello\n");
        git_commit_all(&root, "init");

        let projects = super::discover_projects_in(root_as_str(&sandbox))
            .expect("projects should be discovered");

        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![pane(10, 1, 1), pane(20, 2, 1)]]);
        let mut app = App::load_with_projects(&mut wezterm, projects).expect("app should load");

        app.apply(AppAction::StartCreateWorktreeInput, &mut wezterm)
            .expect("worktree input should start");
        for c in "feature/BOOST-3432".chars() {
            app.apply(AppAction::EditInput(c), &mut wezterm)
                .expect("input should accept branch text");
        }
        app.apply(AppAction::ConfirmInput, &mut wezterm)
            .expect("worktree creation should succeed");

        let created = &app.projects()[app.selected_project_index()];
        assert_eq!(created.branch, "feature/BOOST-3432");
        assert!(
            created
                .cwd
                .starts_with(&format!("{}/wt-", sandbox.display()))
        );
        assert!(created.name.starts_with("wt-"));
    }

    #[test]
    fn land_command_merges_and_removes_worktree() {
        let fixture = create_worktree_fixture("land-command");
        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![pane(10, 1, 1), pane(20, 2, 1)]]);
        let mut app = App::load_with_projects(&mut wezterm, fixture.projects.clone())
            .expect("app should load");
        app.apply(AppAction::ProjectMoveDown, &mut wezterm)
            .expect("selection should move to worktree");

        app.execute_project_command("land")
            .expect("land should succeed");

        assert_eq!(head_message(&fixture.root), "feature change");
        assert!(!fixture.worktree.exists());
        assert!(!branch_exists(&fixture.root, "feature"));
        assert!(app.status_line().contains("Removed worktree"));
    }

    #[test]
    fn pr_command_pushes_branch_and_creates_pull_request() {
        let fixture = create_worktree_fixture("pr-command");
        git(
            &fixture.root,
            &["remote", "add", "origin", root_as_str(&fixture.remote)],
        );
        git(&fixture.root, &["push", "-u", "origin", "main"]);

        let fake_bin = test_sandbox("fake-gh-bin");
        let gh_path = fake_bin.join("gh");
        write_file(
            &gh_path,
            "#!/bin/sh
if [ \"$1\" = \"pr\" ] && [ \"$2\" = \"view\" ]; then
  exit 1
fi
if [ \"$1\" = \"pr\" ] && [ \"$2\" = \"create\" ]; then
  printf '%s\n' 'https://example.com/pr/123'
  exit 0
fi
printf '%s\n' \"unexpected gh invocation: $*\" >&2
exit 1
",
        );
        chmod_executable(&gh_path);
        let original_path = env::var("PATH").unwrap_or_default();
        let patched_path = format!("{}:{}", fake_bin.display(), original_path);
        unsafe {
            env::set_var("PATH", patched_path);
        }

        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![pane(10, 1, 1), pane(20, 2, 1)]]);
        let mut app = App::load_with_projects(&mut wezterm, fixture.projects.clone())
            .expect("app should load");
        app.apply(AppAction::ProjectMoveDown, &mut wezterm)
            .expect("selection should move to worktree");

        let result = app.execute_project_command("pr");

        unsafe {
            env::set_var("PATH", original_path);
        }
        result.expect("pr should succeed");

        assert!(remote_branch_exists(&fixture.remote, "feature"));
        assert!(
            app.status_line()
                .contains("PR ready for feature -> main: https://example.com/pr/123")
        );
    }

    #[test]
    fn switching_tabs_preserves_pane_mode() {
        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![pane(10, 1, 1), pane(20, 1, 1)]]);
        let mut app =
            App::load_with_projects(&mut wezterm, test_projects()).expect("app should load");

        app.apply(AppAction::SwitchToPanes, &mut wezterm)
            .expect("switch should succeed");
        app.apply(AppAction::AttachSelected, &mut wezterm)
            .expect("attach should succeed");
        app.apply(AppAction::SwitchToProjects, &mut wezterm)
            .expect("switch should succeed");
        app.apply(AppAction::SwitchToPanes, &mut wezterm)
            .expect("switch should succeed");

        assert_eq!(app.active_tab(), AppTab::Panes);
        assert_eq!(app.mode(), Mode::Insert);
    }

    #[test]
    fn project_shell_open_spawns_and_refocuses_tui() {
        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![pane(10, 1, 1), pane(20, 2, 1)]]);
        let mut app =
            App::load_with_projects(&mut wezterm, test_projects()).expect("app should load");

        app.apply(AppAction::OpenProjectShell, &mut wezterm)
            .expect("open should succeed");

        assert_eq!(
            wezterm.calls,
            vec![
                Call::ListPanes,
                Call::SpawnNewTab {
                    pane_id: 10,
                    cwd: "/tmp/repos/alpha".to_string(),
                    command: NewTabCommand::Shell,
                },
                Call::ActivatePane(10),
            ]
        );
    }

    #[test]
    fn project_actions_use_selected_project_cwd() {
        set_wezterm_pane();
        let mut wezterm = FakeWezterm::new(vec![vec![pane(10, 1, 1), pane(20, 2, 1)]]);
        let mut app =
            App::load_with_projects(&mut wezterm, test_projects()).expect("app should load");
        app.apply(AppAction::ProjectMoveDown, &mut wezterm)
            .expect("move should succeed");

        app.apply(AppAction::OpenProjectEditor, &mut wezterm)
            .expect("open should succeed");
        app.apply(AppAction::OpenProjectGit, &mut wezterm)
            .expect("open should succeed");

        assert_eq!(
            wezterm.calls,
            vec![
                Call::ListPanes,
                Call::SpawnNewTab {
                    pane_id: 10,
                    cwd: "/tmp/repos/beta".to_string(),
                    command: NewTabCommand::Nvim,
                },
                Call::ActivatePane(10),
                Call::SpawnNewTab {
                    pane_id: 10,
                    cwd: "/tmp/repos/beta".to_string(),
                    command: NewTabCommand::Lazygit,
                },
                Call::ActivatePane(10),
            ]
        );
    }

    #[test]
    fn builds_project_tree_with_root_first_and_worktrees_nested() {
        let projects = build_project_entries(vec![
            GitProjectProbe {
                name: "nerve_center.codex-hooks".to_string(),
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
                    name: "nerve_center.codex-hooks".to_string(),
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
        assert!(
            remove_error
                .to_string()
                .contains("git worktree remove failed")
        );

        run_git_branch_delete(root_as_str(&root), "review")
            .expect_err("branch should still be checked out in dirty worktree");
    }

    #[derive(Debug)]
    struct WorktreeFixture {
        root: PathBuf,
        worktree: PathBuf,
        remote: PathBuf,
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

        let projects = super::discover_projects_in(root_as_str(&sandbox))
            .expect("projects should be discovered");
        WorktreeFixture {
            root,
            worktree,
            remote,
            projects,
        }
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

    fn head_message(workdir: &Path) -> String {
        let output = Command::new("git")
            .args(["log", "-1", "--pretty=%s"])
            .current_dir(workdir)
            .output()
            .expect("git log should start");
        assert!(output.status.success(), "git log should succeed");
        String::from_utf8(output.stdout)
            .expect("git log output should be utf-8")
            .trim()
            .to_string()
    }

    fn branch_exists(workdir: &Path, branch: &str) -> bool {
        Command::new("git")
            .args([
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ])
            .current_dir(workdir)
            .status()
            .expect("git show-ref should start")
            .success()
    }

    fn remote_branch_exists(remote_repo: &Path, branch: &str) -> bool {
        Command::new("git")
            .args([
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ])
            .current_dir(remote_repo)
            .status()
            .expect("git show-ref should start")
            .success()
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
