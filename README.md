# Nerve Center

`nerve_center` shows a list of projects and panes in a terminal UI.

## Project Status View

Each project row in the `Projects` view is shown as:

```text
project-name  branch  status
```

Example:

```text
alpha         main    S2 M1 ?3 ^1
beta          feature clean
gamma         fix/ui  D1 U1 v2
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
