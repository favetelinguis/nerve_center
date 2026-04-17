use std::collections::BTreeSet;
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};

use crate::wezterm::SpawnCommand;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandProjectKind {
    Root,
    Worktree,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRuntime {
    Claude,
    OpenCode,
}

impl AgentRuntime {
    pub fn parse_command(name: &str) -> Result<Self> {
        match name {
            "claude" | "claude-code" => Ok(Self::Claude),
            "opencode" => Ok(Self::OpenCode),
            _ => bail!("unknown agent runtime: {name}"),
        }
    }

    pub fn from_state_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "claude" | "claude-code" | "cc" => Some(Self::Claude),
            "opencode" | "oc" => Some(Self::OpenCode),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::OpenCode => "opencode",
        }
    }

    pub fn short_label(self) -> &'static str {
        match self {
            Self::Claude => "cc",
            Self::OpenCode => "oc",
        }
    }

    pub fn spawn_command(self) -> SpawnCommand {
        SpawnCommand::new(self.label(), vec![self.label().to_string()])
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectCommand {
    Add { branch: String },
    Remove,
    Merge { target: Option<String> },
    Pr { target: Option<String> },
    Land { target: Option<String> },
    Agent { runtime: AgentRuntime },
    GitSwitch { branch: String },
    GitPull,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandCompletion {
    pub label: String,
    pub insert_text: Option<String>,
    pub append_space: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CompletionState {
    pub items: Vec<CommandCompletion>,
}

#[derive(Debug, Clone, Copy)]
pub struct CommandContext<'a> {
    pub project_kind: Option<CommandProjectKind>,
    pub root_cwd: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandScope {
    Any,
    RootOnly,
    WorktreeOnly,
}

type DynamicCompletionProvider = for<'a> fn(&CommandContext<'a>) -> Result<Vec<String>>;

#[derive(Clone, Copy)]
enum CompletionSource {
    None,
    Static(&'static [&'static str]),
    Dynamic(DynamicCompletionProvider),
}

#[derive(Clone, Copy)]
struct ArgumentSpec {
    name: &'static str,
    required: bool,
    source: CompletionSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeafKind {
    Agent,
    WtAdd,
    WtRemove,
    WtMerge,
    WtPr,
    WtLand,
    GitSwitch,
    GitPull,
}

#[derive(Clone, Copy)]
enum CommandNext {
    Branch(&'static [CommandSpec]),
    Leaf {
        kind: LeafKind,
        args: &'static [ArgumentSpec],
    },
}

#[derive(Clone, Copy)]
struct CommandSpec {
    token: &'static str,
    scope: CommandScope,
    next: CommandNext,
}

const NO_ARGS: &[ArgumentSpec] = &[];
const AGENT_RUNTIME_ARGS: &[ArgumentSpec] = &[ArgumentSpec {
    name: "runtime",
    required: true,
    source: CompletionSource::Static(&["claude", "opencode"]),
}];
const WT_ADD_ARGS: &[ArgumentSpec] = &[ArgumentSpec {
    name: "branch-name",
    required: true,
    source: CompletionSource::None,
}];
const WT_TARGET_ARGS: &[ArgumentSpec] = &[ArgumentSpec {
    name: "target",
    required: false,
    source: CompletionSource::Dynamic(list_git_branches),
}];
const GIT_SWITCH_ARGS: &[ArgumentSpec] = &[ArgumentSpec {
    name: "branchname",
    required: true,
    source: CompletionSource::Dynamic(list_git_branches),
}];

const WT_SUBCOMMANDS: &[CommandSpec] = &[
    CommandSpec {
        token: "add",
        scope: CommandScope::Any,
        next: CommandNext::Leaf {
            kind: LeafKind::WtAdd,
            args: WT_ADD_ARGS,
        },
    },
    CommandSpec {
        token: "remove",
        scope: CommandScope::WorktreeOnly,
        next: CommandNext::Leaf {
            kind: LeafKind::WtRemove,
            args: NO_ARGS,
        },
    },
    CommandSpec {
        token: "merge",
        scope: CommandScope::WorktreeOnly,
        next: CommandNext::Leaf {
            kind: LeafKind::WtMerge,
            args: WT_TARGET_ARGS,
        },
    },
    CommandSpec {
        token: "pr",
        scope: CommandScope::WorktreeOnly,
        next: CommandNext::Leaf {
            kind: LeafKind::WtPr,
            args: WT_TARGET_ARGS,
        },
    },
    CommandSpec {
        token: "land",
        scope: CommandScope::WorktreeOnly,
        next: CommandNext::Leaf {
            kind: LeafKind::WtLand,
            args: WT_TARGET_ARGS,
        },
    },
];

const GIT_SUBCOMMANDS: &[CommandSpec] = &[
    CommandSpec {
        token: "switch",
        scope: CommandScope::RootOnly,
        next: CommandNext::Leaf {
            kind: LeafKind::GitSwitch,
            args: GIT_SWITCH_ARGS,
        },
    },
    CommandSpec {
        token: "pull",
        scope: CommandScope::RootOnly,
        next: CommandNext::Leaf {
            kind: LeafKind::GitPull,
            args: NO_ARGS,
        },
    },
];

const TOP_LEVEL_COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        token: "agent",
        scope: CommandScope::Any,
        next: CommandNext::Leaf {
            kind: LeafKind::Agent,
            args: AGENT_RUNTIME_ARGS,
        },
    },
    CommandSpec {
        token: "wt",
        scope: CommandScope::Any,
        next: CommandNext::Branch(WT_SUBCOMMANDS),
    },
    CommandSpec {
        token: "git",
        scope: CommandScope::RootOnly,
        next: CommandNext::Branch(GIT_SUBCOMMANDS),
    },
];

pub fn parse_project_command(command: &str) -> Result<ProjectCommand> {
    let parts = command.split_whitespace().collect::<Vec<_>>();
    let Some(name) = parts.first().copied() else {
        bail!("empty projects command")
    };

    let Some(spec) = TOP_LEVEL_COMMANDS.iter().find(|spec| spec.token == name) else {
        bail!("unknown projects command: {command}");
    };

    parse_spec(spec, &parts[1..], command)
}

pub fn complete_command(command: &str, context: &CommandContext<'_>) -> Result<CompletionState> {
    if context.project_kind.is_none() {
        return Ok(CompletionState::default());
    }

    let parsed = ParsedInput::from(command);
    let (complete_tokens, partial) = parsed.position();
    let candidates = complete_from_specs(TOP_LEVEL_COMMANDS, complete_tokens, partial, context)?;
    Ok(CompletionState { items: candidates })
}

fn parse_spec(spec: &CommandSpec, remaining: &[&str], command: &str) -> Result<ProjectCommand> {
    match spec.next {
        CommandNext::Branch(children) => {
            let Some(child_name) = remaining.first().copied() else {
                return missing_subcommand_error(spec.token);
            };
            let Some(child) = children.iter().find(|child| child.token == child_name) else {
                return unknown_subcommand_error(spec.token, command, child_name);
            };
            parse_spec(child, &remaining[1..], command)
        }
        CommandNext::Leaf { kind, args } => parse_leaf(kind, args, remaining, command),
    }
}

fn parse_leaf(
    kind: LeafKind,
    args: &[ArgumentSpec],
    remaining: &[&str],
    command: &str,
) -> Result<ProjectCommand> {
    let required_count = args.iter().filter(|arg| arg.required).count();
    if remaining.len() < required_count {
        match kind {
            LeafKind::Agent => bail!("agent requires a runtime"),
            LeafKind::WtAdd => bail!("wt add requires a branch name"),
            LeafKind::GitSwitch => bail!("git switch requires a branch name"),
            _ => bail!("missing required arguments for projects command: {command}"),
        }
    }

    if remaining.len() > args.len() {
        match kind {
            LeafKind::WtRemove => bail!("wt remove does not take a target branch"),
            _ => bail!("too many arguments for projects command: {command}"),
        }
    }

    match kind {
        LeafKind::Agent => Ok(ProjectCommand::Agent {
            runtime: AgentRuntime::parse_command(remaining[0])?,
        }),
        LeafKind::WtAdd => Ok(ProjectCommand::Add {
            branch: remaining[0].to_string(),
        }),
        LeafKind::WtRemove => Ok(ProjectCommand::Remove),
        LeafKind::WtMerge => Ok(ProjectCommand::Merge {
            target: remaining.first().map(|value| (*value).to_string()),
        }),
        LeafKind::WtPr => Ok(ProjectCommand::Pr {
            target: remaining.first().map(|value| (*value).to_string()),
        }),
        LeafKind::WtLand => Ok(ProjectCommand::Land {
            target: remaining.first().map(|value| (*value).to_string()),
        }),
        LeafKind::GitSwitch => Ok(ProjectCommand::GitSwitch {
            branch: remaining[0].to_string(),
        }),
        LeafKind::GitPull => Ok(ProjectCommand::GitPull),
    }
}

fn missing_subcommand_error(token: &str) -> Result<ProjectCommand> {
    match token {
        "wt" => bail!("wt requires a subcommand"),
        "git" => bail!("git requires a subcommand"),
        _ => bail!("missing subcommand for {token}"),
    }
}

fn unknown_subcommand_error(
    token: &str,
    command: &str,
    child_name: &str,
) -> Result<ProjectCommand> {
    match token {
        "agent" => bail!("unknown agent runtime: {child_name}"),
        "wt" => bail!("unknown worktree command: {command}"),
        "git" => bail!("unknown git command: {command}"),
        _ => bail!("unknown subcommand for {token}: {command}"),
    }
}

fn complete_from_specs(
    specs: &'static [CommandSpec],
    complete_tokens: &[&str],
    partial: &str,
    context: &CommandContext<'_>,
) -> Result<Vec<CommandCompletion>> {
    if complete_tokens.is_empty() {
        return complete_specs(specs, partial, context);
    }

    let token = complete_tokens[0];
    let Some(spec) = specs
        .iter()
        .find(|spec| spec.token == token && scope_matches(spec.scope, context.project_kind))
    else {
        return Ok(Vec::new());
    };

    match spec.next {
        CommandNext::Branch(children) => {
            complete_from_specs(children, &complete_tokens[1..], partial, context)
        }
        CommandNext::Leaf { args, .. } => {
            complete_leaf(args, &complete_tokens[1..], partial, context)
        }
    }
}

fn complete_specs(
    specs: &'static [CommandSpec],
    partial: &str,
    context: &CommandContext<'_>,
) -> Result<Vec<CommandCompletion>> {
    let query = partial.to_ascii_lowercase();
    Ok(specs
        .iter()
        .filter(|spec| scope_matches(spec.scope, context.project_kind))
        .filter(|spec| spec.token.to_ascii_lowercase().contains(&query))
        .map(|spec| CommandCompletion {
            label: spec.token.to_string(),
            insert_text: Some(spec.token.to_string()),
            append_space: should_append_space(spec.next),
        })
        .collect())
}

fn complete_leaf(
    args: &[ArgumentSpec],
    consumed_args: &[&str],
    partial: &str,
    context: &CommandContext<'_>,
) -> Result<Vec<CommandCompletion>> {
    let Some(arg) = args.get(consumed_args.len()) else {
        return Ok(Vec::new());
    };

    complete_argument(arg, partial, context)
}

fn complete_argument(
    arg: &ArgumentSpec,
    partial: &str,
    context: &CommandContext<'_>,
) -> Result<Vec<CommandCompletion>> {
    let query = partial.to_ascii_lowercase();
    let values = match arg.source {
        CompletionSource::None => Vec::new(),
        CompletionSource::Static(values) => {
            values.iter().map(|value| (*value).to_string()).collect()
        }
        CompletionSource::Dynamic(provider) => provider(context)?,
    };

    let filtered = values
        .into_iter()
        .filter(|value| value.to_ascii_lowercase().contains(&query))
        .map(|value| CommandCompletion {
            label: value.clone(),
            insert_text: Some(value),
            append_space: false,
        })
        .collect::<Vec<_>>();
    if !filtered.is_empty() {
        return Ok(filtered);
    }

    Ok(vec![CommandCompletion {
        label: format!("[{}]", arg.name),
        insert_text: None,
        append_space: false,
    }])
}

fn should_append_space(next: CommandNext) -> bool {
    match next {
        CommandNext::Branch(_) => true,
        CommandNext::Leaf { args, .. } => !args.is_empty(),
    }
}

fn scope_matches(scope: CommandScope, project_kind: Option<CommandProjectKind>) -> bool {
    match (scope, project_kind) {
        (_, None) => false,
        (CommandScope::Any, Some(_)) => true,
        (CommandScope::RootOnly, Some(CommandProjectKind::Root)) => true,
        (CommandScope::WorktreeOnly, Some(CommandProjectKind::Worktree)) => true,
        _ => false,
    }
}

fn list_git_branches(context: &CommandContext<'_>) -> Result<Vec<String>> {
    let root_cwd = context
        .root_cwd
        .ok_or_else(|| anyhow!("command completion requires a selected project root"))?;

    let mut branches = list_git_refs(root_cwd, "refs/heads")?;
    branches.extend(list_git_refs(root_cwd, "refs/remotes")?);
    Ok(branches)
}

fn list_git_refs(root_cwd: &str, namespace: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args([
            "-C",
            root_cwd,
            "for-each-ref",
            "--format=%(refname:short)",
            namespace,
        ])
        .output()
        .with_context(|| format!("failed to list git branches for {root_cwd}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "failed to list git branches for {root_cwd}: {}",
            stderr.trim()
        );
    }

    let stdout =
        String::from_utf8(output.stdout).context("git branch stdout was not valid UTF-8")?;
    let mut branches = BTreeSet::new();
    for line in stdout.lines().map(str::trim) {
        if line.is_empty() || line.ends_with("/HEAD") {
            continue;
        }
        branches.insert(line.to_string());
    }
    Ok(branches.into_iter().collect())
}

#[derive(Debug, Clone)]
struct ParsedInput<'a> {
    tokens: Vec<&'a str>,
    trailing_space: bool,
}

impl<'a> ParsedInput<'a> {
    fn from(input: &'a str) -> Self {
        Self {
            tokens: input.split_whitespace().collect(),
            trailing_space: input.ends_with(char::is_whitespace),
        }
    }

    fn position(&self) -> (&[&'a str], &'a str) {
        if self.trailing_space {
            (&self.tokens, "")
        } else {
            match self.tokens.split_last() {
                Some((last, rest)) => (rest, last),
                None => (&[], ""),
            }
        }
    }
}

pub fn apply_completion(command: &str, completion: &CommandCompletion) -> String {
    let Some(insert_text) = completion.insert_text.as_deref() else {
        return command.to_string();
    };

    let parsed = ParsedInput::from(command);
    let mut tokens = parsed
        .tokens
        .iter()
        .map(|token| (*token).to_string())
        .collect::<Vec<_>>();
    if parsed.trailing_space {
        tokens.push(insert_text.to_string());
    } else if tokens.is_empty() {
        tokens.push(insert_text.to_string());
    } else if let Some(last) = tokens.last_mut() {
        *last = insert_text.to_string();
    }

    let mut updated = tokens.join(" ");
    if completion.append_space && !updated.is_empty() {
        updated.push(' ');
    }
    updated
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        AgentRuntime, CommandCompletion, CommandContext, CommandProjectKind, ProjectCommand,
        apply_completion, complete_command, parse_project_command,
    };

    #[test]
    fn root_commands_include_git_and_worktree_root_only_actions_are_hidden() {
        let root = complete_labels(
            CommandContext {
                project_kind: Some(CommandProjectKind::Root),
                root_cwd: Some("/tmp/root"),
            },
            "",
        )
        .expect("root completions should resolve");
        assert_eq!(root, vec!["agent", "wt", "git"]);

        let wt_root = complete_labels(
            CommandContext {
                project_kind: Some(CommandProjectKind::Root),
                root_cwd: Some("/tmp/root"),
            },
            "wt ",
        )
        .expect("wt root completions should resolve");
        assert_eq!(wt_root, vec!["add"]);

        let git_root = complete_labels(
            CommandContext {
                project_kind: Some(CommandProjectKind::Root),
                root_cwd: Some("/tmp/root"),
            },
            "git ",
        )
        .expect("git root completions should resolve");
        assert_eq!(git_root, vec!["switch", "pull"]);
    }

    #[test]
    fn worktree_commands_hide_git_and_show_worktree_actions() {
        let worktree = complete_labels(
            CommandContext {
                project_kind: Some(CommandProjectKind::Worktree),
                root_cwd: Some("/tmp/root"),
            },
            "",
        )
        .expect("worktree completions should resolve");
        assert_eq!(worktree, vec!["agent", "wt"]);

        let wt_worktree = complete_labels(
            CommandContext {
                project_kind: Some(CommandProjectKind::Worktree),
                root_cwd: Some("/tmp/root"),
            },
            "wt ",
        )
        .expect("wt worktree completions should resolve");
        assert_eq!(wt_worktree, vec!["add", "remove", "merge", "pr", "land"]);
    }

    #[test]
    fn wt_add_shows_branch_placeholder() {
        let items = complete_items(
            CommandContext {
                project_kind: Some(CommandProjectKind::Root),
                root_cwd: Some("/tmp/root"),
            },
            "wt add ",
        )
        .expect("wt add placeholder should resolve");
        assert_eq!(items, vec!["[branch-name]"]);
    }

    #[test]
    fn git_switch_uses_dynamic_branch_completion() {
        let repo = create_git_repo("git-switch-completion");
        git(&repo, &["branch", "feature/one"]);
        git(&repo, &["branch", "feature/two"]);

        let items = complete_labels(
            CommandContext {
                project_kind: Some(CommandProjectKind::Root),
                root_cwd: Some(repo.to_str().unwrap()),
            },
            "git switch one",
        )
        .expect("git branches should complete");
        assert_eq!(items, vec!["feature/one"]);
    }

    #[test]
    fn git_switch_completion_includes_remote_tracking_branches() {
        let repo = create_git_repo("git-switch-remote-completion");
        let remote = test_sandbox("git-switch-remote-origin").join("origin.git");

        git(
            &test_sandbox_parent(&remote),
            &["init", "--bare", remote.to_str().unwrap()],
        );
        git(
            &repo,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&repo, &["push", "-u", "origin", "main"]);
        git(&repo, &["switch", "-c", "feature/remote-only"]);
        write_file(&repo.join("remote.txt"), "remote\n");
        git(
            &repo,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "add",
                ".",
            ],
        );
        git(
            &repo,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                "remote branch",
            ],
        );
        git(&repo, &["push", "-u", "origin", "feature/remote-only"]);
        git(&repo, &["switch", "main"]);
        git(&repo, &["branch", "-D", "feature/remote-only"]);
        git(&repo, &["fetch", "origin"]);

        let items = complete_labels(
            CommandContext {
                project_kind: Some(CommandProjectKind::Root),
                root_cwd: Some(repo.to_str().unwrap()),
            },
            "git switch origin/feature",
        )
        .expect("remote git branches should complete");
        assert_eq!(items, vec!["origin/feature/remote-only"]);
    }

    #[test]
    fn git_switch_completion_prefers_local_branches_before_remote_tracking_branches() {
        let repo = create_git_repo("git-switch-local-first");
        let remote = test_sandbox("git-switch-local-first-origin").join("origin.git");

        git(
            &test_sandbox_parent(&remote),
            &["init", "--bare", remote.to_str().unwrap()],
        );
        git(
            &repo,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&repo, &["push", "-u", "origin", "main"]);
        git(&repo, &["branch", "feature/match"]);
        git(&repo, &["push", "origin", "main:feature/match"]);
        git(&repo, &["fetch", "origin"]);

        let items = complete_labels(
            CommandContext {
                project_kind: Some(CommandProjectKind::Root),
                root_cwd: Some(repo.to_str().unwrap()),
            },
            "git switch match",
        )
        .expect("git branches should complete");
        assert_eq!(items, vec!["feature/match", "origin/feature/match"]);
    }

    #[test]
    fn parses_git_switch_and_existing_commands() {
        assert_eq!(
            parse_project_command("git switch feature/test").expect("git switch should parse"),
            ProjectCommand::GitSwitch {
                branch: "feature/test".to_string(),
            }
        );
        assert_eq!(
            parse_project_command("git pull").expect("git pull should parse"),
            ProjectCommand::GitPull
        );
        assert_eq!(
            parse_project_command("agent claude").expect("agent should parse"),
            ProjectCommand::Agent {
                runtime: AgentRuntime::Claude,
            }
        );
    }

    #[test]
    fn apply_completion_replaces_partial_tokens_and_appends_space_when_needed() {
        let completed = apply_completion(
            "git sw",
            &CommandCompletion {
                label: "switch".to_string(),
                insert_text: Some("switch".to_string()),
                append_space: true,
            },
        );
        assert_eq!(completed, "git switch ");

        let branch = apply_completion(
            "git switch fea",
            &CommandCompletion {
                label: "feature/test".to_string(),
                insert_text: Some("feature/test".to_string()),
                append_space: false,
            },
        );
        assert_eq!(branch, "git switch feature/test");
    }

    fn complete_labels(context: CommandContext<'_>, command: &str) -> anyhow::Result<Vec<String>> {
        complete_command(command, &context)
            .map(|state| state.items.into_iter().map(|item| item.label).collect())
    }

    fn complete_items(context: CommandContext<'_>, command: &str) -> anyhow::Result<Vec<String>> {
        complete_labels(context, command)
    }

    fn create_git_repo(label: &str) -> PathBuf {
        let root = test_sandbox(label);
        git(&root, &["init", "--initial-branch=main"]);
        write_file(&root.join("tracked.txt"), "hello\n");
        git(
            &root,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "add",
                "tracked.txt",
            ],
        );
        git(
            &root,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                "init",
            ],
        );
        root
    }

    fn git(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .status()
            .expect("git should spawn");
        assert!(status.success(), "git command should succeed: git {args:?}");
    }

    fn write_file(path: &Path, content: &str) {
        fs::write(path, content).expect("file should be written");
    }

    fn test_sandbox(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        let path = env::temp_dir().join(format!("nerve-center-command-{label}-{unique}"));
        fs::create_dir_all(&path).expect("sandbox should be created");
        path
    }

    fn test_sandbox_parent(path: &Path) -> PathBuf {
        path.parent()
            .expect("path should have a parent")
            .to_path_buf()
    }
}
