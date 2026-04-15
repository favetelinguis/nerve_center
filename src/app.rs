use std::collections::BTreeSet;
use std::env;
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
pub struct ProjectEntry {
    pub name: String,
    pub cwd: String,
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

#[derive(Debug)]
pub struct App {
    rows: Vec<PaneRow>,
    selected_index: usize,
    projects: Vec<ProjectEntry>,
    selected_project_index: usize,
    active_tab: AppTab,
    mode: Mode,
    tui_pane_id: u64,
    attached_pane_id: Option<u64>,
    status_message: String,
    last_error: Option<String>,
    should_quit: bool,
}

impl App {
    pub fn load<W: WeztermClient>(wezterm: &mut W) -> Result<Self> {
        let projects = discover_projects()?;
        Self::load_with_projects(wezterm, projects)
    }

    fn load_with_projects<W: WeztermClient>(
        wezterm: &mut W,
        projects: Vec<ProjectEntry>,
    ) -> Result<Self> {
        let tui_pane_id = tui_pane_id_from_env()?;
        let panes = wezterm.list_panes()?;

        let mut app = Self {
            rows: Vec::new(),
            selected_index: 0,
            projects,
            selected_project_index: 0,
            active_tab: AppTab::Projects,
            mode: Mode::Normal,
            tui_pane_id,
            attached_pane_id: None,
            status_message: String::new(),
            last_error: None,
            should_quit: false,
        };
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
        let mode = match self.mode {
            Mode::Normal => "NORMAL",
            Mode::Insert => "INSERT",
        };
        let selected = self
            .selected_row()
            .map(|row| row.pane.pane_id.to_string())
            .unwrap_or_else(|| "-".to_string());
        let attached = self
            .attached_pane_id
            .map(|pane_id| pane_id.to_string())
            .unwrap_or_else(|| "-".to_string());
        let message = self
            .last_error
            .as_deref()
            .unwrap_or(self.status_message.as_str());
        let tab = match self.active_tab {
            AppTab::Projects => "projects",
            AppTab::Panes => "panes",
        };
        let project = self
            .selected_project()
            .map(|item| item.name.as_str())
            .unwrap_or("-");

        format!(
            "tab={tab} mode={mode} selected={selected} attached={attached} project={project} {message}"
        )
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

    fn set_status(&mut self, status: impl Into<String>) {
        self.status_message = status.into();
        self.last_error = None;
    }
}

fn discover_projects() -> Result<Vec<ProjectEntry>> {
    let home = env::var("HOME").context("HOME is not set")?;
    let repos_root = format!("{home}/repos");
    let output = Command::new("fd")
        .args(["-HI", "^.git$", "--max-depth", "4", "--prune", &repos_root])
        .output()
        .context("failed to spawn fd for project discovery")?;

    if !output.status.success() {
        bail!(
            "fd project discovery failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let stdout = String::from_utf8(output.stdout)
        .context("fd project discovery stdout was not valid UTF-8")?;
    Ok(parse_projects(&stdout, &home))
}

fn parse_projects(stdout: &str, home: &str) -> Vec<ProjectEntry> {
    let repos_prefix = format!("{home}/repos/");
    let mut project_names = BTreeSet::new();

    for line in stdout.lines() {
        let project = line.trim().trim_end_matches("/.git");
        if project.is_empty() {
            continue;
        }

        let relative = project.strip_prefix(&repos_prefix).unwrap_or(project);
        let Some(name) = relative.split('/').next() else {
            continue;
        };
        if name.is_empty() {
            continue;
        }

        project_names.insert(name.to_string());
    }

    project_names
        .into_iter()
        .map(|name| ProjectEntry {
            cwd: format!("{repos_prefix}{name}"),
            name,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use anyhow::Result;

    use super::{App, AppTab, Mode, ProjectEntry, parse_projects};
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
            },
            ProjectEntry {
                name: "beta".to_string(),
                cwd: "/tmp/repos/beta".to_string(),
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
    fn parses_top_level_project_names_from_fd_output() {
        let output = concat!(
            "/home/test/repos/zeph/.git\n",
            "/home/test/repos/codex/codex-cli/.git\n",
            "/home/test/repos/codex/codex-rs/.git\n",
            "/home/test/repos/hello/.git\n"
        );

        assert_eq!(
            parse_projects(output, "/home/test"),
            vec![
                ProjectEntry {
                    name: "codex".to_string(),
                    cwd: "/home/test/repos/codex".to_string(),
                },
                ProjectEntry {
                    name: "hello".to_string(),
                    cwd: "/home/test/repos/hello".to_string(),
                },
                ProjectEntry {
                    name: "zeph".to_string(),
                    cwd: "/home/test/repos/zeph".to_string(),
                },
            ]
        );
    }
}
