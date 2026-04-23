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
            Constraint::Length(7),
            Constraint::Length(3),
        ])
        .split(frame.area());

    render_projects(frame, app, layout[0]);
    render_input(frame, app, layout[1]);
    render_completions(frame, app, layout[2]);

    let footer =
        Paragraph::new(Line::from(app.status_line())).block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer, layout[3]);
}

fn render_input(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let follow_indicator = if app.is_follow_mode() {
        format!("Follow [ON q={}] | ", app.follow_queue_len())
    } else {
        String::new()
    };
    let title = if app.is_search_active() {
        format!("{follow_indicator}Search [ACTIVE]")
    } else if app.is_command_active() {
        format!(
            "{follow_indicator}Command [ACTIVE] {}",
            app.selected_project_name().unwrap_or("-")
        )
    } else if app.is_forwarding() {
        format!("{follow_indicator}Forwarding [ACTIVE]")
    } else if app.is_input_active() {
        format!("{follow_indicator}Input [ACTIVE]")
    } else {
        format!("{follow_indicator}Input")
    };
    let border_style = if app.is_search_active() {
        Style::default().add_modifier(Modifier::BOLD)
    } else if app.is_follow_mode() && app.is_forwarding() {
        Style::default().add_modifier(Modifier::BOLD)
    } else if app.is_command_active() {
        Style::default().add_modifier(Modifier::BOLD)
    } else if app.is_follow_mode() {
        Style::default().add_modifier(Modifier::BOLD)
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

fn render_completions(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let rows = if app.is_command_active() && !app.command_completions().is_empty() {
        app.command_completions()
            .iter()
            .map(|completion| ListItem::new(completion.label.clone()))
            .collect::<Vec<_>>()
    } else if app.is_command_active() {
        vec![ListItem::new("No command completions")]
    } else {
        vec![ListItem::new("")]
    };

    let border_style = if app.is_command_active() {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let list = List::new(rows)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title("Completions"),
        )
        .highlight_style(Style::default().add_modifier(Modifier::BOLD))
        .highlight_symbol(symbols::DOT);

    let mut list_state = ratatui::widgets::ListState::default();
    if let Some(selected) = app.selected_command_completion_index() {
        list_state.select(Some(selected));
    }
    frame.render_stateful_widget(list, area, &mut list_state);
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
        let statuses = app
            .projects()
            .iter()
            .enumerate()
            .map(|(index, project)| {
                project_status_text(
                    &project.status_summary.display_text(),
                    app.project_operation_text(index).as_deref(),
                    app.project_stale_reason(index),
                )
            })
            .collect::<Vec<_>>();
        let status_width = statuses.iter().map(String::len).max().unwrap_or(0);
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
            .enumerate()
            .map(|(index, project)| {
                let label = &labels[index];
                let status = &statuses[index];
                let agents = &agents[index];
                let text = format!(
                    "{label:<label_width$}  {branch:<branch_width$}  {status:<status_width$}  {agents}",
                    branch = project.branch,
                    status = status,
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

fn project_status_text(
    status: &str,
    operation: Option<&str>,
    stale_reason: Option<&str>,
) -> String {
    let mut parts = vec![status.to_string()];
    if let Some(operation) = operation {
        parts.push(operation.to_string());
    }
    if let Some(reason) = stale_reason {
        parts.push(format!("stale:{reason}"));
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::project_status_text;

    #[test]
    fn project_status_text_appends_stale_reason_when_present() {
        assert_eq!(
            project_status_text("clean", None, Some("refresh_failed")),
            "clean stale:refresh_failed"
        );
    }

    #[test]
    fn project_status_text_includes_operation_state_when_present() {
        assert_eq!(
            project_status_text("clean", Some("op:git_pull[running]"), None),
            "clean op:git_pull[running]"
        );
    }
}
