mod app;
mod cli;
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
    let mut wezterm = ProcessWezterm;

    match cli.command {
        None => tui::run(&mut wezterm),
        Some(Commands::List) => cli::run_list(&mut wezterm),
        Some(Commands::Doctor) => cli::run_doctor(&mut wezterm),
    }
}
