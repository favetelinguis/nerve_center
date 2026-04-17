use std::io::{self, Stdout, Write};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use crossterm::event::{self, Event};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use url::Url;

use crate::app::App;
use crate::input::{AppAction, action_for_key};
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
                let open_editor = action == AppAction::OpenProjectEditor;
                if let Err(error) = app.apply(action, wezterm) {
                    app.record_error(error.to_string());
                    continue;
                }

                if open_editor {
                    if let Err(error) = open_selected_project_editor(terminal, app, wezterm) {
                        app.record_error(error.to_string());
                        continue;
                    }
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

fn reinit_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    *terminal = init_terminal()?;
    Ok(())
}

fn open_selected_project_editor<W: WeztermClient>(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    wezterm: &mut W,
) -> Result<()> {
    let Some(cwd) = app.selected_project_cwd().map(str::to_string) else {
        app.record_error("No projects found");
        return Ok(());
    };
    let project_name = app.selected_project_name().unwrap_or("-").to_string();

    restore_terminal(terminal)?;
    let editor_result = run_blocking_command("nvim", &[], &cwd)
        .with_context(|| format!("failed to open nvim for {project_name}"));
    let reinit_result = reinit_terminal(terminal);

    let mut post_resume_result = Ok(());
    if reinit_result.is_ok() {
        if let Err(error) = emit_selected_project_cwd(terminal, app) {
            post_resume_result = Err(error);
        } else if let Err(error) = app.tick(wezterm) {
            post_resume_result = Err(error);
        }
    }

    reinit_result?;
    post_resume_result?;
    editor_result
}

fn run_blocking_command(program: &str, args: &[&str], cwd: &str) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .status()
        .with_context(|| format!("failed to spawn {program}"))?;

    if status.success() {
        Ok(())
    } else {
        bail!("{program} exited with status {status}")
    }
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
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::run_blocking_command;
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

    #[test]
    fn blocking_command_runs_in_requested_cwd() {
        let root = test_sandbox("blocking-command-cwd");
        let cwd = root.join("project");
        let log_path = root.join("pwd.log");
        fs::create_dir_all(&cwd).expect("cwd should be created");
        let shell_command = format!("pwd > '{}'", log_path.display());

        run_blocking_command("sh", &["-c", shell_command.as_str()], cwd.to_str().unwrap())
            .expect("command should succeed");

        assert_eq!(
            fs::read_to_string(log_path)
                .expect("log should exist")
                .trim(),
            cwd.to_str().unwrap()
        );
    }

    fn test_sandbox(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        let path = env::temp_dir().join(format!("nerve-center-{label}-{unique}"));
        fs::create_dir_all(&path).expect("sandbox should be created");
        path
    }
}
