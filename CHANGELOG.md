# Changelog

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
