# Changelog

## 0.3.0 - 2026-04-23

### Added
- A daemon-backed client/server architecture for workspace refresh, subscriptions, and project operations.
- Socket-based daemon lifecycle commands and reconnect behavior for the TUI client.

### Changed
- Moved workspace coordination out of the UI path so background refreshes and operations can be handled by the daemon.

### Fixed
- Client autostart tests now inject the existing-socket wait path instead of consulting ambient local daemon state.

### Commits since 0.2.0
- `a4d617c` update changelog for 0.1.0
- `0e1bcaf` updated with client server architecture

## 0.2.0 - 2026-04-18

### Added
- Support for launching and monitoring pi agent panes.
- A global follow mode to jump to the next agent pane that needs input.
- Project search in the TUI.
- Context-aware completion for agent and worktree commands.
- An `e` workflow that opens `nvim` in the TUI pane and returns to `nerve_center` when it exits.

### Changed
- Improved hook handling concurrency and cleaned up hook-related command flow.
- Expanded the README to cover follow mode, project commands, agent monitors, and hook installation.

### Fixed
- OpenCode hook ingestion now tracks input-required state correctly.

### Commits since 0.1.0
- `2455612` better concorrency in hooks and remove hooks commands
- `14f8f99` open nvim in tui pane
- `8f73af5` fix hooks to support imput state in opencode
- `2118c9b` add project search
- `866af6d` add agent command completion
- `83a6ec7` add a follow mode for active agent
- `91e47c2` add support for pi agent

## 0.1.0 - 2026-04-16

### Added
- Initial WezTerm-focused TUI for browsing local git projects.
- Project view with branch and git status summaries.
- Linked worktree management commands including add, remove, merge, PR, and land workflows.
- Agent monitoring for Claude and OpenCode panes.
- Support for multiple agents per project and tab switching between agents and projects.
- Config file support for defining repository sources.
- GitHub Actions release workflow.

### Changed
- Decoupled repository naming from branch naming in the UI.
- Updated project switching to change the pane cwd for better WezTerm project workflows.
- Improved command discovery and expanded the README.

### Commits included in 0.1.0
- `c0e14d0` intial switcher for wezterm
- `be06d0c` add prject view
- `0819bec` add level 1 worktree support
- `1d4dc8c` add remove command
- `16f42cb` add merge pr land
- `5518433` decouple branch name and repo name
- `510d2ec` add git status to project view
- `bb3416d` add agent monitoring opencode and claude
- `4ce8eaf` add gh workflow for release
- `60ddcc8` add config file
- `dec4dd7` add better command discovery
- `4e05de6` have proper tab switching for agents and projects
- `22b27d8` change cwd on project switch will make wezproject work wonders
- `3465b3c` add support for multiple agents in a project
- `cf07bf5` update readme
