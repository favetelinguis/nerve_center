use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::app::Mode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppAction {
    MoveUp,
    MoveDown,
    AttachSelected,
    Quit,
    ExitInsert,
    Forward(String),
}

pub fn action_for_key(mode: Mode, key: KeyEvent) -> Option<AppAction> {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }

    match mode {
        Mode::Normal => normal_mode_action(key),
        Mode::Insert => insert_mode_action(key),
    }
}

fn normal_mode_action(key: KeyEvent) -> Option<AppAction> {
    if key.modifiers != KeyModifiers::NONE {
        return None;
    }

    match key.code {
        KeyCode::Char('j') => Some(AppAction::MoveDown),
        KeyCode::Char('k') => Some(AppAction::MoveUp),
        KeyCode::Char('i') => Some(AppAction::AttachSelected),
        KeyCode::Char('q') => Some(AppAction::Quit),
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
    use crate::app::Mode;

    #[test]
    fn normal_mode_maps_vim_keys() {
        let key = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(action_for_key(Mode::Normal, key), Some(AppAction::MoveDown));
    }

    #[test]
    fn insert_mode_maps_escape_locally() {
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(
            action_for_key(Mode::Insert, key),
            Some(AppAction::ExitInsert)
        );
    }

    #[test]
    fn insert_mode_forwards_q_as_text() {
        let key = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        assert_eq!(
            action_for_key(Mode::Insert, key),
            Some(AppAction::Forward("q".to_string()))
        );
    }

    #[test]
    fn insert_mode_ignores_ctrl_chords() {
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(action_for_key(Mode::Insert, key), None);
    }
}
