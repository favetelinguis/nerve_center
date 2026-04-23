use std::env;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentEvent {
    pub project_id: String,
    pub runtime: String,
    pub pane_id: u64,
    pub state: String,
    pub awaiting_user: Option<String>,
}

impl AgentEvent {
    pub fn effective_state(&self) -> &str {
        if self.awaiting_user.is_some() {
            "needs_input"
        } else {
            self.state.as_str()
        }
    }
}

pub fn append_spool_record(event: &AgentEvent) -> Result<()> {
    let path = spool_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let mut record = serde_json::to_string(event)?;
    record.push('\n');
    let mut spool = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    use std::io::Write;
    spool
        .write_all(record.as_bytes())
        .with_context(|| format!("failed to append {}", path.display()))?;
    Ok(())
}

pub fn drain_spool() -> Result<Vec<AgentEvent>> {
    let path = spool_path()?;
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()))
        }
    };

    let mut events = Vec::new();
    for (index, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event = serde_json::from_str(line)
            .with_context(|| format!("failed to parse {} line {}", path.display(), index + 1))?;
        events.push(event);
    }

    fs::remove_file(&path).with_context(|| format!("failed to remove {}", path.display()))?;
    Ok(events)
}

fn spool_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("agent-spool.jsonl"))
}

fn data_dir() -> Result<PathBuf> {
    if let Ok(path) = env::var("NERVE_CENTER_DATA_DIR") {
        return Ok(PathBuf::from(path));
    }

    let home = env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".local/data/nerve_center"))
}

#[cfg(test)]
mod tests {
    use super::{append_spool_record, drain_spool, AgentEvent};
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn test_dir(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        let path = env::temp_dir().join(format!("nerve-center-agent-{label}-{unique}"));
        fs::create_dir_all(&path).expect("test dir should be created");
        path
    }

    #[test]
    fn agent_event_prefers_needs_input_when_awaiting_user_is_present() {
        let event = AgentEvent {
            project_id: "root:alpha".to_string(),
            runtime: "opencode".to_string(),
            pane_id: 55,
            state: "working".to_string(),
            awaiting_user: Some("question".to_string()),
        };

        assert_eq!(event.effective_state(), "needs_input");
    }

    #[test]
    fn spool_round_trips_and_drains_events() {
        let _env_lock = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let data_dir = test_dir("spool-round-trip");
        let spool_path = data_dir.join("agent-spool.jsonl");
        let first = AgentEvent {
            project_id: "root:alpha".to_string(),
            runtime: "claude".to_string(),
            pane_id: 10,
            state: "working".to_string(),
            awaiting_user: None,
        };
        let second = AgentEvent {
            project_id: "root:beta".to_string(),
            runtime: "opencode".to_string(),
            pane_id: 11,
            state: "working".to_string(),
            awaiting_user: Some("question".to_string()),
        };

        unsafe {
            env::set_var("NERVE_CENTER_DATA_DIR", &data_dir);
        }

        append_spool_record(&first).expect("first spool append should succeed");
        append_spool_record(&second).expect("second spool append should succeed");
        let events = drain_spool().expect("spool drain should succeed");

        unsafe {
            env::remove_var("NERVE_CENTER_DATA_DIR");
        }

        assert_eq!(events, vec![first, second]);
        assert!(!spool_path.exists());
    }
}
