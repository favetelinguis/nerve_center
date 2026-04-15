use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Tabs};
use ratatui::{Frame, symbols};

use crate::app::{App, AppTab, Mode};

pub fn render(frame: &mut Frame, app: &App) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(3),
            Constraint::Length(3),
        ])
        .split(frame.area());

    let tabs = Tabs::new(vec!["1 Projects", "2 Panes"])
        .block(Block::default().borders(Borders::ALL).title("Views"))
        .highlight_style(Style::default().add_modifier(Modifier::BOLD))
        .select(match app.active_tab() {
            AppTab::Projects => 0,
            AppTab::Panes => 1,
        });
    frame.render_widget(tabs, layout[0]);

    match app.active_tab() {
        AppTab::Projects => render_projects(frame, app, layout[1]),
        AppTab::Panes => render_panes(frame, app, layout[1]),
    }

    render_input(frame, app, layout[2]);

    let footer =
        Paragraph::new(Line::from(app.status_line())).block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer, layout[3]);
}

fn render_input(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let title = if app.is_input_active() {
        "Input [ACTIVE]"
    } else {
        "Input"
    };
    let input = Paragraph::new(Line::from(app.input_line()))
        .block(Block::default().borders(Borders::ALL).title(title));
    frame.render_widget(input, area);
}

fn render_projects(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let rows = if app.projects().is_empty() {
        vec![ListItem::new("No projects found")]
    } else {
        let labels = app
            .projects()
            .iter()
            .map(|project| project.tree_label())
            .collect::<Vec<_>>();
        let label_width = labels.iter().map(|label| label.len()).max().unwrap_or(0);

        app.projects()
            .iter()
            .zip(labels)
            .map(|(project, label)| {
                ListItem::new(format!(
                    "{label:<label_width$}  {}",
                    project.branch,
                    label_width = label_width
                ))
            })
            .collect::<Vec<_>>()
    };

    let list = List::new(rows)
        .block(Block::default().borders(Borders::ALL).title("Projects"))
        .highlight_style(Style::default().add_modifier(Modifier::BOLD))
        .highlight_symbol(symbols::DOT);

    let mut list_state = ratatui::widgets::ListState::default();
    if !app.projects().is_empty() {
        list_state.select(Some(app.selected_project_index()));
    }
    frame.render_stateful_widget(list, area, &mut list_state);
}

fn render_panes(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
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
    frame.render_stateful_widget(list, area, &mut list_state);
}
