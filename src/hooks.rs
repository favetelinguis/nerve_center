use std::env;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::cli::InternalCommands;

const CLAUDE_COMMAND_NAME: &str = "internal ingest-claude-hook";
const OPENCODE_SUBCOMMAND_NAME: &str = "ingest-opencode-event";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PaneAgentState {
    version: u8,
    pane_id: u64,
    source: String,
    updated_at_ms: u64,
    effective_state: String,
    main_state: String,
    subagent_count: u64,
    awaiting_user: Option<String>,
    error: Option<String>,
    last_event: String,
}

impl PaneAgentState {
    fn new(pane_id: u64, source: &str) -> Self {
        let mut state = Self {
            version: 1,
            pane_id,
            source: source.to_string(),
            updated_at_ms: now_unix_ms(),
            effective_state: "done".to_string(),
            main_state: "done".to_string(),
            subagent_count: 0,
            awaiting_user: None,
            error: None,
            last_event: String::new(),
        };
        state.recompute_effective_state();
        state
    }

    fn recompute_effective_state(&mut self) {
        self.effective_state = if self.awaiting_user.is_some() {
            "needs_input".to_string()
        } else if self.main_state == "working" || self.subagent_count > 0 {
            "working".to_string()
        } else if self.main_state == "error" {
            "error".to_string()
        } else {
            "done".to_string()
        };
    }

    fn touch(&mut self, event_name: &str) {
        self.updated_at_ms = now_unix_ms();
        self.last_event = event_name.to_string();
        self.recompute_effective_state();
    }
}

pub fn install_claude_hooks() -> Result<()> {
    let binary = current_executable_string()?;
    let settings_path = claude_settings_path()?;
    install_claude_hooks_at(&settings_path, &binary)
}

fn install_claude_hooks_at(settings_path: &Path, binary: &str) -> Result<()> {
    ensure_parent_dir(&settings_path)?;

    let mut root = read_json_object_or_empty(&settings_path)?;
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Default::default()));
    let hook_command = format!("\"{binary}\" {CLAUDE_COMMAND_NAME}");

    ensure_claude_event_hook(hooks, "UserPromptSubmit", None, &hook_command)?;
    ensure_claude_event_hook(hooks, "PreToolUse", None, &hook_command)?;
    ensure_claude_event_hook(hooks, "Stop", None, &hook_command)?;
    ensure_claude_event_hook(hooks, "StopFailure", None, &hook_command)?;
    ensure_claude_event_hook(hooks, "SubagentStart", None, &hook_command)?;
    ensure_claude_event_hook(hooks, "SubagentStop", None, &hook_command)?;
    ensure_claude_event_hook(
        hooks,
        "Notification",
        Some("permission_prompt|elicitation_dialog"),
        &hook_command,
    )?;

    write_json_pretty(settings_path, &root.into())?;
    println!("Installed Claude hooks into {}", settings_path.display());
    Ok(())
}

pub fn install_opencode_hooks() -> Result<()> {
    let binary = current_executable_string()?;
    let plugin_path = opencode_plugin_path()?;
    install_opencode_hooks_at(&plugin_path, &binary)
}

fn install_opencode_hooks_at(plugin_path: &Path, binary: &str) -> Result<()> {
    ensure_parent_dir(&plugin_path)?;

    let plugin = r#"const BINARY = __BINARY__;
async function sendState(payload) {
  try {
    const proc = Bun.spawn([BINARY, "internal", "__OPENCODE_COMMAND__"], {
      stdin: new Blob([JSON.stringify(payload)]),
      stdout: "ignore",
      stderr: "ignore",
    })
    await proc.exited
  } catch (_error) {
  }
}

export const NerveCenterPlugin = async (_ctx) => {
  return {
    event: async ({ event }) => {
      if (!event || !event.type) {
        return
      }
      switch (event.type) {
        case "session.status": {
          const status = event.properties?.status?.type
          if (status === "busy" || status === "retry") {
            await sendState({ runtime: "opencode", state: "working", event_type: event.type })
          } else if (status === "idle") {
            await sendState({ runtime: "opencode", state: "done", event_type: event.type })
          }
          break
        }
        case "session.idle":
          await sendState({ runtime: "opencode", state: "done", event_type: event.type })
          break
        case "session.error":
          await sendState({ runtime: "opencode", state: "error", event_type: event.type })
          break
        case "permission.updated":
        case "permission.asked":
          await sendState({ runtime: "opencode", state: "needs_input", event_type: event.type })
          break
        case "permission.replied":
          await sendState({ runtime: "opencode", state: "working", event_type: event.type })
          break
        default:
          break
      }
    },
  }
}
"#
    .replace("__BINARY__", &serde_json::to_string(binary)?)
    .replace("__OPENCODE_COMMAND__", OPENCODE_SUBCOMMAND_NAME);

    fs::write(plugin_path, plugin)
        .with_context(|| format!("failed to write {}", plugin_path.display()))?;
    println!("Installed OpenCode plugin into {}", plugin_path.display());
    Ok(())
}

pub fn run_internal(command: InternalCommands) -> Result<()> {
    match command {
        InternalCommands::IngestClaudeHook => ingest_claude_hook(),
        InternalCommands::IngestOpencodeEvent => ingest_opencode_event(),
    }
}

fn ingest_claude_hook() -> Result<()> {
    let pane_id = pane_id_from_env()?;
    let payload = read_stdin_json()?;
    let event_name = payload
        .get("hook_event_name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Claude hook payload missing hook_event_name"))?;

    update_pane_state(pane_id, "claude", |state| {
        match event_name {
            "UserPromptSubmit" | "PreToolUse" => {
                state.main_state = "working".to_string();
                state.awaiting_user = None;
                state.error = None;
            }
            "Stop" => {
                state.main_state = "done".to_string();
                state.subagent_count = 0;
                state.awaiting_user = None;
                state.error = None;
            }
            "StopFailure" => {
                state.main_state = "error".to_string();
                state.subagent_count = 0;
                state.awaiting_user = None;
                state.error = payload
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| Some("claude stop failure".to_string()));
            }
            "SubagentStart" => {
                state.subagent_count = state.subagent_count.saturating_add(1);
                state.awaiting_user = None;
            }
            "SubagentStop" => {
                state.subagent_count = state.subagent_count.saturating_sub(1);
            }
            "Notification" => {
                let notification_type = payload
                    .get("notification_type")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if matches!(
                    notification_type,
                    "permission_prompt" | "elicitation_dialog"
                ) {
                    state.awaiting_user = Some(notification_type.to_string());
                }
            }
            _ => {}
        }
        state.touch(event_name);
    })
}

fn ingest_opencode_event() -> Result<()> {
    let pane_id = pane_id_from_env()?;
    let payload = read_stdin_json()?;
    let state_name = payload
        .get("state")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("OpenCode payload missing state"))?;
    let event_type = payload
        .get("event_type")
        .and_then(Value::as_str)
        .unwrap_or("opencode");

    update_pane_state(pane_id, "opencode", |state| {
        match state_name {
            "working" => {
                state.main_state = "working".to_string();
                state.awaiting_user = None;
                state.error = None;
            }
            "needs_input" => {
                state.awaiting_user = Some("permission".to_string());
            }
            "done" => {
                state.main_state = "done".to_string();
                state.awaiting_user = None;
                state.error = None;
                state.subagent_count = 0;
            }
            "error" => {
                state.main_state = "error".to_string();
                state.awaiting_user = None;
                state.error = Some(event_type.to_string());
                state.subagent_count = 0;
            }
            _ => {}
        }
        state.touch(event_type);
    })
}

fn update_pane_state<F>(pane_id: u64, source: &str, mutate: F) -> Result<()>
where
    F: FnOnce(&mut PaneAgentState),
{
    let path = state_file_path(pane_id)?;
    update_pane_state_at_path(&path, pane_id, source, mutate)
}

fn update_pane_state_at_path<F>(path: &Path, pane_id: u64, source: &str, mutate: F) -> Result<()>
where
    F: FnOnce(&mut PaneAgentState),
{
    ensure_parent_dir(&path)?;

    let mut state = read_pane_state(&path)?.unwrap_or_else(|| PaneAgentState::new(pane_id, source));
    if state.source != source {
        state = PaneAgentState::new(pane_id, source);
    }
    mutate(&mut state);

    let tmp_path = path.with_extension("tmp");
    let content = serde_json::to_vec_pretty(&state).context("failed to serialize pane state")?;
    fs::write(&tmp_path, content)
        .with_context(|| format!("failed to write {}", tmp_path.display()))?;
    fs::rename(&tmp_path, &path).with_context(|| format!("failed to update {}", path.display()))?;
    Ok(())
}

fn read_pane_state(path: &Path) -> Result<Option<PaneAgentState>> {
    if !path.exists() {
        return Ok(None);
    }
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let state = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(Some(state))
}

fn state_file_path(pane_id: u64) -> Result<PathBuf> {
    Ok(agent_state_dir()?.join(pane_id.to_string()))
}

fn agent_state_dir() -> Result<PathBuf> {
    if let Ok(path) = env::var("NERVE_CENTER_DATA_DIR") {
        return Ok(PathBuf::from(path));
    }
    let home = env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".local/data/nerve_center"))
}

fn pane_id_from_env() -> Result<u64> {
    let value = env::var("WEZTERM_PANE").context("WEZTERM_PANE is not set")?;
    value
        .parse::<u64>()
        .with_context(|| format!("WEZTERM_PANE is not a valid pane id: {value}"))
}

fn claude_settings_path() -> Result<PathBuf> {
    let home = env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".claude/settings.json"))
}

fn opencode_plugin_path() -> Result<PathBuf> {
    let home = env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config/opencode/plugins/nerve_center.js"))
}

fn current_executable_string() -> Result<String> {
    let exe = env::current_exe().context("failed to resolve current executable")?;
    exe.to_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("current executable path is not valid UTF-8"))
}

fn read_stdin_json() -> Result<Value> {
    let mut input = String::new();
    io::stdin()
        .read_to_string(&mut input)
        .context("failed to read stdin")?;
    if input.trim().is_empty() {
        bail!("stdin is empty")
    }
    serde_json::from_str(&input).context("failed to parse stdin JSON")
}

fn ensure_parent_dir(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    Ok(())
}

fn read_json_object_or_empty(path: &Path) -> Result<serde_json::Map<String, Value>> {
    if !path.exists() {
        return Ok(Default::default());
    }

    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    match serde_json::from_str::<Value>(&content)? {
        Value::Object(map) => Ok(map),
        _ => bail!(
            "{} does not contain a top-level JSON object",
            path.display()
        ),
    }
}

fn ensure_claude_event_hook(
    hooks_value: &mut Value,
    event_name: &str,
    matcher: Option<&str>,
    command: &str,
) -> Result<()> {
    let hooks = hooks_value
        .as_object_mut()
        .ok_or_else(|| anyhow!("Claude hooks config is not a JSON object"))?;
    let entry = hooks
        .entry(event_name.to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    let groups = entry
        .as_array_mut()
        .ok_or_else(|| anyhow!("Claude hook event {event_name} is not an array"))?;

    for group in groups.iter() {
        if group_matches(group, matcher) && group_contains_command(group, command) {
            return Ok(());
        }
    }

    let mut group = serde_json::Map::new();
    if let Some(matcher) = matcher {
        group.insert("matcher".to_string(), Value::String(matcher.to_string()));
    }
    group.insert(
        "hooks".to_string(),
        Value::Array(vec![json!({
            "type": "command",
            "command": command,
        })]),
    );
    groups.push(Value::Object(group));
    Ok(())
}

fn group_matches(group: &Value, matcher: Option<&str>) -> bool {
    match (group.get("matcher").and_then(Value::as_str), matcher) {
        (None, None) => true,
        (Some(existing), Some(expected)) => existing == expected,
        _ => false,
    }
}

fn group_contains_command(group: &Value, command: &str) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|hook| hook.get("command").and_then(Value::as_str) == Some(command))
}

fn write_json_pretty(path: &Path, value: &Value) -> Result<()> {
    let content = serde_json::to_vec_pretty(value).context("failed to serialize JSON")?;
    fs::write(path, content).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let unique = now_unix_ms();
        let dir = env::temp_dir().join(format!("nerve-center-hooks-{name}-{unique}"));
        fs::create_dir_all(&dir).expect("test dir should be created");
        dir
    }

    #[test]
    fn claude_installer_writes_expected_events() {
        let home = test_dir("claude-install");
        let settings_path = home.join(".claude/settings.json");

        install_claude_hooks_at(&settings_path, "/tmp/nerve_center")
            .expect("claude install should succeed");

        let settings = fs::read_to_string(settings_path).expect("settings should be readable");
        assert!(settings.contains("UserPromptSubmit"));
        assert!(settings.contains("Notification"));
        assert!(settings.contains(CLAUDE_COMMAND_NAME));
    }

    #[test]
    fn opencode_installer_writes_plugin() {
        let home = test_dir("opencode-install");
        let plugin_path = home.join(".config/opencode/plugins/nerve_center.js");

        install_opencode_hooks_at(&plugin_path, "/tmp/nerve_center")
            .expect("opencode install should succeed");

        let plugin = fs::read_to_string(plugin_path).expect("plugin should be readable");
        assert!(plugin.contains(OPENCODE_SUBCOMMAND_NAME));
        assert!(plugin.contains("permission.replied"));
    }

    #[test]
    fn opencode_state_update_writes_pane_file() {
        let data_dir = test_dir("opencode-state");
        let state_path = data_dir.join("42");

        update_pane_state_at_path(&state_path, 42, "opencode", |state| {
            state.main_state = "working".to_string();
            state.touch("session.status");
        })
        .expect("state write should succeed");

        let state = fs::read_to_string(state_path).expect("pane file should exist");
        assert!(state.contains("\"source\": \"opencode\""));
        assert!(state.contains("\"effective_state\": \"working\""));
    }
}
