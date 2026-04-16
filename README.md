# Nerve Center

`nerve_center` shows a list of projects and panes in a terminal UI.

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

Each project row in the `Projects` view is shown as:

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
