use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::workspace::{AgentSnapshot, ProjectSnapshot, WorkspaceSnapshot};

use super::agent::AgentEvent;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JournalOperationRecord {
    pub operation_id: String,
    pub root_id: String,
    pub phase: String,
}

pub fn restore_operation_from_journal_line(line: &str) -> anyhow::Result<JournalOperationRecord> {
    let mut record: JournalOperationRecord = serde_json::from_str(line)?;
    if record.phase == "running" {
        record.phase = "interrupted".to_string();
    }
    Ok(record)
}

#[derive(Debug, Default, Clone)]
pub struct WorkspaceState {
    pub projects: BTreeMap<String, ProjectSnapshot>,
    pub project_order: Vec<String>,
    pub warnings: Vec<String>,
    pub running_operations_by_root: BTreeMap<String, String>,
}

impl WorkspaceState {
    pub fn try_start_operation(&mut self, root_id: &str, operation_id: &str) -> anyhow::Result<()> {
        if let Some(existing) = self.running_operations_by_root.get(root_id) {
            anyhow::bail!("{root_id} already has running operation {existing}");
        }

        self.running_operations_by_root
            .insert(root_id.to_string(), operation_id.to_string());
        Ok(())
    }

    pub fn finish_operation(&mut self, root_id: &str) {
        self.running_operations_by_root.remove(root_id);
    }

    pub fn apply_agent_event(&mut self, event: AgentEvent) {
        let Some(project) = self.projects.get_mut(&event.project_id) else {
            return;
        };

        project.freshness = crate::workspace::ProjectFreshness {
            state: "fresh".to_string(),
            updated_at_ms: now_unix_ms(),
            stale_reason: None,
        };

        let status = event.effective_state().to_string();
        if let Some(agent) = project
            .agents
            .iter_mut()
            .find(|agent| agent.runtime == event.runtime && agent.pane_id == Some(event.pane_id))
        {
            agent.status = status;
            agent.pane_id = Some(event.pane_id);
            return;
        }

        project.agents.push(AgentSnapshot {
            runtime: event.runtime,
            status,
            pane_id: Some(event.pane_id),
        });
    }

    pub fn snapshot(&self, generated_at_ms: u64) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            protocol_version: 1,
            generated_at_ms,
            projects: self.projects.clone(),
            project_order: self.project_order.clone(),
            warnings: self.warnings.clone(),
        }
    }

    pub fn root_ids(&self) -> Vec<String> {
        self.projects
            .values()
            .filter(|project| project.id == project.root_id)
            .map(|project| project.id.clone())
            .collect()
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::{restore_operation_from_journal_line, WorkspaceState};
    use crate::daemon::agent::AgentEvent;
    use crate::workspace::{ProjectFreshness, ProjectSnapshot};

    #[test]
    fn snapshot_preserves_project_order() {
        let mut state = WorkspaceState::default();
        state.project_order = vec!["root:alpha".to_string(), "root:beta".to_string()];
        state.projects.insert(
            "root:alpha".to_string(),
            ProjectSnapshot {
                id: "root:alpha".to_string(),
                ..ProjectSnapshot::default()
            },
        );
        state.projects.insert(
            "root:beta".to_string(),
            ProjectSnapshot {
                id: "root:beta".to_string(),
                ..ProjectSnapshot::default()
            },
        );

        let snapshot = state.snapshot(99);
        assert_eq!(snapshot.project_order, vec!["root:alpha", "root:beta"]);
    }

    #[test]
    fn rejects_second_mutating_operation_for_same_root() {
        let mut state = WorkspaceState::default();

        assert!(state.try_start_operation("root:alpha", "op-1").is_ok());
        let error = state.try_start_operation("root:alpha", "op-2").unwrap_err();
        assert!(error
            .to_string()
            .contains("root:alpha already has running operation"));
    }

    #[test]
    fn restores_interrupted_operation_as_interrupted_state() {
        let journal = r#"{"operation_id":"op-9","root_id":"root:alpha","phase":"running"}"#;
        let restored = restore_operation_from_journal_line(journal).unwrap();
        assert_eq!(restored.phase, "interrupted");
    }

    #[test]
    fn agent_events_refresh_project_freshness() {
        let mut state = WorkspaceState::default();
        state.projects.insert(
            "root:alpha".to_string(),
            ProjectSnapshot {
                id: "root:alpha".to_string(),
                freshness: ProjectFreshness {
                    state: "stale".to_string(),
                    updated_at_ms: 1,
                    stale_reason: Some("refresh_pending".to_string()),
                },
                ..ProjectSnapshot::default()
            },
        );

        state.apply_agent_event(AgentEvent {
            project_id: "root:alpha".to_string(),
            runtime: "opencode".to_string(),
            pane_id: 44,
            state: "working".to_string(),
            awaiting_user: Some("question".to_string()),
        });

        let project = &state.projects["root:alpha"];
        assert_eq!(project.freshness.state, "fresh");
        assert!(project.freshness.updated_at_ms > 1);
        assert_eq!(project.freshness.stale_reason, None);
    }
}
