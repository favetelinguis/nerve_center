mod app;
mod cli;
mod client;
mod command;
mod daemon;
mod hooks;
mod input;
mod project_id;
mod projects;
mod tui;
mod ui;
mod wezterm;
mod workspace;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Commands};
use wezterm::ProcessWezterm;

pub fn run() -> Result<()> {
    let cli = Cli::parse();

    if cli.remove_hooks_claude {
        hooks::remove_claude_hooks()?;
    }
    if cli.remove_hooks_opencode {
        hooks::remove_opencode_hooks()?;
    }
    if cli.remove_hooks_pi {
        hooks::remove_pi_hooks()?;
    }
    if cli.install_hooks_claude {
        hooks::install_claude_hooks()?;
    }
    if cli.install_hooks_opencode {
        hooks::install_opencode_hooks()?;
    }
    if cli.install_hooks_pi {
        hooks::install_pi_hooks()?;
    }
    if cli.remove_hooks_claude
        || cli.remove_hooks_opencode
        || cli.remove_hooks_pi
        || cli.install_hooks_claude
        || cli.install_hooks_opencode
        || cli.install_hooks_pi
    {
        return Ok(());
    }

    if let Some(Commands::Internal(internal)) = cli.command {
        return hooks::run_internal(internal.command);
    }

    let mut wezterm = ProcessWezterm;

    match cli.command {
        None => tui::run(&mut wezterm),
        Some(Commands::List) => cli::run_list(&mut wezterm),
        Some(Commands::Doctor) => cli::run_doctor(&mut wezterm),
        Some(Commands::Daemon { command }) => match command.unwrap_or(cli::DaemonCommand::Run) {
            cli::DaemonCommand::Run => daemon::run(),
            cli::DaemonCommand::Start => client::run_daemon_start(),
            cli::DaemonCommand::Stop => client::run_daemon_stop(),
            cli::DaemonCommand::Restart => client::run_daemon_restart(),
        },
        Some(Commands::Internal(_)) => unreachable!("handled before wezterm initialization"),
    }
}
