use anyhow::{Result, anyhow};

use crate::input::AppAction;
use crate::wezterm::{
    PaneInfo, SplitDirection, TuiTabLayout, WeztermClient, find_pane, listable_panes,
    tui_pane_id_from_env, tui_tab_layout,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Normal,
    Insert,
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
    mode: Mode,
    tui_pane_id: u64,
    attached_pane_id: Option<u64>,
    status_message: String,
    last_error: Option<String>,
    should_quit: bool,
}

impl App {
    pub fn load<W: WeztermClient>(wezterm: &mut W) -> Result<Self> {
        let tui_pane_id = tui_pane_id_from_env()?;
        let panes = wezterm.list_panes()?;

        let mut app = Self {
            rows: Vec::new(),
            selected_index: 0,
            mode: Mode::Normal,
            tui_pane_id,
            attached_pane_id: None,
            status_message: String::new(),
            last_error: None,
            should_quit: false,
        };
        app.replace_rows(panes)?;
        app.set_status(format!("Loaded {} panes", app.rows.len()));
        Ok(app)
    }

    pub fn mode(&self) -> Mode {
        self.mode
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

        format!("mode={mode} selected={selected} attached={attached} {message}")
    }

    pub fn record_error(&mut self, error: impl Into<String>) {
        self.last_error = Some(error.into());
    }

    pub fn apply<W: WeztermClient>(&mut self, action: AppAction, wezterm: &mut W) -> Result<()> {
        match action {
            AppAction::MoveUp => {
                self.move_up();
                Ok(())
            }
            AppAction::MoveDown => {
                self.move_down();
                Ok(())
            }
            AppAction::AttachSelected => self.attach_selected(wezterm),
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

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use anyhow::Result;

    use super::{App, Mode};
    use crate::input::AppAction;
    use crate::wezterm::{PaneInfo, SplitDirection, WeztermClient};

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
        let mut app = App::load(&mut wezterm).expect("app should load");

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
        let mut app = App::load(&mut wezterm).expect("app should load");
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
        let mut app = App::load(&mut wezterm).expect("app should load");

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
        let mut app = App::load(&mut wezterm).expect("app should load");

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
        let mut app = App::load(&mut wezterm).expect("app should load");
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
}
