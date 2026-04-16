use std::io::{self, Stdout};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::app::App;
use crate::input::action_for_key;
use crate::ui;
use crate::wezterm::WeztermClient;

pub fn run<W: WeztermClient>(wezterm: &mut W) -> Result<()> {
    let mut app = App::load(wezterm)?;
    let mut terminal = init_terminal()?;
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
            if let Some(action) =
                action_for_key(app.active_tab(), app.mode(), app.is_input_active(), key)
            {
                if let Err(error) = app.apply(action, wezterm) {
                    app.record_error(error.to_string());
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
