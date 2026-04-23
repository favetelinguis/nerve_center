use std::io::{self, Stdout, Write};
use std::process::Command;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use crossterm::event::{self, Event};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use url::Url;

use crate::app::App;
use crate::input::{action_for_key, AppAction};
use crate::ui;
use crate::wezterm::WeztermClient;

pub fn run<W: WeztermClient>(wezterm: &mut W) -> Result<()> {
    let mut daemon = crate::client::Client::connect_or_spawn()?;
    let (snapshot, mut subscription) = crate::client::Client::connect_subscription_or_spawn()?;
    let mut app = App::from_snapshot(snapshot);
    app.tick(wezterm)?;
    let mut terminal = init_terminal()?;
    emit_selected_project_cwd(&mut terminal, &app)?;
    let run_result = run_loop(
        &mut terminal,
        &mut app,
        &mut daemon,
        &mut subscription,
        wezterm,
    );
    let restore_result = restore_terminal(&mut terminal);

    run_result?;
    restore_result?;
    Ok(())
}

fn run_loop<W: WeztermClient>(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    daemon: &mut crate::client::Client,
    subscription: &mut crate::client::Subscription,
    wezterm: &mut W,
) -> Result<()> {
    while !app.should_quit() {
        terminal.draw(|frame| ui::render(frame, app))?;

        if !event::poll(Duration::from_millis(250))? {
            if let Err(error) = apply_subscription_updates(app, subscription) {
                app.record_error(error.to_string());
            }
            if let Err(error) = app.tick(wezterm) {
                app.record_error(error.to_string());
            }
            continue;
        }

        if let Event::Key(key) = event::read()? {
            if let Some(action) = action_for_key(
                app.is_input_active(),
                app.is_search_active(),
                app.is_forwarding(),
                key,
            ) {
                let selected_cwd_before = app.selected_project_cwd().map(str::to_string);
                let open_editor = action == AppAction::OpenProjectEditor;
                if let Err(error) = app.apply(action, wezterm) {
                    app.record_error(error.to_string());
                    continue;
                }

                if let Err(error) = flush_daemon_requests(app, daemon) {
                    app.record_error(error.to_string());
                    continue;
                }

                if let Err(error) = apply_subscription_updates(app, subscription) {
                    app.record_error(error.to_string());
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

fn flush_daemon_requests(app: &mut App, daemon: &mut crate::client::Client) -> Result<()> {
    for message in app.drain_outbox() {
        if let Err(error) = daemon.send(message.clone()) {
            reconnect_daemon(app, daemon)?;
            daemon.send(message).with_context(|| {
                format!("failed to resend daemon request after reconnect: {error}")
            })?;
        }
    }
    Ok(())
}

fn apply_subscription_updates(
    app: &mut App,
    subscription: &mut crate::client::Subscription,
) -> Result<()> {
    match subscription.try_recv_latest() {
        Ok(Some(snapshot)) => app.replace_workspace(snapshot),
        Ok(None) => {}
        Err(_) => {
            let (snapshot, replacement) = crate::client::Client::connect_subscription_or_spawn()?;
            *subscription = replacement;
            app.replace_workspace(snapshot);
        }
    }
    Ok(())
}

fn reconnect_daemon(app: &mut App, daemon: &mut crate::client::Client) -> Result<()> {
    let snapshot = daemon.reconnect()?;
    app.replace_workspace(snapshot);
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

    use anyhow::Result;

    use crate::app::App;
    use crate::daemon::protocol::ClientMessage;
    use crate::input::AppAction;
    use crate::projects::{ProjectEntry, ProjectKind, ProjectStatusSummary};
    use crate::wezterm::{PaneInfo, SpawnCommand, SplitDirection, WeztermClient};
    use crate::workspace::{ProjectGitState, ProjectSnapshot, WorkspaceSnapshot};

    use super::apply_subscription_updates;
    use super::reconnect_daemon;
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

    #[derive(Default)]
    struct NoopWezterm;

    impl WeztermClient for NoopWezterm {
        fn list_panes(&mut self) -> Result<Vec<PaneInfo>> {
            Ok(Vec::new())
        }

        fn move_pane_to_new_tab(&mut self, _pane_id: u64) -> Result<()> {
            unreachable!("not used in this test")
        }

        fn split_pane(
            &mut self,
            _host_pane_id: u64,
            _move_pane_id: u64,
            _direction: SplitDirection,
        ) -> Result<()> {
            unreachable!("not used in this test")
        }

        fn activate_pane(&mut self, _pane_id: u64) -> Result<()> {
            unreachable!("not used in this test")
        }

        fn send_text(&mut self, _pane_id: u64, _text: &str) -> Result<()> {
            unreachable!("not used in this test")
        }

        fn spawn_new_tab(
            &mut self,
            _pane_id: u64,
            _cwd: &str,
            _command: &SpawnCommand,
        ) -> Result<u64> {
            unreachable!("not used in this test")
        }
    }

    #[test]
    fn flush_daemon_requests_sends_all_queued_messages() {
        let snapshot = test_snapshot(&[ProjectEntry {
            name: "alpha".to_string(),
            cwd: "/tmp/repos/alpha".to_string(),
            branch: "main".to_string(),
            status_summary: ProjectStatusSummary::default(),
            root_name: "alpha".to_string(),
            root_cwd: "/tmp/repos/alpha".to_string(),
            kind: ProjectKind::Root,
        }]);
        let mut app = App::from_snapshot(snapshot);
        let mut wezterm = NoopWezterm;

        app.apply(AppAction::StartCommandInput, &mut wezterm)
            .expect("command input should start");
        for c in "git pull".chars() {
            app.apply(AppAction::EditInput(c), &mut wezterm)
                .expect("input should accept text");
        }
        app.apply(AppAction::ConfirmInput, &mut wezterm)
            .expect("confirm should queue command");

        let mut client = crate::client::Client::stub();
        super::flush_daemon_requests(&mut app, &mut client).expect("queued messages should flush");

        assert_eq!(
            client.sent_messages(),
            &[ClientMessage::RunOperation {
                project_id: "/tmp/repos/alpha".to_string(),
                operation: "git_pull".to_string(),
                command: "git pull".to_string(),
            }]
        );
    }

    #[test]
    fn queued_daemon_operations_do_not_poll_for_workspace_state() {
        let mut app = App::from_snapshot(test_snapshot(&[ProjectEntry {
            name: "alpha".to_string(),
            cwd: "/tmp/repos/alpha".to_string(),
            branch: "main".to_string(),
            status_summary: ProjectStatusSummary {
                modified: 1,
                ..ProjectStatusSummary::default()
            },
            root_name: "alpha".to_string(),
            root_cwd: "/tmp/repos/alpha".to_string(),
            kind: ProjectKind::Root,
        }]));
        let mut wezterm = NoopWezterm;

        app.apply(AppAction::StartCommandInput, &mut wezterm)
            .expect("command input should start");
        for c in "git pull".chars() {
            app.apply(AppAction::EditInput(c), &mut wezterm)
                .expect("input should accept text");
        }
        app.apply(AppAction::ConfirmInput, &mut wezterm)
            .expect("confirm should queue command");

        let mut client = crate::client::Client::stub();

        super::flush_daemon_requests(&mut app, &mut client)
            .expect("queued mutation should send request without polling snapshot");

        assert_eq!(app.projects()[0].status_summary.display_text(), "M1");
    }

    #[test]
    fn reconnect_daemon_replaces_workspace_from_fresh_snapshot() {
        let mut app = App::from_snapshot(test_snapshot(&[ProjectEntry {
            name: "alpha".to_string(),
            cwd: "/tmp/repos/alpha".to_string(),
            branch: "main".to_string(),
            status_summary: ProjectStatusSummary::default(),
            root_name: "alpha".to_string(),
            root_cwd: "/tmp/repos/alpha".to_string(),
            kind: ProjectKind::Root,
        }]));
        let mut client =
            crate::client::Client::stub_with_snapshot(test_snapshot(&[ProjectEntry {
                name: "beta".to_string(),
                cwd: "/tmp/repos/beta".to_string(),
                branch: "main".to_string(),
                status_summary: ProjectStatusSummary::default(),
                root_name: "beta".to_string(),
                root_cwd: "/tmp/repos/beta".to_string(),
                kind: ProjectKind::Root,
            }]));

        reconnect_daemon(&mut app, &mut client).expect("reconnect should refresh app workspace");

        assert_eq!(app.projects()[0].name, "beta");
    }

    #[test]
    fn subscription_updates_replace_workspace_when_snapshot_changes() {
        let mut app = App::from_snapshot(test_snapshot(&[ProjectEntry {
            name: "alpha".to_string(),
            cwd: "/tmp/repos/alpha".to_string(),
            branch: "main".to_string(),
            status_summary: ProjectStatusSummary {
                modified: 1,
                ..ProjectStatusSummary::default()
            },
            root_name: "alpha".to_string(),
            root_cwd: "/tmp/repos/alpha".to_string(),
            kind: ProjectKind::Root,
        }]));
        let mut subscription =
            crate::client::Subscription::stub_with_updates(vec![test_snapshot(&[ProjectEntry {
                name: "alpha".to_string(),
                cwd: "/tmp/repos/alpha".to_string(),
                branch: "main".to_string(),
                status_summary: ProjectStatusSummary::default(),
                root_name: "alpha".to_string(),
                root_cwd: "/tmp/repos/alpha".to_string(),
                kind: ProjectKind::Root,
            }])]);

        apply_subscription_updates(&mut app, &mut subscription)
            .expect("subscription update should refresh workspace state");

        assert_eq!(app.projects()[0].status_summary.display_text(), "clean");
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

    fn test_snapshot(projects: &[ProjectEntry]) -> WorkspaceSnapshot {
        let mut snapshot = WorkspaceSnapshot {
            protocol_version: 1,
            generated_at_ms: 0,
            projects: Default::default(),
            project_order: Vec::new(),
            warnings: Vec::new(),
        };

        for project in projects {
            snapshot.project_order.push(project.cwd.clone());
            snapshot.projects.insert(
                project.cwd.clone(),
                ProjectSnapshot {
                    id: project.cwd.clone(),
                    name: project.name.clone(),
                    cwd: project.cwd.clone(),
                    root_id: project.root_cwd.clone(),
                    kind: project.kind.clone(),
                    git: ProjectGitState {
                        branch: project.branch.clone(),
                        status: project.status_summary.display_text(),
                        status_summary: project.status_summary.clone(),
                    },
                    ..ProjectSnapshot::default()
                },
            );
        }

        snapshot
    }
}
