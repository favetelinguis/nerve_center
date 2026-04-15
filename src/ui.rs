use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use ratatui::{Frame, symbols};

use crate::app::{App, Mode};

pub fn render(frame: &mut Frame, app: &App) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(3)])
        .split(frame.area());

    let rows = if app.rows().is_empty() {
        vec![ListItem::new("No selectable panes")]
    } else {
        app.rows()
            .iter()
            .map(|row| {
                let active_marker = if row.pane.is_active { "*" } else { " " };
                let attached_marker = if row.is_attached(app.attached_pane_id()) {
                    "@"
                } else {
                    " "
                };
                let text = format!(
                    "{attached_marker}{active_marker} pane={} window={} tab={} title={} cwd={}",
                    row.pane.pane_id,
                    row.pane.window_id,
                    row.pane.tab_id,
                    row.pane.title,
                    row.pane.cwd
                );
                ListItem::new(text)
            })
            .collect::<Vec<_>>()
    };

    let highlight = match app.mode() {
        Mode::Normal => "NORMAL",
        Mode::Insert => "INSERT",
    };

    let list = List::new(rows)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!("Nerve Center [{highlight}]")),
        )
        .highlight_style(Style::default().add_modifier(Modifier::BOLD))
        .highlight_symbol(symbols::DOT);

    let mut list_state = ratatui::widgets::ListState::default();
    if !app.rows().is_empty() {
        list_state.select(Some(app.selected_index()));
    }
    frame.render_stateful_widget(list, layout[0], &mut list_state);

    let footer =
        Paragraph::new(Line::from(app.status_line())).block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer, layout[1]);
}
