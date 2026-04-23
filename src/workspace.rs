use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::projects::{ProjectKind, ProjectStatusSummary};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct WorkspaceSnapshot {
    pub protocol_version: u32,
    pub generated_at_ms: u64,
    pub projects: BTreeMap<String, ProjectSnapshot>,
    pub project_order: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ProjectSnapshot {
    pub id: String,
    pub name: String,
    pub cwd: String,
    pub root_id: String,
    pub kind: ProjectKind,
    pub git: ProjectGitState,
    pub agents: Vec<AgentSnapshot>,
    pub operation: ProjectOperationState,
    pub freshness: ProjectFreshness,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ProjectGitState {
    pub branch: String,
    pub status: String,
    pub status_summary: ProjectStatusSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct AgentSnapshot {
    pub runtime: String,
    pub status: String,
    pub pane_id: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ProjectOperationState {
    pub kind: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ProjectFreshness {
    pub state: String,
    pub updated_at_ms: u64,
    #[serde(default)]
    pub stale_reason: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::{ProjectFreshness, ProjectGitState, ProjectSnapshot, WorkspaceSnapshot};
    use std::collections::BTreeMap;

    #[test]
    fn workspace_snapshot_round_trips_through_json() {
        let mut projects = BTreeMap::new();
        projects.insert(
            "alpha".to_string(),
            ProjectSnapshot {
                id: "alpha".to_string(),
                name: "alpha".to_string(),
                cwd: "/tmp/alpha".to_string(),
                root_id: "alpha".to_string(),
                git: ProjectGitState::default(),
                freshness: ProjectFreshness::default(),
                ..ProjectSnapshot::default()
            },
        );

        let snapshot = WorkspaceSnapshot {
            protocol_version: 1,
            generated_at_ms: 42,
            projects,
            project_order: vec!["alpha".to_string()],
            warnings: Vec::new(),
        };

        let json = serde_json::to_string(&snapshot).unwrap();
        let decoded: WorkspaceSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, snapshot);
    }
}
