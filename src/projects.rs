use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::command::AgentRuntime;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ProjectKind {
    #[default]
    Root,
    Worktree,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectEntry {
    pub name: String,
    pub cwd: String,
    pub branch: String,
    pub status_summary: ProjectStatusSummary,
    pub root_name: String,
    pub root_cwd: String,
    pub kind: ProjectKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ProjectStatusSummary {
    pub staged: usize,
    pub modified: usize,
    pub deleted: usize,
    pub untracked: usize,
    pub conflicts: usize,
    pub ahead: usize,
    pub behind: usize,
}

impl ProjectStatusSummary {
    fn has_local_changes(&self) -> bool {
        self.staged > 0
            || self.modified > 0
            || self.deleted > 0
            || self.untracked > 0
            || self.conflicts > 0
    }

    pub fn display_text(&self) -> String {
        let mut parts = Vec::new();

        if !self.has_local_changes() {
            parts.push("clean".to_string());
        } else {
            if self.staged > 0 {
                parts.push(format!("S{}", self.staged));
            }
            if self.modified > 0 {
                parts.push(format!("M{}", self.modified));
            }
            if self.deleted > 0 {
                parts.push(format!("D{}", self.deleted));
            }
            if self.untracked > 0 {
                parts.push(format!("?{}", self.untracked));
            }
            if self.conflicts > 0 {
                parts.push(format!("U{}", self.conflicts));
            }
        }

        if self.ahead > 0 {
            parts.push(format!("^{}", self.ahead));
        }
        if self.behind > 0 {
            parts.push(format!("v{}", self.behind));
        }

        parts.join(" ")
    }
}

impl ProjectEntry {
    pub fn list_label(&self) -> String {
        match self.kind {
            ProjectKind::Root => self.name.clone(),
            ProjectKind::Worktree => format!("  |- {}", self.name),
        }
    }

    pub(crate) fn search_label(&self) -> &str {
        self.name.as_str()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentMonitorState {
    Starting,
    Working,
    NeedsInput,
    Done,
    Error,
}

impl AgentMonitorState {
    fn from_state_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "working" | "busy" | "running" | "retry" => Some(Self::Working),
            "needs_input" | "needs-input" | "input" | "awaiting_user" | "awaiting-user" => {
                Some(Self::NeedsInput)
            }
            "done" | "idle" | "complete" | "completed" => Some(Self::Done),
            "error" | "failed" => Some(Self::Error),
            _ => None,
        }
    }

    pub(crate) fn short_code(self) -> char {
        match self {
            Self::Starting => 's',
            Self::Working => 'w',
            Self::NeedsInput => 'i',
            Self::Done => 'd',
            Self::Error => 'e',
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectAgentMonitor {
    pub(crate) pane_id: u64,
    pub(crate) runtime: AgentRuntime,
    pub(crate) state: AgentMonitorState,
}

impl ProjectAgentMonitor {
    pub fn display_text(&self) -> String {
        format!(
            "{}:{}[{}]",
            self.runtime.short_label(),
            self.pane_id,
            self.state.short_code()
        )
    }
}

const DEFAULT_REPO_SOURCE: &str = "~/repos";

#[derive(Debug, Deserialize)]
struct AppConfig {
    repo_sources: Vec<String>,
}

pub(crate) fn discover_projects_in(repo_sources: &[PathBuf]) -> Result<Vec<ProjectEntry>> {
    let mut probes = BTreeMap::new();

    for repo_source in repo_sources {
        for entry in fs::read_dir(repo_source)
            .with_context(|| format!("failed to read repo source {}", repo_source.display()))?
        {
            let entry = entry
                .with_context(|| format!("failed to read entry in {}", repo_source.display()))?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            if let Some(probe) = inspect_git_project(&path)? {
                probes.entry(probe.cwd.clone()).or_insert(probe);
            }
        }
    }

    Ok(build_project_entries(probes.into_values().collect()))
}

fn home_dir_from_env() -> Result<PathBuf> {
    let home = env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home))
}

pub(crate) fn config_path_from_home(home: &Path) -> PathBuf {
    home.join(".config/nerve_center/config.toml")
}

pub(crate) fn load_repo_sources_from_config() -> Result<Vec<PathBuf>> {
    let home = home_dir_from_env()?;
    let config_path = config_path_from_home(&home);
    load_repo_sources_from_config_at(&config_path, &home)
}

pub(crate) fn load_repo_sources_from_config_at(
    config_path: &Path,
    home: &Path,
) -> Result<Vec<PathBuf>> {
    ensure_repo_config_exists(config_path)?;

    let content = fs::read_to_string(config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let config: AppConfig = toml::from_str(&content)
        .with_context(|| format!("failed to parse {}", config_path.display()))?;
    normalize_repo_sources(&config.repo_sources, home)
}

fn ensure_repo_config_exists(config_path: &Path) -> Result<()> {
    if config_path.exists() {
        return Ok(());
    }

    let parent = config_path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", config_path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    fs::write(config_path, default_repo_config())
        .with_context(|| format!("failed to write {}", config_path.display()))?;
    Ok(())
}

fn default_repo_config() -> String {
    format!("repo_sources = [\"{}\"]\n", DEFAULT_REPO_SOURCE)
}

fn normalize_repo_sources(repo_sources: &[String], home: &Path) -> Result<Vec<PathBuf>> {
    if repo_sources.is_empty() {
        bail!("repo_sources must contain at least one directory")
    }

    let mut normalized = BTreeMap::new();
    for repo_source in repo_sources {
        let path = expand_home_path(repo_source, home);
        if !path.exists() {
            bail!("configured repo source does not exist: {}", path.display())
        }
        if !path.is_dir() {
            bail!(
                "configured repo source is not a directory: {}",
                path.display()
            )
        }

        let canonical = path
            .canonicalize()
            .with_context(|| format!("failed to resolve {}", path.display()))?;
        normalized
            .entry(canonical.to_string_lossy().into_owned())
            .or_insert(canonical);
    }

    Ok(normalized.into_values().collect())
}

fn expand_home_path(path: &str, home: &Path) -> PathBuf {
    if path == "~" {
        return home.to_path_buf();
    }

    if let Some(rest) = path.strip_prefix("~/") {
        return home.join(rest);
    }

    PathBuf::from(path)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitProjectProbe {
    pub(crate) name: String,
    pub(crate) cwd: String,
    pub(crate) branch: String,
    pub(crate) status_summary: ProjectStatusSummary,
    pub(crate) root_name: String,
    pub(crate) root_cwd: String,
    pub(crate) common_dir: String,
    pub(crate) is_root: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitStatusProbe {
    pub(crate) branch: String,
    pub(crate) status_summary: ProjectStatusSummary,
}

fn inspect_git_project(path: &Path) -> Result<Option<GitProjectProbe>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args([
            "rev-parse",
            "--show-toplevel",
            "--git-dir",
            "--git-common-dir",
        ])
        .output()
        .with_context(|| format!("failed to inspect git metadata for {}", path.display()))?;

    if !output.status.success() {
        return Ok(None);
    }

    let stdout =
        String::from_utf8(output.stdout).context("git rev-parse stdout was not valid UTF-8")?;
    let mut lines = stdout.lines();
    let cwd = lines
        .next()
        .context("git rev-parse missing --show-toplevel output")?
        .trim()
        .to_string();
    let git_dir = lines
        .next()
        .context("git rev-parse missing --git-dir output")?
        .trim()
        .to_string();
    let common_dir = lines
        .next()
        .context("git rev-parse missing --git-common-dir output")?
        .trim()
        .to_string();

    let cwd_path = PathBuf::from(&cwd);
    let git_dir = resolve_git_path(&cwd_path, &git_dir);
    let common_dir = resolve_git_path(&cwd_path, &common_dir);
    let root_cwd = common_dir
        .parent()
        .context("git common dir did not have a parent directory")?
        .to_path_buf();
    let root_name = root_cwd
        .file_name()
        .context("git root did not have a final path component")?
        .to_string_lossy()
        .into_owned();
    let cwd_name = cwd_path
        .file_name()
        .context("project path did not have a final path component")?
        .to_string_lossy()
        .into_owned();
    let status = read_project_status(path)?;
    let is_root = git_dir == common_dir;
    let name = if is_root || matches!(status.branch.as_str(), "DETACHED" | "N/A") {
        cwd_name
    } else {
        status.branch.clone()
    };

    Ok(Some(GitProjectProbe {
        name,
        cwd,
        branch: status.branch,
        status_summary: status.status_summary,
        root_name,
        root_cwd: root_cwd.to_string_lossy().into_owned(),
        common_dir: common_dir.to_string_lossy().into_owned(),
        is_root,
    }))
}

fn resolve_git_path(base: &Path, git_path: &str) -> PathBuf {
    let path = Path::new(git_path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

pub(crate) fn read_branch_name(path: &Path) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["branch", "--show-current"])
        .output()
        .with_context(|| format!("failed to read branch for {}", path.display()))?;

    if !output.status.success() {
        return Ok("N/A".to_string());
    }

    let branch = String::from_utf8(output.stdout)
        .context("git branch stdout was not valid UTF-8")?
        .trim()
        .to_string();
    if branch.is_empty() {
        Ok("DETACHED".to_string())
    } else {
        Ok(branch)
    }
}

pub(crate) fn read_project_status(path: &Path) -> Result<GitStatusProbe> {
    let output = Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(path)
        .args(["status", "--porcelain=v2", "--branch"])
        .output()
        .with_context(|| format!("failed to read git status for {}", path.display()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "git status failed for {}: {}",
            path.display(),
            stderr.trim()
        );
    }

    let stdout =
        String::from_utf8(output.stdout).context("git status stdout was not valid UTF-8")?;
    parse_project_status_output(&stdout)
}

pub(crate) fn run_git_pull(cwd: &str) -> Result<()> {
    let output = Command::new("git")
        .args(["-C", cwd, "pull", "--ff-only"])
        .output()
        .with_context(|| format!("failed to pull updates in {cwd}"))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!("git pull failed: {}", stderr.trim())
}

pub(crate) fn run_git_switch(cwd: &str, branch: &str) -> Result<()> {
    if git_command(cwd, &["switch", branch]).status.success() {
        return Ok(());
    }

    if branch.contains('/') {
        let local_branch = branch.rsplit('/').next().unwrap_or(branch);
        let output = git_command(cwd, &["switch", "--track", "-c", local_branch, branch]);
        if output.status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git switch failed: {}", stderr.trim())
    }

    let output = git_command(cwd, &["switch", branch]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!("git switch failed: {}", stderr.trim())
}

pub(crate) fn run_git_worktree_add(root_cwd: &str, branch: &str) -> Result<()> {
    let worktree_cwd = generated_worktree_path(root_cwd, branch)?;
    let output = git_command(root_cwd, &["worktree", "add", "-b", branch, &worktree_cwd]);

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!("git worktree add failed: {}", stderr.trim())
}

pub(crate) fn run_git_merge(root_cwd: &str, branch: &str, target: Option<&str>) -> Result<()> {
    let target = target
        .map(str::to_string)
        .map(Ok)
        .unwrap_or_else(|| resolve_default_branch(root_cwd))?;

    let switch_output = git_command(root_cwd, &["switch", &target]);
    if !switch_output.status.success() {
        let stderr = String::from_utf8_lossy(&switch_output.stderr);
        bail!("git switch failed: {}", stderr.trim());
    }

    let merge_output = git_command(root_cwd, &["merge", "--ff-only", branch]);
    if merge_output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&merge_output.stderr);
    bail!("git merge failed: {}", stderr.trim())
}

pub(crate) fn run_git_push(cwd: &str, branch: &str) -> Result<()> {
    let output = git_command(cwd, &["push", "-u", "origin", branch]);
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!("git push failed: {}", stderr.trim())
}

pub(crate) fn run_gh_pr_create(cwd: &str, branch: &str, target: &str) -> Result<()> {
    let output = Command::new("gh")
        .args(["pr", "create", "--base", target, "--head", branch, "--fill"])
        .current_dir(cwd)
        .output()
        .with_context(|| format!("failed to create pull request from {cwd}"))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!("gh pr create failed: {}", stderr.trim())
}

pub(crate) fn run_git_worktree_remove(root_cwd: &str, target_cwd: &str) -> Result<()> {
    let output = Command::new("git")
        .args(["-C", root_cwd, "worktree", "remove", target_cwd])
        .output()
        .with_context(|| format!("failed to remove worktree {target_cwd}"))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!("git worktree remove failed: {}", stderr.trim())
}

pub(crate) fn run_git_branch_delete(root_cwd: &str, branch: &str) -> Result<()> {
    let output = Command::new("git")
        .args(["-C", root_cwd, "branch", "-d", branch])
        .output()
        .with_context(|| format!("failed to delete branch {branch} from {root_cwd}"))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!("git branch -d failed: {}", stderr.trim())
}

pub(crate) fn resolve_default_branch(root_cwd: &str) -> Result<String> {
    let output = git_command(
        root_cwd,
        &["symbolic-ref", "refs/remotes/origin/HEAD", "--short"],
    );
    if output.status.success() {
        let symbolic_ref = String::from_utf8(output.stdout)
            .context("git symbolic-ref stdout was not valid UTF-8")?
            .trim()
            .to_string();
        if let Some(branch) = symbolic_ref.strip_prefix("origin/") {
            return Ok(branch.to_string());
        }
    }

    for candidate in ["main", "master"] {
        if local_branch_exists(root_cwd, candidate) {
            return Ok(candidate.to_string());
        }
    }

    let branch = read_branch_name(Path::new(root_cwd))?;
    if matches!(branch.as_str(), "" | "N/A" | "DETACHED") {
        return Ok("main".to_string());
    }

    Ok(branch)
}

fn git_command(cwd: &str, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("failed to run git {:?} in {}: {}", args, cwd, error))
}

fn generated_worktree_path(root_cwd: &str, branch: &str) -> Result<String> {
    let root_path = Path::new(root_cwd);
    let parent = root_path
        .parent()
        .ok_or_else(|| anyhow!("{root_cwd} has no parent directory"))?;
    let root_name = root_path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow!("{root_cwd} does not have a valid final path component"))?;
    let branch_suffix = branch.replace('/', "-");

    Ok(parent
        .join(format!("{root_name}.{branch_suffix}"))
        .to_string_lossy()
        .into_owned())
}

fn local_branch_exists(root_cwd: &str, branch: &str) -> bool {
    git_command(
        root_cwd,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .status
    .success()
}

pub(crate) fn parse_project_status_output(stdout: &str) -> Result<GitStatusProbe> {
    let mut branch = "N/A".to_string();
    let mut status_summary = ProjectStatusSummary::default();

    for line in stdout.lines() {
        if let Some(head) = line.strip_prefix("# branch.head ") {
            branch = if head == "(detached)" {
                "DETACHED".to_string()
            } else {
                head.to_string()
            };
            continue;
        }

        if let Some(counts) = line.strip_prefix("# branch.ab +") {
            let (ahead, behind) = counts
                .split_once(" -")
                .context("git status branch.ab header was malformed")?;
            status_summary.ahead = ahead
                .parse()
                .context("git status ahead count was not a number")?;
            status_summary.behind = behind
                .parse()
                .context("git status behind count was not a number")?;
            continue;
        }

        if line.starts_with("u ") {
            status_summary.conflicts += 1;
            continue;
        }

        if line.starts_with("? ") {
            status_summary.untracked += 1;
            continue;
        }

        if !matches!(line.as_bytes().first(), Some(b'1' | b'2')) {
            continue;
        }

        let xy = line
            .split_whitespace()
            .nth(1)
            .context("git status record missing XY field")?;
        let mut xy_chars = xy.chars();
        let staged_status = xy_chars
            .next()
            .context("git status XY field was missing staged status")?;
        let worktree_status = xy_chars
            .next()
            .context("git status XY field was missing worktree status")?;
        if xy_chars.next().is_some() {
            bail!("git status XY field was longer than expected");
        }

        if staged_status != '.' {
            status_summary.staged += 1;
        }
        if staged_status == 'D' || worktree_status == 'D' {
            status_summary.deleted += 1;
        }
        if worktree_status != '.' && worktree_status != 'D' {
            status_summary.modified += 1;
        }
    }

    Ok(GitStatusProbe {
        branch,
        status_summary,
    })
}

pub(crate) fn build_project_entries(probes: Vec<GitProjectProbe>) -> Vec<ProjectEntry> {
    let mut groups = BTreeMap::<String, Vec<GitProjectProbe>>::new();
    for probe in probes {
        groups
            .entry(probe.common_dir.clone())
            .or_default()
            .push(probe);
    }

    let mut groups = groups.into_values().collect::<Vec<_>>();
    groups.sort_by(|left, right| left[0].root_name.cmp(&right[0].root_name));

    let mut projects = Vec::new();
    for mut group in groups {
        group.sort_by(|left, right| match (left.is_root, right.is_root) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => left.name.cmp(&right.name),
        });

        for probe in group {
            projects.push(ProjectEntry {
                name: probe.name,
                cwd: probe.cwd,
                branch: probe.branch,
                status_summary: probe.status_summary,
                root_name: probe.root_name,
                root_cwd: probe.root_cwd,
                kind: if probe.is_root {
                    ProjectKind::Root
                } else {
                    ProjectKind::Worktree
                },
            });
        }
    }

    projects
}

pub(crate) fn normalize_pane_cwd(cwd: &str) -> Option<String> {
    let cwd = cwd.strip_prefix("file://").unwrap_or(cwd);
    (!cwd.is_empty()).then(|| cwd.trim_end_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use super::resolve_default_branch;
    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn resolve_default_branch_prefers_local_main_or_master_before_current_branch() {
        let sandbox = test_dir("default-branch-local");
        let repo = sandbox.join("repo");

        git(
            &sandbox,
            &["init", "--initial-branch=master", path_as_str(&repo)],
        );
        fs::write(repo.join("tracked.txt"), "hello\n").expect("tracked file should be written");
        git_commit_all(&repo, "init");
        git(&repo, &["switch", "-c", "feature/test"]);

        let branch =
            resolve_default_branch(path_as_str(&repo)).expect("default branch should resolve");

        assert_eq!(branch, "master");
    }

    #[test]
    fn resolve_default_branch_prefers_remote_head_when_available() {
        let sandbox = test_dir("default-branch-remote-head");
        let remote = sandbox.join("remote.git");
        let repo = sandbox.join("repo");

        git(&sandbox, &["init", "--bare", path_as_str(&remote)]);
        git(
            &sandbox,
            &["clone", path_as_str(&remote), path_as_str(&repo)],
        );
        git(&repo, &["switch", "-c", "main"]);
        fs::write(repo.join("tracked.txt"), "hello\n").expect("tracked file should be written");
        git_commit_all(&repo, "init");
        git(&repo, &["push", "-u", "origin", "main"]);
        git(&repo, &["switch", "-c", "feature/test"]);

        let branch =
            resolve_default_branch(path_as_str(&repo)).expect("default branch should resolve");

        assert_eq!(branch, "main");
    }

    #[test]
    fn resolve_default_branch_falls_back_to_current_branch_when_main_and_master_are_missing() {
        let sandbox = test_dir("default-branch-fallback");
        let repo = sandbox.join("repo");

        git(
            &sandbox,
            &["init", "--initial-branch=trunk", path_as_str(&repo)],
        );
        fs::write(repo.join("tracked.txt"), "hello\n").expect("tracked file should be written");
        git_commit_all(&repo, "init");
        git(&repo, &["switch", "-c", "feature/test"]);

        let branch =
            resolve_default_branch(path_as_str(&repo)).expect("default branch should resolve");

        assert_eq!(branch, "feature/test");
    }

    fn test_dir(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        let path = env::temp_dir().join(format!("nerve-center-projects-{label}-{unique}"));
        fs::create_dir_all(&path).expect("test dir should be created");
        path
    }

    fn git(workdir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(workdir)
            .output()
            .expect("git command should run");

        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_commit_all(repo: &Path, message: &str) {
        git(
            repo,
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
            repo,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                message,
            ],
        );
    }

    fn path_as_str(path: &Path) -> &str {
        path.to_str().expect("path should be valid utf-8")
    }
}
