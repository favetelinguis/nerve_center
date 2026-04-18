use std::env;
use std::fs;
use std::io::ErrorKind;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::cli::InternalCommands;

const CLAUDE_COMMAND_NAME: &str = "internal ingest-claude-hook";
const OPENCODE_SUBCOMMAND_NAME: &str = "ingest-opencode-event";
const PI_SUBCOMMAND_NAME: &str = "ingest-pi-event";
const LOCK_STALE_AFTER_MS: u64 = 30_000;
const LOCK_RETRY_DELAY_MS: u64 = 10;
const LOCK_RETRY_LIMIT: usize = 500;

static TMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

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
    let settings_path = claude_settings_path()?;
    let binary = current_executable_string()?;
    install_claude_hooks_at(&settings_path, &binary)
}

fn install_claude_hooks_at(settings_path: &Path, binary: &str) -> Result<()> {
    ensure_parent_dir(&settings_path)?;

    let mut root = read_json_object_or_empty(&settings_path)?;
    remove_installed_claude_hooks(&mut root)?;
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Default::default()));
    let hook_command = format!("\"{binary}\" {CLAUDE_COMMAND_NAME}");

    ensure_claude_event_hook(hooks, "UserPromptSubmit", None, &hook_command)?;
    ensure_claude_event_hook(hooks, "Stop", None, &hook_command)?;
    ensure_claude_event_hook(hooks, "StopFailure", None, &hook_command)?;
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

pub fn remove_claude_hooks() -> Result<()> {
    let settings_path = claude_settings_path()?;
    remove_claude_hooks_at(&settings_path)
}

fn remove_claude_hooks_at(settings_path: &Path) -> Result<()> {
    if !settings_path.exists() {
        println!("No Claude hooks found at {}", settings_path.display());
        return Ok(());
    }

    let mut root = read_json_object_or_empty(settings_path)?;
    remove_installed_claude_hooks(&mut root)?;
    write_json_pretty(settings_path, &root.into())?;
    println!("Removed Claude hooks from {}", settings_path.display());
    Ok(())
}

pub fn install_opencode_hooks() -> Result<()> {
    let plugin_path = opencode_plugin_path()?;
    let binary = current_executable_string()?;
    install_opencode_hooks_at(&plugin_path, &binary)
}

fn install_opencode_hooks_at(plugin_path: &Path, binary: &str) -> Result<()> {
    remove_opencode_hooks_at(plugin_path)?;
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

function normalizeToolName(tool) {
  return typeof tool === "string" ? tool.toLowerCase().replace(/[_-]/g, "") : ""
}

function isQuestionTool(tool, input) {
  const toolName = normalizeToolName(tool)
  return toolName === "question" || toolName === "askuserquestion" || hasQuestionsInput(input)
}

function hasQuestionsInput(input) {
  return Array.isArray(input?.questions) && input.questions.length > 0
}

function questionToolState(part) {
  if (!part || part.type !== "tool") {
    return null
  }

  const toolState = part.state
  const looksLikeQuestionTool = isQuestionTool(part.tool, toolState?.input)

  if (!looksLikeQuestionTool || !toolState || typeof toolState.status !== "string") {
    return null
  }

  if (toolState.status === "pending") {
    return "needs_input"
  }

  if (
    toolState.status === "running" ||
    toolState.status === "completed" ||
    toolState.status === "error"
  ) {
    return "working"
  }

  return null
}

export const NerveCenterPlugin = async (_ctx) => {
  return {
    "tool.execute.before": async (input) => {
      if (!isQuestionTool(input?.tool, input?.args)) {
        return
      }

      await sendState({
        runtime: "opencode",
        state: "needs_input",
        event_type: "tool.execute.before",
        awaiting_user: "question",
      })
    },
    event: async ({ event }) => {
      if (!event || !event.type) {
        return
      }
        switch (event.type) {
        case "session.status": {
          const status = event.properties?.status?.type
          if (status === "busy" || status === "retry") {
            await sendState({ runtime: "opencode", state: "working", event_type: event.type })
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
          await sendState({
            runtime: "opencode",
            state: "needs_input",
            event_type: event.type,
            awaiting_user: "permission",
          })
          break
        case "question.asked":
          await sendState({
            runtime: "opencode",
            state: "needs_input",
            event_type: event.type,
            awaiting_user: "question",
          })
          break
        case "permission.replied":
          await sendState({ runtime: "opencode", state: "working", event_type: event.type })
          break
        case "message.part.updated": {
          const state = questionToolState(event.properties?.part)
          if (state) {
            await sendState({
              runtime: "opencode",
              state,
              event_type: event.type,
              awaiting_user: state === "needs_input" ? "question" : null,
            })
          }
          break
        }
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

pub fn remove_opencode_hooks() -> Result<()> {
    let plugin_path = opencode_plugin_path()?;
    remove_opencode_hooks_at(&plugin_path)
}

fn remove_opencode_hooks_at(plugin_path: &Path) -> Result<()> {
    match fs::remove_file(plugin_path) {
        Ok(()) => {
            println!("Removed OpenCode plugin from {}", plugin_path.display());
            Ok(())
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            println!("No OpenCode plugin found at {}", plugin_path.display());
            Ok(())
        }
        Err(error) => {
            Err(error).with_context(|| format!("failed to remove {}", plugin_path.display()))
        }
    }
}

pub fn install_pi_hooks() -> Result<()> {
    let extension_path = pi_extension_path()?;
    let binary = current_executable_string()?;
    install_pi_hooks_at(&extension_path, &binary)
}

fn install_pi_hooks_at(extension_path: &Path, binary: &str) -> Result<()> {
    ensure_parent_dir(extension_path)?;

    let extension = r#"import { spawn } from "node:child_process";
import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";

const BINARY = __BINARY__;
const COMMAND = "__PI_COMMAND__";

async function sendState(payload) {
  if (!process.env.WEZTERM_PANE) {
    return;
  }

  try {
    await new Promise((resolve) => {
      const proc = spawn(BINARY, ["internal", COMMAND], {
        stdio: ["pipe", "ignore", "ignore"],
        env: process.env,
      });
      proc.on("error", () => resolve(undefined));
      proc.on("close", () => resolve(undefined));
      proc.stdin.end(JSON.stringify(payload));
    });
  } catch (_error) {
  }
}

export default function (pi: ExtensionAPI) {
  let sawError = false;

  pi.on("session_start", async () => {
    sawError = false;
    await sendState({ runtime: "pi", state: "done", event_type: "session_start" });
  });

  pi.on("agent_start", async () => {
    sawError = false;
    await sendState({ runtime: "pi", state: "working", event_type: "agent_start" });
  });

  pi.on("tool_execution_start", async () => {
    await sendState({ runtime: "pi", state: "working", event_type: "tool_execution_start" });
  });

  pi.on("message_end", async (event) => {
    const message = event.message;
    if (!message || message.role !== "assistant") {
      return;
    }

    const error = typeof message.errorMessage === "string" ? message.errorMessage : undefined;
    if (message.stopReason === "error" || message.stopReason === "aborted" || error) {
      sawError = true;
      await sendState({
        runtime: "pi",
        state: "error",
        event_type: "message_end",
        error: error ?? String(message.stopReason ?? "error"),
      });
    }
  });

  pi.on("agent_end", async () => {
    await sendState({
      runtime: "pi",
      state: sawError ? "error" : "done",
      event_type: "agent_end",
    });
  });

  pi.on("session_shutdown", async () => {
    sawError = false;
    await sendState({ runtime: "pi", state: "done", event_type: "session_shutdown" });
  });
}
"#
    .replace("__BINARY__", &serde_json::to_string(binary)?)
    .replace("__PI_COMMAND__", PI_SUBCOMMAND_NAME);

    fs::write(extension_path, extension)
        .with_context(|| format!("failed to write {}", extension_path.display()))?;
    println!(
        "Installed pi extension into {} (restart pi or run /reload in existing sessions)",
        extension_path.display()
    );
    Ok(())
}

pub fn remove_pi_hooks() -> Result<()> {
    let extension_path = pi_extension_path()?;
    remove_pi_hooks_at(&extension_path)
}

fn remove_pi_hooks_at(extension_path: &Path) -> Result<()> {
    match fs::remove_file(extension_path) {
        Ok(()) => {
            println!(
                "Removed pi extension from {} (restart pi or run /reload in existing sessions)",
                extension_path.display()
            );
            Ok(())
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            println!("No pi extension found at {}", extension_path.display());
            Ok(())
        }
        Err(error) => {
            Err(error).with_context(|| format!("failed to remove {}", extension_path.display()))
        }
    }
}

pub fn run_internal(command: InternalCommands) -> Result<()> {
    match command {
        InternalCommands::IngestClaudeHook => ingest_claude_hook(),
        InternalCommands::IngestOpencodeEvent => ingest_opencode_event(),
        InternalCommands::IngestPiEvent => ingest_pi_event(),
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
    let awaiting_user = payload.get("awaiting_user").and_then(Value::as_str);

    update_pane_state(pane_id, "opencode", |state| {
        apply_opencode_state_update(state, state_name, event_type, awaiting_user);
    })
}

fn ingest_pi_event() -> Result<()> {
    let pane_id = pane_id_from_env()?;
    let payload = read_stdin_json()?;
    let state_name = payload
        .get("state")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("pi payload missing state"))?;
    let event_type = payload
        .get("event_type")
        .and_then(Value::as_str)
        .unwrap_or("pi");
    let awaiting_user = payload.get("awaiting_user").and_then(Value::as_str);
    let error = payload.get("error").and_then(Value::as_str);

    update_pane_state(pane_id, "pi", |state| {
        apply_pi_state_update(state, state_name, event_type, awaiting_user, error);
    })
}

fn apply_opencode_state_update(
    state: &mut PaneAgentState,
    state_name: &str,
    event_type: &str,
    awaiting_user: Option<&str>,
) {
    match state_name {
        "working" => {
            state.main_state = "working".to_string();
            state.error = None;

            if clears_opencode_input_wait(event_type) {
                state.awaiting_user = None;
            }
        }
        "needs_input" => {
            state.awaiting_user = Some(awaiting_user.unwrap_or("permission").to_string());
            state.error = None;
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
}

fn clears_opencode_input_wait(event_type: &str) -> bool {
    matches!(event_type, "permission.replied" | "message.part.updated")
}

fn apply_pi_state_update(
    state: &mut PaneAgentState,
    state_name: &str,
    event_type: &str,
    awaiting_user: Option<&str>,
    error: Option<&str>,
) {
    match state_name {
        "working" => {
            state.main_state = "working".to_string();
            state.awaiting_user = None;
            state.error = None;
        }
        "needs_input" => {
            state.main_state = "working".to_string();
            state.awaiting_user = Some(awaiting_user.unwrap_or("input").to_string());
            state.error = None;
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
            state.error = Some(error.unwrap_or(event_type).to_string());
            state.subagent_count = 0;
        }
        _ => {}
    }
    state.touch(event_type);
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

    let _lock = PaneStateLock::acquire(path)?;

    let mut state = read_pane_state(&path)?.unwrap_or_else(|| PaneAgentState::new(pane_id, source));
    if state.source != source {
        state = PaneAgentState::new(pane_id, source);
    }
    mutate(&mut state);

    let tmp_path = unique_tmp_path(path);
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

fn pi_extension_path() -> Result<PathBuf> {
    let home = env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".pi/agent/extensions/nerve_center.ts"))
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

fn remove_installed_claude_hooks(root: &mut serde_json::Map<String, Value>) -> Result<()> {
    let Some(hooks) = root.get_mut("hooks") else {
        return Ok(());
    };

    for event_name in [
        "UserPromptSubmit",
        "PreToolUse",
        "Stop",
        "StopFailure",
        "SubagentStart",
        "SubagentStop",
        "Notification",
    ] {
        remove_claude_event_hooks_matching(hooks, event_name, is_installed_claude_hook_command)?;
    }

    if hooks.as_object().is_some_and(|hooks| hooks.is_empty()) {
        root.remove("hooks");
    }
    Ok(())
}

fn remove_claude_event_hooks_matching(
    hooks_value: &mut Value,
    event_name: &str,
    matches_command: fn(&str) -> bool,
) -> Result<()> {
    let hooks = hooks_value
        .as_object_mut()
        .ok_or_else(|| anyhow!("Claude hooks config is not a JSON object"))?;
    let Some(entry) = hooks.get_mut(event_name) else {
        return Ok(());
    };
    let groups = entry
        .as_array_mut()
        .ok_or_else(|| anyhow!("Claude hook event {event_name} is not an array"))?;

    groups.retain_mut(|group| {
        let Some(hooks) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
            return true;
        };
        hooks.retain(|hook| {
            let Some(command) = hook.get("command").and_then(Value::as_str) else {
                return true;
            };
            !matches_command(command)
        });
        !hooks.is_empty()
    });

    if groups.is_empty() {
        hooks.remove(event_name);
    }
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

fn is_installed_claude_hook_command(command: &str) -> bool {
    command.trim_end().ends_with(CLAUDE_COMMAND_NAME)
}

fn write_json_pretty(path: &Path, value: &Value) -> Result<()> {
    let content = serde_json::to_vec_pretty(value).context("failed to serialize JSON")?;
    fs::write(path, content).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn unique_tmp_path(path: &Path) -> PathBuf {
    let suffix = TMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let extension = format!("tmp.{}.{}.{}", process::id(), now_unix_ms(), suffix);
    path.with_extension(extension)
}

struct PaneStateLock {
    path: PathBuf,
}

impl PaneStateLock {
    fn acquire(state_path: &Path) -> Result<Self> {
        let lock_path = state_path.with_extension("lock");

        for _attempt in 0..LOCK_RETRY_LIMIT {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(_) => return Ok(Self { path: lock_path }),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    if stale_lock_removed(&lock_path)? {
                        continue;
                    }
                    thread::sleep(std::time::Duration::from_millis(LOCK_RETRY_DELAY_MS));
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to acquire {}", lock_path.display()));
                }
            }
        }

        bail!("timed out waiting for {}", lock_path.display())
    }
}

impl Drop for PaneStateLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn stale_lock_removed(lock_path: &Path) -> Result<bool> {
    let Ok(metadata) = fs::metadata(lock_path) else {
        return Ok(false);
    };
    let Ok(modified) = metadata.modified() else {
        return Ok(false);
    };
    let age_ms = SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default()
        .as_millis() as u64;
    if age_ms <= LOCK_STALE_AFTER_MS {
        return Ok(false);
    }

    match fs::remove_file(lock_path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(true),
        Err(error) => {
            Err(error).with_context(|| format!("failed to remove stale {}", lock_path.display()))
        }
    }
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
    use std::sync::{Arc, Barrier};
    use std::thread;

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
        assert!(settings.contains("StopFailure"));
        assert!(settings.contains("Notification"));
        assert!(settings.contains(CLAUDE_COMMAND_NAME));
        assert!(!settings.contains("PreToolUse"));
        assert!(!settings.contains("SubagentStart"));
        assert!(!settings.contains("SubagentStop"));
    }

    #[test]
    fn claude_installer_removes_old_working_hooks() {
        let home = test_dir("claude-install-cleanup");
        let settings_path = home.join(".claude/settings.json");
        let old_command = "\"/tmp/old_nerve_center\" internal ingest-claude-hook";

        ensure_parent_dir(&settings_path).expect("settings parent should be created");

        write_json_pretty(
            &settings_path,
            &json!({
                "hooks": {
                    "UserPromptSubmit": [{
                        "hooks": [{
                            "type": "command",
                            "command": old_command,
                        }],
                    }],
                    "PreToolUse": [{
                        "hooks": [{
                            "type": "command",
                            "command": old_command,
                        }],
                    }],
                    "SubagentStart": [{
                        "hooks": [{
                            "type": "command",
                            "command": old_command,
                        }],
                    }],
                    "SubagentStop": [{
                        "hooks": [{
                            "type": "command",
                            "command": old_command,
                        }],
                    }],
                    "Notification": [{
                        "matcher": "permission_prompt|elicitation_dialog",
                        "hooks": [{
                            "type": "command",
                            "command": old_command,
                        }],
                    }],
                },
            }),
        )
        .expect("seed settings should be written");

        install_claude_hooks_at(&settings_path, "/tmp/new_nerve_center")
            .expect("claude install should succeed");

        let settings = fs::read_to_string(settings_path).expect("settings should be readable");
        assert!(settings.contains("/tmp/new_nerve_center"));
        assert!(!settings.contains("/tmp/old_nerve_center"));
        assert!(!settings.contains("PreToolUse"));
        assert!(!settings.contains("SubagentStart"));
        assert!(!settings.contains("SubagentStop"));
    }

    #[test]
    fn claude_remover_clears_installed_hooks_but_keeps_other_hooks() {
        let home = test_dir("claude-remove");
        let settings_path = home.join(".claude/settings.json");

        ensure_parent_dir(&settings_path).expect("settings parent should be created");

        write_json_pretty(
            &settings_path,
            &json!({
                "hooks": {
                    "UserPromptSubmit": [{
                        "hooks": [
                            {
                                "type": "command",
                                "command": "\"/tmp/old_nerve_center\" internal ingest-claude-hook",
                            },
                            {
                                "type": "command",
                                "command": "printf keep-me",
                            }
                        ],
                    }],
                    "Notification": [{
                        "matcher": "permission_prompt|elicitation_dialog",
                        "hooks": [{
                            "type": "command",
                            "command": "\"/tmp/old_nerve_center\" internal ingest-claude-hook",
                        }],
                    }],
                },
                "theme": "dark",
            }),
        )
        .expect("seed settings should be written");

        remove_claude_hooks_at(&settings_path).expect("claude remove should succeed");

        let settings = fs::read_to_string(settings_path).expect("settings should be readable");
        assert!(!settings.contains(CLAUDE_COMMAND_NAME));
        assert!(settings.contains("printf keep-me"));
        assert!(settings.contains("theme"));
    }

    #[test]
    fn opencode_installer_writes_plugin() {
        let home = test_dir("opencode-install");
        let plugin_path = home.join(".config/opencode/plugins/nerve_center.js");

        install_opencode_hooks_at(&plugin_path, "/tmp/nerve_center")
            .expect("opencode install should succeed");

        let plugin = fs::read_to_string(plugin_path).expect("plugin should be readable");
        assert!(plugin.contains(OPENCODE_SUBCOMMAND_NAME));
        assert!(plugin.contains("session.idle"));
        assert!(plugin.contains("message.part.updated"));
        assert!(plugin.contains("tool.execute.before"));
        assert!(plugin.contains("question.asked"));
        assert!(plugin.contains("permission.updated"));
        assert!(plugin.contains("permission.replied"));
        assert!(plugin.contains("questions"));
        assert!(!plugin.contains("status === \"idle\""));
    }

    #[test]
    fn opencode_question_prompt_sets_needs_input_until_resolved() {
        let mut state = PaneAgentState::new(42, "opencode");

        apply_opencode_state_update(
            &mut state,
            "needs_input",
            "message.part.updated",
            Some("question"),
        );

        assert_eq!(state.awaiting_user.as_deref(), Some("question"));
        assert_eq!(state.effective_state, "needs_input");

        apply_opencode_state_update(&mut state, "working", "message.part.updated", None);

        assert_eq!(state.awaiting_user, None);
        assert_eq!(state.main_state, "working");
        assert_eq!(state.effective_state, "working");
    }

    #[test]
    fn opencode_session_status_does_not_clear_pending_question() {
        let mut state = PaneAgentState::new(42, "opencode");

        apply_opencode_state_update(
            &mut state,
            "needs_input",
            "tool.execute.before",
            Some("question"),
        );
        apply_opencode_state_update(&mut state, "working", "session.status", None);

        assert_eq!(state.awaiting_user.as_deref(), Some("question"));
        assert_eq!(state.main_state, "working");
        assert_eq!(state.effective_state, "needs_input");
    }

    #[test]
    fn opencode_remover_deletes_plugin_file() {
        let home = test_dir("opencode-remove");
        let plugin_path = home.join(".config/opencode/plugins/nerve_center.js");

        ensure_parent_dir(&plugin_path).expect("plugin parent should be created");
        fs::write(&plugin_path, "stale plugin").expect("plugin should be seeded");

        remove_opencode_hooks_at(&plugin_path).expect("opencode remove should succeed");

        assert!(!plugin_path.exists());
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

    #[test]
    fn pi_installer_writes_extension() {
        let home = test_dir("pi-install");
        let extension_path = home.join(".pi/agent/extensions/nerve_center.ts");

        install_pi_hooks_at(&extension_path, "/tmp/nerve_center")
            .expect("pi install should succeed");

        let extension = fs::read_to_string(extension_path).expect("extension should be readable");
        assert!(extension.contains(PI_SUBCOMMAND_NAME));
        assert!(extension.contains("agent_start"));
        assert!(extension.contains("session_shutdown"));
        assert!(extension.contains("runtime: \"pi\""));
    }

    #[test]
    fn pi_remover_deletes_extension_file() {
        let home = test_dir("pi-remove");
        let extension_path = home.join(".pi/agent/extensions/nerve_center.ts");

        ensure_parent_dir(&extension_path).expect("extension parent should be created");
        fs::write(&extension_path, "stale extension").expect("extension should be seeded");

        remove_pi_hooks_at(&extension_path).expect("pi remove should succeed");

        assert!(!extension_path.exists());
    }

    #[test]
    fn pi_state_update_writes_pane_file() {
        let data_dir = test_dir("pi-state");
        let state_path = data_dir.join("42");

        update_pane_state_at_path(&state_path, 42, "pi", |state| {
            apply_pi_state_update(state, "working", "agent_start", None, None);
        })
        .expect("pi state write should succeed");

        let state = fs::read_to_string(state_path).expect("pane file should exist");
        assert!(state.contains("\"source\": \"pi\""));
        assert!(state.contains("\"effective_state\": \"working\""));
    }

    #[test]
    fn concurrent_updates_share_a_single_consistent_state_file() {
        let data_dir = test_dir("concurrent-state");
        let state_path = data_dir.join("8");
        let worker_count = 8;
        let barrier = Arc::new(Barrier::new(worker_count));
        let mut workers = Vec::new();

        for _ in 0..worker_count {
            let barrier = barrier.clone();
            let state_path = state_path.clone();
            workers.push(thread::spawn(move || {
                barrier.wait();
                update_pane_state_at_path(&state_path, 8, "claude", |state| {
                    state.main_state = "working".to_string();
                    state.subagent_count = state.subagent_count.saturating_add(1);
                    state.touch("concurrent");
                })
            }));
        }

        for worker in workers {
            worker
                .join()
                .expect("worker should not panic")
                .expect("state update should succeed");
        }

        let state = read_pane_state(&state_path)
            .expect("pane state should be readable")
            .expect("pane state should exist");
        assert_eq!(state.subagent_count, worker_count as u64);
        assert_eq!(state.effective_state, "working");
    }
}
