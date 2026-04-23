use anyhow::Result;
use clap::{Args, Parser, Subcommand};

use crate::wezterm::{listable_panes, sort_panes, tui_pane_id_from_env, PaneInfo, WeztermClient};

#[derive(Debug, Parser)]
#[command(
    name = "nerve_center",
    version,
    about = "Control WezTerm panes from a Rust TUI"
)]
pub struct Cli {
    #[arg(long = "install-hooks-claude")]
    pub install_hooks_claude: bool,

    #[arg(long = "install-hooks-opencode")]
    pub install_hooks_opencode: bool,

    #[arg(long = "install-hooks-pi")]
    pub install_hooks_pi: bool,

    #[arg(long = "remove-hooks-claude")]
    pub remove_hooks_claude: bool,

    #[arg(long = "remove-hooks-opencode")]
    pub remove_hooks_opencode: bool,

    #[arg(long = "remove-hooks-pi")]
    pub remove_hooks_pi: bool,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Print the current pane list without starting the TUI
    List,
    /// Validate that WezTerm integration prerequisites are available
    Doctor,
    Daemon {
        #[command(subcommand)]
        command: Option<DaemonCommand>,
    },
    #[command(hide = true)]
    Internal(InternalCli),
}

#[derive(Debug, Subcommand, Clone, Copy)]
pub enum DaemonCommand {
    Run,
    Start,
    Stop,
    Restart,
}

#[derive(Debug, Args)]
pub struct InternalCli {
    #[command(subcommand)]
    pub command: InternalCommands,
}

#[derive(Debug, Subcommand, Clone, Copy)]
pub enum InternalCommands {
    IngestClaudeHook,
    IngestOpencodeEvent,
    IngestPiEvent,
}

pub fn run_list<W: WeztermClient>(wezterm: &mut W) -> Result<()> {
    let tui_pane_id = tui_pane_id_from_env()?;
    let mut panes = listable_panes(&wezterm.list_panes()?, tui_pane_id);
    sort_panes(&mut panes);

    if panes.is_empty() {
        println!("No panes available.");
        return Ok(());
    }

    for pane in panes {
        println!("{}", format_pane_line(&pane));
    }

    Ok(())
}

pub fn run_doctor<W: WeztermClient>(wezterm: &mut W) -> Result<()> {
    let tui_pane_id = tui_pane_id_from_env()?;
    let panes = wezterm.list_panes()?;
    let pane_count = panes.len();
    let listed_count = listable_panes(&panes, tui_pane_id).len();

    println!("WEZTERM_PANE={tui_pane_id}");
    println!("wezterm cli list returned {pane_count} panes");
    println!("selectable panes: {listed_count}");
    println!("doctor: ok");

    Ok(())
}

fn format_pane_line(pane: &PaneInfo) -> String {
    format!(
        "[pane {}] window={} tab={} active={} title={} cwd={}",
        pane.pane_id, pane.window_id, pane.tab_id, pane.is_active, pane.title, pane.cwd
    )
}

#[cfg(test)]
mod tests {
    use super::{Cli, Commands, DaemonCommand};
    use clap::Parser;

    #[test]
    fn parses_daemon_restart_subcommand() {
        let cli = Cli::parse_from(["nerve_center", "daemon", "restart"]);

        assert!(matches!(
            cli.command,
            Some(Commands::Daemon {
                command: Some(DaemonCommand::Restart)
            })
        ));
    }

    #[test]
    fn parses_bare_daemon_command_as_no_subcommand() {
        let cli = Cli::parse_from(["nerve_center", "daemon"]);

        assert!(matches!(
            cli.command,
            Some(Commands::Daemon { command: None })
        ));
    }
}
