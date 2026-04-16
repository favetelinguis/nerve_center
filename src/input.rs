use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::app::{AppTab, Mode};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppAction {
    SwitchToProjects,
    SwitchToPanes,
    MoveUp,
    MoveDown,
    ProjectMoveUp,
    ProjectMoveDown,
    StartCommandInput,
    ConfirmInput,
    CancelInput,
    EditInput(char),
    DeleteInputChar,
    AttachSelected,
    OpenProjectShell,
    OpenProjectEditor,
    OpenProjectGit,
    Quit,
    ExitInsert,
    Forward(String),
}

pub fn action_for_key(
    active_tab: AppTab,
    mode: Mode,
    input_active: bool,
    key: KeyEvent,
) -> Option<AppAction> {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }

    if input_active {
        return input_action(key);
    }

    match active_tab {
        AppTab::Projects => project_mode_action(key),
        AppTab::Panes => match mode {
            Mode::Normal => pane_normal_mode_action(key),
            Mode::Insert => insert_mode_action(key),
        },
    }
}

fn pane_normal_mode_action(key: KeyEvent) -> Option<AppAction> {
    if key.modifiers != KeyModifiers::NONE {
        return None;
    }

    match key.code {
        KeyCode::Char('1') => Some(AppAction::SwitchToProjects),
        KeyCode::Char('2') => Some(AppAction::SwitchToPanes),
        KeyCode::Char(':') => Some(AppAction::StartCommandInput),
        KeyCode::Char('j') => Some(AppAction::MoveDown),
        KeyCode::Char('k') => Some(AppAction::MoveUp),
        KeyCode::Char('i') => Some(AppAction::AttachSelected),
        KeyCode::Char('q') => Some(AppAction::Quit),
        _ => None,
    }
}

fn project_mode_action(key: KeyEvent) -> Option<AppAction> {
    match key.modifiers {
        KeyModifiers::NONE => match key.code {
            KeyCode::Char('1') => Some(AppAction::SwitchToProjects),
            KeyCode::Char('2') => Some(AppAction::SwitchToPanes),
            KeyCode::Char(':') => Some(AppAction::StartCommandInput),
            KeyCode::Char('j') => Some(AppAction::ProjectMoveDown),
            KeyCode::Char('k') => Some(AppAction::ProjectMoveUp),
            KeyCode::Char('q') => Some(AppAction::Quit),
            _ => None,
        },
        KeyModifiers::CONTROL => match key.code {
            KeyCode::Char('t') => Some(AppAction::OpenProjectShell),
            KeyCode::Char('e') => Some(AppAction::OpenProjectEditor),
            KeyCode::Char('v') => Some(AppAction::OpenProjectGit),
            _ => None,
        },
        _ => None,
    }
}

fn input_action(key: KeyEvent) -> Option<AppAction> {
    match key.code {
        KeyCode::Esc => Some(AppAction::CancelInput),
        KeyCode::Enter => Some(AppAction::ConfirmInput),
        KeyCode::Backspace => Some(AppAction::DeleteInputChar),
        KeyCode::Char(c) if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT => {
            Some(AppAction::EditInput(c))
        }
        _ => None,
    }
}

fn insert_mode_action(key: KeyEvent) -> Option<AppAction> {
    match key.code {
        KeyCode::Esc => Some(AppAction::ExitInsert),
        KeyCode::Enter => Some(AppAction::Forward("\n".to_string())),
        KeyCode::Tab => Some(AppAction::Forward("\t".to_string())),
        KeyCode::Backspace => Some(AppAction::Forward("\u{7f}".to_string())),
        KeyCode::Char(c) if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT => {
            Some(AppAction::Forward(c.to_string()))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::{AppAction, action_for_key};
    use crate::app::{AppTab, Mode};

    #[test]
    fn pane_normal_mode_maps_vim_keys() {
        let key = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(
            action_for_key(AppTab::Panes, Mode::Normal, false, key),
            Some(AppAction::MoveDown)
        );
        assert_eq!(
            action_for_key(
                AppTab::Panes,
                Mode::Normal,
                false,
                KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE)
            ),
            Some(AppAction::StartCommandInput)
        );
    }

    #[test]
    fn insert_mode_maps_escape_locally() {
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(
            action_for_key(AppTab::Panes, Mode::Insert, false, key),
            Some(AppAction::ExitInsert)
        );
    }

    #[test]
    fn insert_mode_forwards_q_as_text() {
        let key = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        assert_eq!(
            action_for_key(AppTab::Panes, Mode::Insert, false, key),
            Some(AppAction::Forward("q".to_string()))
        );
    }

    #[test]
    fn insert_mode_ignores_ctrl_chords() {
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(
            action_for_key(AppTab::Panes, Mode::Insert, false, key),
            None
        );
    }

    #[test]
    fn project_mode_maps_navigation_and_actions() {
        assert_eq!(
            action_for_key(
                AppTab::Projects,
                Mode::Insert,
                false,
                KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE)
            ),
            Some(AppAction::StartCommandInput)
        );
        assert_eq!(
            action_for_key(
                AppTab::Projects,
                Mode::Insert,
                false,
                KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)
            ),
            Some(AppAction::ProjectMoveDown)
        );
        assert_eq!(
            action_for_key(
                AppTab::Projects,
                Mode::Insert,
                false,
                KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE)
            ),
            Some(AppAction::SwitchToPanes)
        );
        assert_eq!(
            action_for_key(
                AppTab::Projects,
                Mode::Normal,
                false,
                KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL)
            ),
            Some(AppAction::OpenProjectShell)
        );
        assert_eq!(
            action_for_key(
                AppTab::Projects,
                Mode::Normal,
                false,
                KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL)
            ),
            Some(AppAction::OpenProjectEditor)
        );
        assert_eq!(
            action_for_key(
                AppTab::Projects,
                Mode::Normal,
                false,
                KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL)
            ),
            Some(AppAction::OpenProjectGit)
        );
        assert_eq!(
            action_for_key(
                AppTab::Projects,
                Mode::Normal,
                false,
                KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL)
            ),
            None
        );
    }

    #[test]
    fn input_mode_maps_editing_keys() {
        assert_eq!(
            action_for_key(
                AppTab::Projects,
                Mode::Normal,
                true,
                KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)
            ),
            Some(AppAction::EditInput('x'))
        );
        assert_eq!(
            action_for_key(
                AppTab::Projects,
                Mode::Normal,
                true,
                KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)
            ),
            Some(AppAction::DeleteInputChar)
        );
        assert_eq!(
            action_for_key(
                AppTab::Projects,
                Mode::Normal,
                true,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            ),
            Some(AppAction::ConfirmInput)
        );
    }
}
