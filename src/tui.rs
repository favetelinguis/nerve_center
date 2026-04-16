use std::io::{self, Stdout, Write};
use std::time::Duration;

use anyhow::{anyhow, Result};
use crossterm::event::{self, Event};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use url::Url;

use crate::app::App;
use crate::input::action_for_key;
use crate::ui;
use crate::wezterm::WeztermClient;

pub fn run<W: WeztermClient>(wezterm: &mut W) -> Result<()> {
    let mut app = App::load(wezterm)?;
    let mut terminal = init_terminal()?;
    emit_selected_project_cwd(&mut terminal, &app)?;
    let run_result = run_loop(&mut terminal, &mut app, wezterm);
    let restore_result = restore_terminal(&mut terminal);

    run_result?;
    restore_result?;
    Ok(())
}

fn run_loop<W: WeztermClient>(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    wezterm: &mut W,
) -> Result<()> {
    while !app.should_quit() {
        terminal.draw(|frame| ui::render(frame, app))?;

        if !event::poll(Duration::from_millis(250))? {
            if let Err(error) = app.tick(wezterm) {
                app.record_error(error.to_string());
            }
            continue;
        }

        if let Event::Key(key) = event::read()? {
            if let Some(action) = action_for_key(app.is_input_active(), app.is_forwarding(), key) {
                let selected_cwd_before = app.selected_project_cwd().map(str::to_string);
                if let Err(error) = app.apply(action, wezterm) {
                    app.record_error(error.to_string());
                    continue;
                }

                if selected_cwd_before.as_deref() != app.selected_project_cwd() {
                    if let Err(error) = emit_selected_project_cwd(terminal, app) {
                        app.record_error(error.to_string());
                    }
                }
            }
        }
    }

    Ok(())
}

fn init_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn emit_selected_project_cwd(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &App,
) -> Result<()> {
    let Some(cwd) = app.selected_project_cwd() else {
        return Ok(());
    };

    emit_wezterm_cwd(terminal.backend_mut(), cwd)
}

fn emit_wezterm_cwd<W: Write>(writer: &mut W, cwd: &str) -> Result<()> {
    let sequence = wezterm_cwd_sequence(cwd)?;
    writer.write_all(sequence.as_bytes())?;
    writer.flush()?;
    Ok(())
}

fn wezterm_cwd_sequence(cwd: &str) -> Result<String> {
    let url = Url::from_file_path(cwd)
        .map_err(|_| anyhow!("failed to convert cwd into file URL: {cwd}"))?;
    Ok(format!("\u{1b}]7;{url}\u{7}"))
}

#[cfg(test)]
mod tests {
    use super::wezterm_cwd_sequence;

    #[test]
    fn encodes_wezterm_cwd_sequence_as_file_url() {
        let sequence = wezterm_cwd_sequence("/tmp/repos/space repo/#hash")
            .expect("cwd sequence should be encoded");

        assert_eq!(
            sequence,
            "\u{1b}]7;file:///tmp/repos/space%20repo/%23hash\u{7}"
        );
    }
}
