use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PaneInfo {
    pub window_id: u64,
    pub tab_id: u64,
    pub pane_id: u64,
    pub workspace: String,
    pub size: PaneSize,
    pub title: String,
    pub cwd: String,
    pub cursor_x: u64,
    pub cursor_y: u64,
    pub cursor_shape: String,
    pub cursor_visibility: String,
    pub left_col: u64,
    pub top_row: u64,
    pub tab_title: String,
    pub window_title: String,
    pub is_active: bool,
    pub is_zoomed: bool,
    pub tty_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PaneSize {
    pub rows: u64,
    pub cols: u64,
    pub pixel_width: u64,
    pub pixel_height: u64,
    pub dpi: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDirection {
    Right,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnCommand {
    label: String,
    argv: Vec<String>,
}

impl SpawnCommand {
    pub fn new(label: impl Into<String>, argv: Vec<String>) -> Self {
        Self {
            label: label.into(),
            argv,
        }
    }

    pub fn shell() -> Self {
        Self::new("shell", vec!["zsh".to_string(), "-il".to_string()])
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn argv(&self) -> &[String] {
        &self.argv
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiTabLayout {
    Solo,
    Attached(PaneInfo),
    Unsupported { pane_count: usize },
}

pub trait WeztermClient {
    fn list_panes(&mut self) -> Result<Vec<PaneInfo>>;
    fn move_pane_to_new_tab(&mut self, pane_id: u64) -> Result<()>;
    fn split_pane(
        &mut self,
        host_pane_id: u64,
        move_pane_id: u64,
        direction: SplitDirection,
    ) -> Result<()>;
    fn activate_pane(&mut self, pane_id: u64) -> Result<()>;
    fn send_text(&mut self, pane_id: u64, text: &str) -> Result<()>;
    fn spawn_new_tab(&mut self, pane_id: u64, cwd: &str, command: &SpawnCommand) -> Result<u64>;
}

#[derive(Debug, Default)]
pub struct ProcessWezterm;

impl WeztermClient for ProcessWezterm {
    fn list_panes(&mut self) -> Result<Vec<PaneInfo>> {
        let output = run_wezterm_cli(["list", "--format", "json"])?;
        parse_pane_list_output(&output)
    }

    fn move_pane_to_new_tab(&mut self, pane_id: u64) -> Result<()> {
        run_wezterm_cli(["move-pane-to-new-tab", "--pane-id", &pane_id.to_string()])?;
        Ok(())
    }

    fn split_pane(
        &mut self,
        host_pane_id: u64,
        move_pane_id: u64,
        direction: SplitDirection,
    ) -> Result<()> {
        let direction_flag = match direction {
            SplitDirection::Right => "--right",
        };

        run_wezterm_cli([
            "split-pane",
            "--pane-id",
            &host_pane_id.to_string(),
            direction_flag,
            "--move-pane-id",
            &move_pane_id.to_string(),
        ])?;
        Ok(())
    }

    fn activate_pane(&mut self, pane_id: u64) -> Result<()> {
        run_wezterm_cli(["activate-pane", "--pane-id", &pane_id.to_string()])?;
        Ok(())
    }

    fn send_text(&mut self, pane_id: u64, text: &str) -> Result<()> {
        run_wezterm_cli([
            "send-text",
            "--pane-id",
            &pane_id.to_string(),
            "--no-paste",
            text,
        ])?;
        Ok(())
    }

    fn spawn_new_tab(&mut self, pane_id: u64, cwd: &str, command: &SpawnCommand) -> Result<u64> {
        let mut args = vec![
            "spawn".to_string(),
            "--pane-id".to_string(),
            pane_id.to_string(),
            "--cwd".to_string(),
            cwd.to_string(),
            "--".to_string(),
        ];
        args.extend(resolve_spawn_argv(command));
        let output = run_wezterm_cli(args)?;
        output.trim().parse::<u64>().with_context(|| {
            format!("failed to parse spawned pane id from wezterm cli output: {output:?}")
        })
    }
}

fn resolve_spawn_argv(command: &SpawnCommand) -> Vec<String> {
    let mut argv = command.argv().to_vec();
    if let Some(program) = argv.first_mut() {
        if let Some(resolved) = resolve_executable(program) {
            *program = resolved;
        }
    }
    argv
}

fn resolve_executable(program: &str) -> Option<String> {
    if program.contains('/') {
        return Some(program.to_string());
    }

    executable_search_dirs()
        .into_iter()
        .map(|dir| dir.join(program))
        .find(|path| is_candidate_executable(path))
        .and_then(|path| path.to_str().map(str::to_string))
}

fn executable_search_dirs() -> Vec<PathBuf> {
    let mut dirs = env::var_os("PATH")
        .map(|value| env::split_paths(&value).collect::<Vec<_>>())
        .unwrap_or_default();

    for extra in npm_search_dirs() {
        if !dirs.iter().any(|existing| existing == &extra) {
            dirs.push(extra);
        }
    }

    dirs
}

fn npm_search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    if let Ok(home) = env::var("HOME") {
        dirs.push(PathBuf::from(&home).join(".local/npm/bin"));
        dirs.push(PathBuf::from(&home).join(".npm-global/bin"));
    }

    if let Some(prefix) = npm_global_prefix() {
        dirs.push(prefix.join("bin"));
    }

    dirs
}

fn npm_global_prefix() -> Option<PathBuf> {
    let output = Command::new("npm").args(["prefix", "-g"]).output().ok()?;
    if !output.status.success() {
        return None;
    }

    let prefix = String::from_utf8(output.stdout).ok()?;
    let prefix = prefix.trim();
    (!prefix.is_empty()).then(|| PathBuf::from(prefix))
}

fn is_candidate_executable(path: &Path) -> bool {
    path.is_file()
}

fn parse_pane_list_output(output: &str) -> Result<Vec<PaneInfo>> {
    if let Ok(panes) = serde_json::from_str::<Vec<PaneInfo>>(output) {
        return Ok(panes);
    }

    let start = output.find('[');
    let end = output.rfind(']');
    if let (Some(start), Some(end)) = (start, end) {
        if start <= end {
            let candidate = &output[start..=end];
            if let Ok(panes) = serde_json::from_str::<Vec<PaneInfo>>(candidate) {
                return Ok(panes);
            }
        }
    }

    Err(anyhow!("failed to parse wezterm pane list JSON"))
}

pub fn tui_pane_id_from_env() -> Result<u64> {
    let value = env::var("WEZTERM_PANE").context("WEZTERM_PANE is not set")?;
    value
        .parse::<u64>()
        .with_context(|| format!("WEZTERM_PANE is not a valid pane id: {value}"))
}

pub fn sort_panes(panes: &mut [PaneInfo]) {
    panes.sort_by_key(|pane| (pane.window_id, pane.tab_id, pane.pane_id));
}

pub fn listable_panes(panes: &[PaneInfo], tui_pane_id: u64) -> Vec<PaneInfo> {
    let mut filtered = panes
        .iter()
        .filter(|pane| pane.pane_id != tui_pane_id)
        .cloned()
        .collect::<Vec<_>>();
    sort_panes(&mut filtered);
    filtered
}

pub fn find_pane(panes: &[PaneInfo], pane_id: u64) -> Result<&PaneInfo> {
    panes
        .iter()
        .find(|pane| pane.pane_id == pane_id)
        .ok_or_else(|| anyhow!("pane {pane_id} not found in wezterm list"))
}

pub fn tui_tab_layout(panes: &[PaneInfo], tui_pane_id: u64) -> Result<TuiTabLayout> {
    let tui_tab_id = find_pane(panes, tui_pane_id)?.tab_id;
    let tab_panes = panes
        .iter()
        .filter(|pane| pane.tab_id == tui_tab_id)
        .cloned()
        .collect::<Vec<_>>();

    match tab_panes.len() {
        0 => bail!("tui pane {tui_pane_id} is missing from its own tab"),
        1 => Ok(TuiTabLayout::Solo),
        2 => Ok(TuiTabLayout::Attached(
            tab_panes
                .into_iter()
                .find(|pane| pane.pane_id != tui_pane_id)
                .context("failed to determine attached pane")?,
        )),
        pane_count => Ok(TuiTabLayout::Unsupported { pane_count }),
    }
}

fn run_wezterm_cli<I, S>(args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new("wezterm")
        .arg("cli")
        .args(args)
        .output()
        .context("failed to spawn wezterm cli")?;

    if output.status.success() {
        String::from_utf8(output.stdout).context("wezterm cli stdout was not valid UTF-8")
    } else {
        let stderr =
            String::from_utf8(output.stderr).unwrap_or_else(|_| "<non-utf8 stderr>".to_string());
        bail!("wezterm cli command failed: {}", stderr.trim());
    }
}

#[cfg(test)]
mod tests {
    use super::{
        find_pane, listable_panes, parse_pane_list_output, tui_tab_layout, PaneInfo, TuiTabLayout,
    };

    const SAMPLE_JSON: &str = r#"
[
  {
    "window_id": 4,
    "tab_id": 202,
    "pane_id": 341,
    "workspace": "rust tui",
    "size": {
      "rows": 44,
      "cols": 94,
      "pixel_width": 940,
      "pixel_height": 1144,
      "dpi": 96
    },
    "title": "nerve_center",
    "cwd": "file:///repo/nerve_center",
    "cursor_x": 2,
    "cursor_y": 36,
    "cursor_shape": "Default",
    "cursor_visibility": "Visible",
    "left_col": 0,
    "top_row": 0,
    "tab_title": "1",
    "window_title": "nerve_center",
    "is_active": true,
    "is_zoomed": false,
    "tty_name": "/dev/pts/5"
  },
  {
    "window_id": 4,
    "tab_id": 202,
    "pane_id": 342,
    "workspace": "rust tui",
    "size": {
      "rows": 44,
      "cols": 94,
      "pixel_width": 940,
      "pixel_height": 1144,
      "dpi": 96
    },
    "title": "shell",
    "cwd": "file:///repo/other",
    "cursor_x": 2,
    "cursor_y": 41,
    "cursor_shape": "Default",
    "cursor_visibility": "Visible",
    "left_col": 95,
    "top_row": 0,
    "tab_title": "1",
    "window_title": "nerve_center",
    "is_active": false,
    "is_zoomed": false,
    "tty_name": "/dev/pts/6"
  },
  {
    "window_id": 4,
    "tab_id": 205,
    "pane_id": 346,
    "workspace": "rust tui",
    "size": {
      "rows": 44,
      "cols": 189,
      "pixel_width": 1890,
      "pixel_height": 1144,
      "dpi": 96
    },
    "title": "rs_agent",
    "cwd": "file:///repo/rs_agent",
    "cursor_x": 2,
    "cursor_y": 24,
    "cursor_shape": "Default",
    "cursor_visibility": "Visible",
    "left_col": 0,
    "top_row": 0,
    "tab_title": "",
    "window_title": "nerve_center",
    "is_active": true,
    "is_zoomed": false,
    "tty_name": "/dev/pts/8"
  }
]
"#;

    fn sample_panes() -> Vec<PaneInfo> {
        serde_json::from_str(SAMPLE_JSON).expect("sample JSON should parse")
    }

    #[test]
    fn parses_wezterm_json_shape() {
        let panes = sample_panes();
        assert_eq!(panes.len(), 3);
        assert_eq!(panes[0].pane_id, 341);
        assert_eq!(panes[1].title, "shell");
    }

    #[test]
    fn parses_wezterm_json_with_prefix_noise() {
        let noisy = format!("warning: something\n{}\n", SAMPLE_JSON);
        let panes = parse_pane_list_output(&noisy).expect("pane list should parse with noise");
        assert_eq!(panes.len(), 3);
        assert_eq!(panes[2].pane_id, 346);
    }

    #[test]
    fn excludes_tui_pane_from_selectable_list() {
        let panes = sample_panes();
        let listable = listable_panes(&panes, 341);
        assert_eq!(
            listable.iter().map(|pane| pane.pane_id).collect::<Vec<_>>(),
            vec![342, 346]
        );
    }

    #[test]
    fn detects_attached_layout() {
        let panes = sample_panes();
        let layout = tui_tab_layout(&panes, 341).expect("layout should resolve");
        assert_eq!(
            layout,
            TuiTabLayout::Attached(find_pane(&panes, 342).expect("pane should exist").clone())
        );
    }

    #[test]
    fn detects_unsupported_layout() {
        let mut panes = sample_panes();
        panes.push(find_pane(&panes, 346).expect("pane should exist").clone());
        panes.last_mut().expect("pane should exist").tab_id = 202;

        let layout = tui_tab_layout(&panes, 341).expect("layout should resolve");
        assert_eq!(layout, TuiTabLayout::Unsupported { pane_count: 3 });
    }
}
