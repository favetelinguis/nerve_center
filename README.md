# Nerve Center

`nerve_center` shows a list of projects in a terminal UI.

## Config

On first start, `nerve_center` creates `~/.config/nerve_center/config.toml` with:

```toml
repo_sources = ["~/repos"]
```

`repo_sources` is the list of directories scanned for the projects view. Every configured path must exist and be a directory or startup will fail with an error naming the invalid path.

## Keybindings

- `j` / `k`: move project selection
- `:`: run a project command
- `i`: swap the first running agent for the selected project into the side pane and refocus the TUI
- `o`: open the selected project in IntelliJ IDEA via `idea <project-path>`
- `t`: spawn a terminal for the selected project and swap it into the side pane without refocusing the TUI
- `e`: spawn `nvim` for the selected project and swap it into the side pane without refocusing the TUI
- `q`: quit

## Releases

Release artifacts are built with `cargo-dist` by GitHub Actions when you push a
version tag such as `v0.1.0`.

The generated release workflow publishes downloadable archives for:

- Linux: `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`
- macOS: `x86_64-apple-darwin`, `aarch64-apple-darwin`
- Windows: `x86_64-pc-windows-msvc`

Typical release flow:

```sh
git tag v0.1.0
git push origin v0.1.0
```

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

Notes:

- Counts are per file, not per line.
- `clean` may still be followed by `^<N>` or `v<N>` when the branch differs from upstream but the working tree has no local file changes.
- `agents` shows monitored agent panes for the project. `cc` is Claude, `oc` is OpenCode.
- Agent state markers are `[w]` working, `[i]` needs input, `[d]` done, and `[e]` error.
