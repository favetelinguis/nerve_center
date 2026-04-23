use std::path::Path;

use crate::projects::{ProjectEntry, ProjectKind};

pub fn root_project_id(root_cwd: &str) -> String {
    stable_project_path(root_cwd)
}

pub fn project_id(project: &ProjectEntry) -> String {
    match project.kind {
        ProjectKind::Root => root_project_id(&project.root_cwd),
        ProjectKind::Worktree => stable_project_path(&project.cwd),
    }
}

fn stable_project_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    let normalized = if trimmed.is_empty() { path } else { trimmed };
    let candidate = Path::new(normalized);

    match candidate.canonicalize() {
        Ok(path) => path.to_string_lossy().into_owned(),
        Err(_) => candidate.to_string_lossy().into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{project_id, root_project_id};
    use crate::projects::{ProjectEntry, ProjectKind, ProjectStatusSummary};

    #[test]
    fn derives_unique_ids_from_project_paths() {
        let first = ProjectEntry {
            name: "alpha".to_string(),
            cwd: "/repos/source-a/alpha".to_string(),
            branch: "main".to_string(),
            status_summary: ProjectStatusSummary::default(),
            root_name: "alpha".to_string(),
            root_cwd: "/repos/source-a/alpha".to_string(),
            kind: ProjectKind::Root,
        };
        let second = ProjectEntry {
            name: "alpha".to_string(),
            cwd: "/repos/source-b/alpha".to_string(),
            branch: "main".to_string(),
            status_summary: ProjectStatusSummary::default(),
            root_name: "alpha".to_string(),
            root_cwd: "/repos/source-b/alpha".to_string(),
            kind: ProjectKind::Root,
        };

        assert_eq!(
            root_project_id("/repos/source-a/alpha"),
            "/repos/source-a/alpha"
        );
        assert_ne!(project_id(&first), project_id(&second));
    }
}
