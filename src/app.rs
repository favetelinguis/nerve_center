use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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
    pub root_name: String,
    pub root_cwd: String,
    pub kind: ProjectKind,
}

impl ProjectEntry {
    pub fn tree_label(&self) -> String {
        match self.kind {
            ProjectKind::Root => self.name.clone(),
            ProjectKind::Worktree => format!("  |- {}", self.name),
        }
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
    WorktreeSlug { slug: String },
    Command { tab: AppTab, command: String },
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
            Some(InputMode::WorktreeSlug { slug }) => {
                let root_name = self
                    .selected_project()
                    .map(|project| project.root_name.as_str())
                    .unwrap_or("-");
                format!("Create worktree for {root_name}: {slug}")
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
                    "Ctrl-W create worktree | : command on selected project".to_string()
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

        self.input_mode = Some(InputMode::WorktreeSlug {
            slug: String::new(),
        });
        self.set_status(format!("Create worktree for {root_name}"));
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
            InputMode::WorktreeSlug { slug } => slug.push(c),
            InputMode::Command { command, .. } => command.push(c),
        }
        self.last_error = None;
    }

    fn delete_input_char(&mut self) {
        let Some(input_mode) = self.input_mode.as_mut() else {
            return;
        };

        match input_mode {
            InputMode::WorktreeSlug { slug } => {
                slug.pop();
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
            InputMode::WorktreeSlug { slug } => self.confirm_worktree_input(&slug),
            InputMode::Command { tab, command } => {
                self.input_mode = None;
                self.execute_command(tab, &command)
            }
        }
    }

    fn confirm_worktree_input(&mut self, slug: &str) -> Result<()> {
        let Some(project) = self.selected_project() else {
            self.record_error("No projects found");
            return Ok(());
        };

        let slug = sanitize_worktree_slug(slug);
        if slug.is_empty() {
            self.record_error("Worktree slug is empty");
            return Ok(());
        }

        let root_cwd = project.root_cwd.clone();
        let root_name = project.root_name.clone();
        let target_cwd = format!("{}/{}.{}", self.repos_root, root_name, slug);

        run_git_worktree_add(&root_cwd, &slug, &target_cwd)?;

        self.input_mode = None;
        self.reload_projects(Some(&target_cwd))?;
        self.set_status(format!("Created worktree {} on {}", root_name, slug));
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
        match command {
            "remove" => self.remove_selected_worktree(),
            _ => bail!("unknown projects command: {command}"),
        }
    }

    fn remove_selected_worktree(&mut self) -> Result<()> {
        let project = self
            .selected_project()
            .cloned()
            .ok_or_else(|| anyhow!("No projects found"))?;

        if project.kind != ProjectKind::Worktree {
            bail!("remove only works on linked worktrees");
        }
        if matches!(project.branch.as_str(), "DETACHED" | "N/A") {
            bail!("remove requires a branch-backed worktree");
        }

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
    root_name: String,
    root_cwd: String,
    common_dir: String,
    is_root: bool,
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

    Ok(Some(GitProjectProbe {
        name,
        cwd,
        branch: read_branch_name(path)?,
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

fn sanitize_worktree_slug(input: &str) -> String {
    let mut slug = String::new();
    let mut previous_dash = false;

    for c in input.trim().chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            slug.push(c);
            previous_dash = false;
            continue;
        }

        if !previous_dash {
            slug.push('-');
            previous_dash = true;
        }
    }

    slug.trim_matches('-').to_string()
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
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    use anyhow::Result;

    use super::{
        App, AppTab, GitProjectProbe, Mode, ProjectEntry, ProjectKind, build_project_entries,
        run_git_branch_delete, run_git_worktree_remove, sanitize_worktree_slug,
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
                root_name: "alpha".to_string(),
                root_cwd: "/tmp/repos/alpha".to_string(),
                kind: ProjectKind::Root,
            },
            ProjectEntry {
                name: "beta".to_string(),
                cwd: "/tmp/repos/beta".to_string(),
                branch: "feature".to_string(),
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
        assert!(app.input_line().contains("Create worktree for alpha: x"));

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
                root_name: "nerve_center".to_string(),
                root_cwd: "/home/test/repos/nerve_center".to_string(),
                common_dir: "/home/test/repos/nerve_center/.git".to_string(),
                is_root: false,
            },
            GitProjectProbe {
                name: "alpha".to_string(),
                cwd: "/home/test/repos/alpha".to_string(),
                branch: "main".to_string(),
                root_name: "alpha".to_string(),
                root_cwd: "/home/test/repos/alpha".to_string(),
                common_dir: "/home/test/repos/alpha/.git".to_string(),
                is_root: true,
            },
            GitProjectProbe {
                name: "nerve_center".to_string(),
                cwd: "/home/test/repos/nerve_center".to_string(),
                branch: "main".to_string(),
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
                    root_name: "alpha".to_string(),
                    root_cwd: "/home/test/repos/alpha".to_string(),
                    kind: ProjectKind::Root,
                },
                ProjectEntry {
                    name: "nerve_center".to_string(),
                    cwd: "/home/test/repos/nerve_center".to_string(),
                    branch: "main".to_string(),
                    root_name: "nerve_center".to_string(),
                    root_cwd: "/home/test/repos/nerve_center".to_string(),
                    kind: ProjectKind::Root,
                },
                ProjectEntry {
                    name: "nerve_center.codex-hooks".to_string(),
                    cwd: "/home/test/repos/nerve_center.codex-hooks".to_string(),
                    branch: "codex-hooks".to_string(),
                    root_name: "nerve_center".to_string(),
                    root_cwd: "/home/test/repos/nerve_center".to_string(),
                    kind: ProjectKind::Worktree,
                },
            ]
        );
        assert_eq!(projects[1].tree_label(), "nerve_center");
        assert_eq!(projects[2].tree_label(), "  |- nerve_center.codex-hooks");
    }

    #[test]
    fn sanitizes_worktree_slug_for_branch_and_directory_names() {
        assert_eq!(sanitize_worktree_slug(" Codex Hooks "), "codex-hooks");
        assert_eq!(sanitize_worktree_slug("review__2"), "review__2");
        assert_eq!(sanitize_worktree_slug("***"), "");
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

    fn write_file(path: &Path, content: &str) {
        fs::write(path, content).expect("file should be written");
    }

    fn root_as_str(path: &Path) -> &str {
        path.to_str().expect("path should be valid utf-8")
    }
}
