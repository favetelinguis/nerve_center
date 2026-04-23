use anyhow::bail;

use crate::command::{parse_project_command, ProjectCommand};
use crate::projects::ProjectKind;
use crate::projects::{
    resolve_default_branch, run_gh_pr_create, run_git_branch_delete, run_git_merge, run_git_pull,
    run_git_push, run_git_switch, run_git_worktree_add, run_git_worktree_remove,
};
use crate::workspace::ProjectSnapshot;

pub fn run_operation(
    root_project: &ProjectSnapshot,
    project: &ProjectSnapshot,
    operation: &str,
    command: &str,
) -> anyhow::Result<()> {
    validate_operation_scope(project, operation)?;

    match (operation, parse_project_command(command)?) {
        ("wt_add", ProjectCommand::Add { branch }) => {
            run_git_worktree_add(&root_project.cwd, &branch)
        }
        ("wt_remove", ProjectCommand::Remove) => run_remove_sequence(root_project, project),
        ("wt_merge", ProjectCommand::Merge { target }) => {
            run_git_merge(&root_project.cwd, &project.git.branch, target.as_deref())
        }
        ("wt_pr", ProjectCommand::Pr { target }) => run_pr_sequence(root_project, project, target),
        ("wt_land", ProjectCommand::Land { target }) => {
            run_land_sequence(root_project, project, target)
        }
        ("git_switch", ProjectCommand::GitSwitch { branch }) => {
            run_git_switch(&project.cwd, &branch)
        }
        ("git_pull", ProjectCommand::GitPull) => run_git_pull(&project.cwd),
        (other, _) => bail!("daemon operation {other} did not match command: {command}"),
    }
}

fn validate_operation_scope(project: &ProjectSnapshot, operation: &str) -> anyhow::Result<()> {
    match operation {
        "wt_add" | "git_switch" | "git_pull" if project.kind != ProjectKind::Root => {
            bail!("{operation} requires a root project")
        }
        "wt_remove" | "wt_merge" | "wt_pr" | "wt_land" if project.kind != ProjectKind::Worktree => {
            bail!("{operation} requires a worktree project")
        }
        _ => Ok(()),
    }
}

fn run_remove_sequence(
    root_project: &ProjectSnapshot,
    project: &ProjectSnapshot,
) -> anyhow::Result<()> {
    run_git_worktree_remove(&root_project.cwd, &project.cwd)?;
    run_git_branch_delete(&root_project.cwd, &project.git.branch)
}

fn run_land_sequence(
    root_project: &ProjectSnapshot,
    project: &ProjectSnapshot,
    target: Option<String>,
) -> anyhow::Result<()> {
    run_git_merge(&root_project.cwd, &project.git.branch, target.as_deref())?;
    run_remove_sequence(root_project, project)
}

fn run_pr_sequence(
    root_project: &ProjectSnapshot,
    project: &ProjectSnapshot,
    target: Option<String>,
) -> anyhow::Result<()> {
    let target = target.unwrap_or(resolve_default_branch(&root_project.cwd)?);
    run_git_push(&project.cwd, &project.git.branch)?;
    run_gh_pr_create(&project.cwd, &project.git.branch, &target)
}
