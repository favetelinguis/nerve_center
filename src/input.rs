use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppAction {
    ProjectMoveUp,
    ProjectMoveDown,
    StartCommandInput,
    StartSearchInput,
    ConfirmInput,
    CancelInput,
    EditInput(char),
    DeleteInputChar,
    NextSearchMatch,
    PreviousSearchMatch,
    NextCommandCompletion,
    PreviousCommandCompletion,
    AcceptCommandCompletion,
    AttachProjectAgent,
    ToggleFollowMode,
    OpenProjectIdea,
    OpenProjectTerminal,
    OpenProjectEditor,
    SelectPreviousProjectAgent,
    SelectNextProjectAgent,
    ExitForwarding,
    Forward(String),
    Quit,
}

pub fn action_for_key(
    input_active: bool,
    search_active: bool,
    forwarding_active: bool,
    key: KeyEvent,
) -> Option<AppAction> {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }

    if input_active {
        return if search_active {
            search_input_action(key)
        } else {
            input_action(key)
        };
    }

    if forwarding_active {
        return forwarding_action(key);
    }

    if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('f') {
        return Some(AppAction::ToggleFollowMode);
    }

    if key.modifiers != KeyModifiers::NONE {
        return None;
    }

    match key.code {
        KeyCode::Char(':') => Some(AppAction::StartCommandInput),
        KeyCode::Char('/') => Some(AppAction::StartSearchInput),
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
        KeyCode::Char('f') if key.modifiers == KeyModifiers::CONTROL => {
            Some(AppAction::ToggleFollowMode)
        }
        KeyCode::Esc => Some(AppAction::ExitForwarding),
        KeyCode::Left => Some(AppAction::SelectPreviousProjectAgent),
        KeyCode::Right => Some(AppAction::SelectNextProjectAgent),
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
        KeyCode::Tab => Some(AppAction::AcceptCommandCompletion),
        KeyCode::Backspace => Some(AppAction::DeleteInputChar),
        KeyCode::Char('n') if key.modifiers == KeyModifiers::CONTROL => {
            Some(AppAction::NextCommandCompletion)
        }
        KeyCode::Char('p') if key.modifiers == KeyModifiers::CONTROL => {
            Some(AppAction::PreviousCommandCompletion)
        }
        KeyCode::Char(c) if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT => {
            Some(AppAction::EditInput(c))
        }
        _ => None,
    }
}

fn search_input_action(key: KeyEvent) -> Option<AppAction> {
    match key.code {
        KeyCode::Esc => Some(AppAction::CancelInput),
        KeyCode::Enter => Some(AppAction::ConfirmInput),
        KeyCode::Backspace => Some(AppAction::DeleteInputChar),
        KeyCode::Char('n') if key.modifiers == KeyModifiers::CONTROL => {
            Some(AppAction::NextSearchMatch)
        }
        KeyCode::Char('p') if key.modifiers == KeyModifiers::CONTROL => {
            Some(AppAction::PreviousSearchMatch)
        }
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
                false,
                KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE)
            ),
            Some(AppAction::StartCommandInput)
        );
        assert_eq!(
            action_for_key(
                false,
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
                false,
                KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE)
            ),
            Some(AppAction::ProjectMoveUp)
        );
        assert_eq!(
            action_for_key(
                false,
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
                false,
                KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE)
            ),
            Some(AppAction::OpenProjectIdea)
        );
        assert_eq!(
            action_for_key(
                false,
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
                false,
                KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE)
            ),
            Some(AppAction::OpenProjectEditor)
        );
        assert_eq!(
            action_for_key(
                false,
                false,
                false,
                KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL)
            ),
            Some(AppAction::ToggleFollowMode)
        );
        assert_eq!(
            action_for_key(
                false,
                false,
                false,
                KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL)
            ),
            None
        );
        assert_eq!(
            action_for_key(
                false,
                false,
                false,
                KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)
            ),
            Some(AppAction::StartSearchInput)
        );
    }

    #[test]
    fn input_mode_maps_editing_keys() {
        assert_eq!(
            action_for_key(
                true,
                false,
                false,
                KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)
            ),
            Some(AppAction::EditInput('x'))
        );
        assert_eq!(
            action_for_key(
                true,
                false,
                false,
                KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)
            ),
            Some(AppAction::DeleteInputChar)
        );
        assert_eq!(
            action_for_key(
                true,
                false,
                false,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            ),
            Some(AppAction::ConfirmInput)
        );
        assert_eq!(
            action_for_key(
                true,
                false,
                false,
                KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)
            ),
            Some(AppAction::AcceptCommandCompletion)
        );
        assert_eq!(
            action_for_key(
                true,
                false,
                false,
                KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL)
            ),
            Some(AppAction::NextCommandCompletion)
        );
        assert_eq!(
            action_for_key(
                true,
                false,
                false,
                KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)
            ),
            Some(AppAction::PreviousCommandCompletion)
        );
    }

    #[test]
    fn search_mode_maps_search_navigation_keys() {
        assert_eq!(
            action_for_key(
                true,
                true,
                false,
                KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL)
            ),
            Some(AppAction::NextSearchMatch)
        );
        assert_eq!(
            action_for_key(
                true,
                true,
                false,
                KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)
            ),
            Some(AppAction::PreviousSearchMatch)
        );
        assert_eq!(
            action_for_key(
                true,
                true,
                false,
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
            ),
            Some(AppAction::CancelInput)
        );
    }

    #[test]
    fn forwarding_mode_maps_to_forward_and_escape() {
        assert_eq!(
            action_for_key(
                false,
                false,
                true,
                KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)
            ),
            Some(AppAction::Forward("i".to_string()))
        );
        assert_eq!(
            action_for_key(
                false,
                false,
                true,
                KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)
            ),
            Some(AppAction::SelectPreviousProjectAgent)
        );
        assert_eq!(
            action_for_key(
                false,
                false,
                true,
                KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)
            ),
            Some(AppAction::SelectNextProjectAgent)
        );
        assert_eq!(
            action_for_key(
                false,
                false,
                true,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            ),
            Some(AppAction::Forward("\r".to_string()))
        );
        assert_eq!(
            action_for_key(
                false,
                false,
                true,
                KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL)
            ),
            Some(AppAction::ToggleFollowMode)
        );
        assert_eq!(
            action_for_key(
                false,
                false,
                true,
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
            ),
            Some(AppAction::ExitForwarding)
        );
    }
}
