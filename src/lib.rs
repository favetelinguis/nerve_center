mod app;
mod cli;
mod hooks;
mod input;
mod tui;
mod ui;
mod wezterm;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Commands};
use wezterm::ProcessWezterm;

pub fn run() -> Result<()> {
    let cli = Cli::parse();

    if cli.install_hooks_claude {
        hooks::install_claude_hooks()?;
    }
    if cli.install_hooks_opencode {
        hooks::install_opencode_hooks()?;
    }
    if cli.install_hooks_claude || cli.install_hooks_opencode {
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
        Some(Commands::Internal(_)) => unreachable!("handled before wezterm initialization"),
    }
}
