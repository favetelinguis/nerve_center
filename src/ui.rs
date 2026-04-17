use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use ratatui::{symbols, Frame};

use crate::app::App;

pub fn render(frame: &mut Frame, app: &App) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(3),
            Constraint::Length(3),
        ])
        .split(frame.area());

    render_projects(frame, app, layout[0]);
    render_input(frame, app, layout[1]);

    let footer =
        Paragraph::new(Line::from(app.status_line())).block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer, layout[2]);
}

fn render_input(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let title = if app.is_search_active() {
        "Search [ACTIVE]"
    } else if app.is_input_active() {
        "Input [ACTIVE]"
    } else {
        "Input"
    };
    let border_style = if app.is_search_active() {
        Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else if app.is_input_active() {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let input = Paragraph::new(Line::from(app.input_line())).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(title),
    );
    frame.render_widget(input, area);
}

fn render_projects(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let rows = if app.projects().is_empty() {
        vec![ListItem::new("No projects found")]
    } else {
        let labels = app
            .projects()
            .iter()
            .map(|project| project.list_label())
            .collect::<Vec<_>>();
        let label_width = labels.iter().map(|label| label.len()).max().unwrap_or(0);
        let branch_width = app
            .projects()
            .iter()
            .map(|project| project.branch.len())
            .max()
            .unwrap_or(0);
        let status_width = app
            .projects()
            .iter()
            .map(|project| project.status_summary.display_text().len())
            .max()
            .unwrap_or(0);
        let agents = app
            .projects()
            .iter()
            .enumerate()
            .map(|(index, _)| {
                let monitors = app.project_agent_monitors(index);
                if monitors.is_empty() {
                    "-".to_string()
                } else {
                    monitors
                        .iter()
                        .map(|monitor| monitor.display_text())
                        .collect::<Vec<_>>()
                        .join(" ")
                }
            })
            .collect::<Vec<_>>();

        app.projects()
            .iter()
            .zip(labels)
            .zip(agents)
            .map(|((project, label), agents)| {
                let text = format!(
                    "{label:<label_width$}  {branch:<branch_width$}  {status:<status_width$}  {agents}",
                    branch = project.branch,
                    status = project.status_summary.display_text(),
                    agents = agents,
                    label_width = label_width,
                    branch_width = branch_width,
                    status_width = status_width,
                );
                ListItem::new(text)
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
