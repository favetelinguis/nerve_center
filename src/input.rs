use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppAction {
    ProjectMoveUp,
    ProjectMoveDown,
    StartCommandInput,
    ConfirmInput,
    CancelInput,
    EditInput(char),
    DeleteInputChar,
    AttachProjectAgent,
    OpenProjectIdea,
    OpenProjectTerminal,
    OpenProjectEditor,
    ExitForwarding,
    Forward(String),
    Quit,
}

pub fn action_for_key(
    input_active: bool,
    forwarding_active: bool,
    key: KeyEvent,
) -> Option<AppAction> {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }

    if input_active {
        return input_action(key);
    }

    if forwarding_active {
        return forwarding_action(key);
    }

    if key.modifiers != KeyModifiers::NONE {
        return None;
    }

    match key.code {
        KeyCode::Char(':') => Some(AppAction::StartCommandInput),
        KeyCode::Char('j') => Some(AppAction::ProjectMoveDown),
        KeyCode::Char('k') => Some(AppAction::ProjectMoveUp),
        KeyCode::Char('i') => Some(AppAction::AttachProjectAgent),
        KeyCode::Char('o') => Some(AppAction::OpenProjectIdea),
        KeyCode::Char('t') => Some(AppAction::OpenProjectTerminal),
        KeyCode::Char('e') => Some(AppAction::OpenProjectEditor),
        KeyCode::Char('q') => Some(AppAction::Quit),
        _ => None,
    }
}

fn forwarding_action(key: KeyEvent) -> Option<AppAction> {
    match key.code {
        KeyCode::Esc => Some(AppAction::ExitForwarding),
        KeyCode::Enter => Some(AppAction::Forward("\r".to_string())),
        KeyCode::Tab => Some(AppAction::Forward("\t".to_string())),
        KeyCode::Backspace => Some(AppAction::Forward("\u{7f}".to_string())),
        KeyCode::Char(c) if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT => {
            Some(AppAction::Forward(c.to_string()))
        }
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

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::{AppAction, action_for_key};

    #[test]
    fn project_mode_maps_navigation_and_actions() {
        assert_eq!(
            action_for_key(
                false,
                false,
                KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE)
            ),
            Some(AppAction::StartCommandInput)
        );
        assert_eq!(
            action_for_key(
                false,
                false,
                KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)
            ),
            Some(AppAction::ProjectMoveDown)
        );
        assert_eq!(
            action_for_key(
                false,
                false,
                KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE)
            ),
            Some(AppAction::ProjectMoveUp)
        );
        assert_eq!(
            action_for_key(
                false,
                false,
                KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)
            ),
            Some(AppAction::AttachProjectAgent)
        );
        assert_eq!(
            action_for_key(
                false,
                false,
                KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE)
            ),
            Some(AppAction::OpenProjectIdea)
        );
        assert_eq!(
            action_for_key(
                false,
                false,
                KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE)
            ),
            Some(AppAction::OpenProjectTerminal)
        );
        assert_eq!(
            action_for_key(
                false,
                false,
                KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE)
            ),
            Some(AppAction::OpenProjectEditor)
        );
        assert_eq!(
            action_for_key(
                false,
                false,
                KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL)
            ),
            None
        );
    }

    #[test]
    fn input_mode_maps_editing_keys() {
        assert_eq!(
            action_for_key(
                true,
                false,
                KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)
            ),
            Some(AppAction::EditInput('x'))
        );
        assert_eq!(
            action_for_key(
                true,
                false,
                KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)
            ),
            Some(AppAction::DeleteInputChar)
        );
        assert_eq!(
            action_for_key(
                true,
                false,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            ),
            Some(AppAction::ConfirmInput)
        );
    }

    #[test]
    fn forwarding_mode_maps_to_forward_and_escape() {
        assert_eq!(
            action_for_key(
                false,
                true,
                KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)
            ),
            Some(AppAction::Forward("i".to_string()))
        );
        assert_eq!(
            action_for_key(
                false,
                true,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            ),
            Some(AppAction::Forward("\r".to_string()))
        );
        assert_eq!(
            action_for_key(false, true, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(AppAction::ExitForwarding)
        );
    }
}
