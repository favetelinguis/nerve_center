# Nerve Center

`nerve_center` is a WezTerm-focused terminal UI for browsing local git projects, managing linked worktrees, and jumping between Claude and OpenCode agent panes for the selected project.

## Install

Download the archive for your platform from the GitHub Releases page, extract it, and place the `nerve_center` binary somewhere on your `PATH`.

Typical setup:

1. Download the latest release archive for your OS and CPU.
2. Extract the archive.
3. Move `nerve_center` into a directory on your `PATH`, such as `~/bin` or `/usr/local/bin`.
4. Open WezTerm and run `nerve_center` from a shell pane.

Supported release artifacts:

- Linux: `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`
- macOS: `x86_64-apple-darwin`, `aarch64-apple-darwin`
- Windows: `x86_64-pc-windows-msvc`

## Requirements

- WezTerm
- Git
- A shell running inside WezTerm so `WEZTERM_PANE` is available

Optional tools used by specific features:

- `gh` for `wt pr`
- `idea` for the `o` keybinding
- `nvim` for the `e` keybinding
- `claude` and/or `opencode` for `agent <runtime>` commands

## Quick Start

1. Run `nerve_center` inside a WezTerm pane.
2. Move through projects with `j` and `k`.
3. Press `:` to enter a project command.
4. Use `i` to attach the selected project's preferred running agent.
5. Press `Esc` to stop forwarding keys back to the attached pane.

When multiple agents are running for the selected project, `i` prefers the first agent that currently needs input. While forwarding is active, use `Left` and `Right` to switch to the previous or next running agent for that same project. Switching stops at the first and last agent; it does not wrap.

## Configuration

On first start, `nerve_center` creates `~/.config/nerve_center/config.toml` with:

```toml
repo_sources = ["~/repos"]
```

`repo_sources` is the list of directories scanned for the projects view. Every configured path must exist and be a directory or startup will fail with an error naming the invalid path.

## Running the App

Start the TUI:

```sh
nerve_center
```

Useful companion commands:

```sh
nerve_center list
nerve_center doctor
```

- `nerve_center list`: print the current selectable WezTerm pane list without starting the TUI
- `nerve_center doctor`: validate that WezTerm integration is working and print basic pane information

## Keybindings

### Normal Mode

- `j`: move selection down
- `k`: move selection up
- `:`: open project command input for the selected project
- `i`: attach the selected project's preferred running agent into the side pane and start forwarding mode
- `o`: open the selected project with `idea <project-path>`
- `t`: open a shell for the selected project in the side pane without refocusing the TUI
- `e`: open `nvim` for the selected project in the side pane without refocusing the TUI
- `q`: quit

### Command Input Mode

- Type to enter a command
- `Enter`: run the command
- `Backspace`: delete one character
- `Esc`: cancel command input

### Forwarding Mode

Forwarding mode starts after a successful `i` attach.

- Printable characters: forwarded to the attached pane
- `Enter`: forwarded to the attached pane
- `Tab`: forwarded to the attached pane
- `Backspace`: forwarded to the attached pane
- `Left`: switch to the previous running agent for the attached project
- `Right`: switch to the next running agent for the attached project
- `Esc`: stop forwarding keys and return to normal mode

## Project Commands

Press `:` and enter one of the following commands for the selected project.

### Agent Commands

- `agent claude`: open a new Claude tab rooted at the selected project
- `agent opencode`: open a new OpenCode tab rooted at the selected project

### Worktree Commands

- `wt add <branch>`: create a new linked worktree on `<branch>` in a generated sibling directory
- `wt remove`: remove the selected linked worktree and delete its branch
- `wt merge [target]`: merge the selected worktree branch into `[target]`
- `wt pr [target]`: push the selected worktree branch and create or reuse a pull request targeting `[target]`
- `wt land [target]`: merge the selected worktree branch into `[target]`, then remove the worktree and branch

Notes:

- `wt remove`, `wt merge`, `wt pr`, and `wt land` are intended for linked worktrees, not root repositories.
- If `[target]` is omitted, `nerve_center` resolves a default branch for the repository, usually the remote default branch such as `main`.
- `wt pr` requires `gh` and a configured git remote.

## Hook Installation

Install Claude Code hooks:

```sh
nerve_center --install-hooks-claude
```

Install the OpenCode plugin hook bridge:

```sh
nerve_center --install-hooks-opencode
```

These installers write agent state files to `~/.local/data/nerve_center/<wezterm-pane-id>` when the agent is working, done, errored, or waiting for user input.

There are also hidden internal subcommands used by those hook installers:

- `nerve_center internal ingest-claude-hook`
- `nerve_center internal ingest-opencode-event`

These are not meant to be run manually.

## Project Status View

Each project row in the projects view is shown as:

```text
project-name  branch  status  agents
```

Example:

```text
alpha         main    S2 M1 ?3 ^1  cc:341[w]
beta          feature clean        -
gamma         fix/ui  D1 U1 v2     cc:355[w] oc:366[i]
```

Status symbols:

- `clean`: no local file changes
- `S<N>`: staged files
- `M<N>`: unstaged modified files
- `D<N>`: deleted files
- `?<N>`: untracked files
- `U<N>`: conflicted files
- `^<N>`: commits ahead of the upstream branch
- `v<N>`: commits behind the upstream branch

Agent monitor symbols:

- `cc`: Claude
- `oc`: OpenCode
- `[w]`: working
- `[i]`: needs input
- `[d]`: done
- `[e]`: error

Notes:

- Counts are per file, not per line.
- `clean` may still be followed by `^<N>` or `v<N>` when the branch differs from upstream but the working tree has no local file changes.
- `agents` shows monitored agent panes for the project.

## Typical Workflow

1. Open WezTerm in a pane and run `nerve_center`.
2. Select a repository or linked worktree with `j` and `k`.
3. Start an agent with `:agent claude` or `:agent opencode`.
4. Install hooks if you want live agent state in the project list.
5. Press `i` to attach the agent when it is time to interact with it.
6. Use `Left` and `Right` in forwarding mode if that project has multiple running agents.
7. Use `:wt add`, `:wt merge`, `:wt pr`, `:wt land`, or `:wt remove` to manage worktrees from the same UI.

## Releases

Release artifacts are built with `cargo-dist` by GitHub Actions when you push a version tag such as `v0.1.0`.

Typical release flow:

```sh
git tag v0.1.0
git push origin v0.1.0
```
