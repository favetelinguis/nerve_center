use std::collections::BTreeMap;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};

use crate::daemon::ops::run_operation;
use crate::daemon::protocol::{ClientMessage, ServerEvent, ServerMessage};
use crate::daemon::refresh::RefreshQueue;
use crate::daemon::state::{
    restore_operation_from_journal_line, JournalOperationRecord, WorkspaceState,
};
use crate::projects::{discover_projects_in, ProjectEntry};
use crate::workspace::{ProjectFreshness, ProjectGitState, ProjectSnapshot};

type OperationRunner = Arc<
    dyn Fn(&ProjectSnapshot, &ProjectSnapshot, &str, &str) -> Result<()> + Send + Sync + 'static,
>;

const IDLE_REFRESH_BACKSTOP: Duration = Duration::from_secs(30);

pub fn run() -> Result<()> {
    let repo_sources = crate::projects::load_repo_sources_from_config()?;
    let projects = discover_projects_in(&repo_sources)?;
    let state = SharedState::from_workspace_state(load_workspace_state(projects)?);
    spawn_refresh_worker(state.clone());
    let listener = bind_socket_listener()?;

    run_listener_until_shutdown(listener.listener(), &state)
}

fn run_listener_until_shutdown(listener: &UnixListener, state: &SharedState) -> Result<()> {
    listener.set_nonblocking(true)?;

    while !state.shutdown_requested() {
        match listener.accept() {
            Ok((stream, _)) => {
                let state = state.clone();
                thread::spawn(move || {
                    let _ = serve_shared_client_connection(stream, &state);
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(error).context("failed to accept daemon client connection"),
        }
    }

    Ok(())
}

#[derive(Clone)]
struct SharedState {
    workspace: Arc<Mutex<WorkspaceState>>,
    refresh: Arc<RefreshSignal>,
    subscribers: Arc<Mutex<Vec<mpsc::Sender<crate::workspace::WorkspaceSnapshot>>>>,
    shutdown: Arc<AtomicBool>,
}

impl SharedState {
    fn from_workspace_state(state: WorkspaceState) -> Self {
        Self {
            workspace: Arc::new(Mutex::new(state)),
            refresh: Arc::new(RefreshSignal::default()),
            subscribers: Arc::new(Mutex::new(Vec::new())),
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.refresh.wakeup.notify_all();
    }

    fn shutdown_requested(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    fn running_operation_roots(&self) -> Vec<String> {
        self.lock_workspace()
            .running_operations_by_root
            .keys()
            .cloned()
            .collect()
    }

    fn lock_workspace(&self) -> std::sync::MutexGuard<'_, WorkspaceState> {
        self.workspace
            .lock()
            .expect("daemon state lock should not be poisoned")
    }

    fn mark_root_dirty(&self, root_id: &str, reason: &str) {
        {
            let mut state = self.lock_workspace();
            mark_root_projects_stale(&mut state, root_id, reason);
        }

        let mut queue = self
            .refresh
            .queue
            .lock()
            .expect("refresh queue lock should not be poisoned");
        queue.mark_dirty(root_id);
        self.refresh.wakeup.notify_one();
        self.notify_snapshot();
    }

    fn mark_all_roots_dirty(&self, reason: &str) {
        let root_ids = {
            let mut state = self.lock_workspace();
            let root_ids = state.root_ids();
            for root_id in &root_ids {
                mark_root_projects_stale(&mut state, root_id, reason);
            }
            root_ids
        };

        let mut queue = self
            .refresh
            .queue
            .lock()
            .expect("refresh queue lock should not be poisoned");
        for root_id in &root_ids {
            queue.mark_dirty(root_id);
        }
        self.refresh.wakeup.notify_one();
        drop(queue);
        self.notify_snapshot();
    }

    fn enqueue_root_refresh(&self, root_id: &str) {
        let mut queue = self
            .refresh
            .queue
            .lock()
            .expect("refresh queue lock should not be poisoned");
        queue.mark_dirty(root_id);
        self.refresh.wakeup.notify_one();
    }

    fn snapshot(&self) -> crate::workspace::WorkspaceSnapshot {
        let workspace = self.lock_workspace();
        workspace.snapshot(now_unix_ms())
    }

    fn add_subscriber(&self, subscriber: mpsc::Sender<crate::workspace::WorkspaceSnapshot>) {
        self.subscribers
            .lock()
            .expect("subscriber lock should not be poisoned")
            .push(subscriber);
    }

    fn notify_snapshot(&self) {
        let snapshot = self.snapshot();
        let mut subscribers = self
            .subscribers
            .lock()
            .expect("subscriber lock should not be poisoned");
        subscribers.retain(|subscriber| subscriber.send(snapshot.clone()).is_ok());
    }
}

struct SocketListenerGuard {
    listener: UnixListener,
    path: PathBuf,
}

impl SocketListenerGuard {
    fn bind(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create socket dir {}", parent.display()))?;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).with_context(|| {
                format!(
                    "failed to tighten socket dir permissions {}",
                    parent.display()
                )
            })?;
        }
        if path.exists() {
            match UnixStream::connect(&path) {
                Ok(_) => bail!("daemon socket already active at {}", path.display()),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                    ) =>
                {
                    fs::remove_file(&path).with_context(|| {
                        format!("failed to remove stale socket {}", path.display())
                    })?;
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("failed to inspect daemon socket {}", path.display())
                    });
                }
            }
        }

        let listener = UnixListener::bind(&path)
            .with_context(|| format!("failed to bind daemon socket {}", path.display()))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).with_context(|| {
            format!(
                "failed to tighten daemon socket permissions {}",
                path.display()
            )
        })?;

        Ok(Self { listener, path })
    }

    fn listener(&self) -> &UnixListener {
        &self.listener
    }
}

impl Drop for SocketListenerGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Default)]
struct RefreshSignal {
    queue: Mutex<RefreshQueue>,
    wakeup: Condvar,
}

fn load_workspace_state(projects: Vec<ProjectEntry>) -> Result<WorkspaceState> {
    let mut state = build_workspace_state(projects);
    for event in crate::daemon::agent::drain_spool()? {
        state.apply_agent_event(event);
    }
    restore_operation_journal(&mut state)?;
    Ok(state)
}

fn build_workspace_state(projects: Vec<ProjectEntry>) -> WorkspaceState {
    let mut state = WorkspaceState::default();
    let updated_at_ms = now_unix_ms();

    for project in projects {
        let root_id = crate::project_id::root_project_id(&project.root_cwd);
        let id = crate::project_id::project_id(&project);

        state.project_order.push(id.clone());
        state.projects.insert(
            id.clone(),
            ProjectSnapshot {
                id,
                name: project.name,
                cwd: project.cwd,
                root_id,
                kind: project.kind,
                git: ProjectGitState {
                    branch: project.branch,
                    status: project.status_summary.display_text(),
                    status_summary: project.status_summary,
                },
                freshness: ProjectFreshness {
                    state: "fresh".to_string(),
                    updated_at_ms,
                    stale_reason: None,
                },
                ..ProjectSnapshot::default()
            },
        );
    }

    state
}

fn start_run_operation(
    state: &mut WorkspaceState,
    project_id: &str,
    operation: &str,
) -> Result<(ProjectSnapshot, ProjectSnapshot, String)> {
    let project = state
        .projects
        .get(project_id)
        .cloned()
        .ok_or_else(|| anyhow!("unknown project: {project_id}"))?;
    let root_project = state
        .projects
        .get(&project.root_id)
        .cloned()
        .ok_or_else(|| anyhow!("unknown root project: {}", project.root_id))?;
    let root_id = project.root_id.clone();

    state.try_start_operation(&root_id, operation)?;
    if let Some(project_state) = state.projects.get_mut(project_id) {
        project_state.operation.kind = operation.to_string();
        project_state.operation.message = "running".to_string();
    }

    if let Err(error) = append_operation_journal_record(&JournalOperationRecord {
        operation_id: operation.to_string(),
        root_id: root_id.clone(),
        phase: "running".to_string(),
    }) {
        if let Some(project_state) = state.projects.get_mut(project_id) {
            project_state.operation = Default::default();
        }
        state.finish_operation(&root_id);
        return Err(error);
    }

    Ok((root_project, project, root_id))
}

fn finish_run_operation(state: &mut WorkspaceState, project_id: &str, root_id: &str) {
    if let Some(project_state) = state.projects.get_mut(project_id) {
        project_state.operation = Default::default();
    }
    state.finish_operation(root_id);
}

fn append_finished_operation_record(root_id: &str, operation: &str, succeeded: bool) -> Result<()> {
    append_operation_journal_record(&JournalOperationRecord {
        operation_id: operation.to_string(),
        root_id: root_id.to_string(),
        phase: if succeeded {
            "finished".to_string()
        } else {
            "failed".to_string()
        },
    })
}

fn handle_run_operation_with_runner<F>(
    state: &mut WorkspaceState,
    project_id: &str,
    operation: &str,
    command: &str,
    runner: F,
) -> Result<ServerEvent>
where
    F: FnOnce(&ProjectSnapshot, &ProjectSnapshot, &str, &str) -> Result<()>,
{
    let (root_project, project, root_id) = start_run_operation(state, project_id, operation)?;

    let result = runner(&root_project, &project, operation, command);
    finish_run_operation(state, project_id, &root_id);
    append_finished_operation_record(&root_id, operation, result.is_ok())?;

    result?;
    refresh_workspace_after_operation(state, &root_id);

    Ok(ServerEvent::OperationFinished {
        project_id: project_id.to_string(),
        operation: operation.to_string(),
    })
}

fn handle_run_operation(
    state: &mut WorkspaceState,
    project_id: &str,
    operation: &str,
    command: &str,
) -> Result<ServerEvent> {
    handle_run_operation_with_runner(state, project_id, operation, command, run_operation)
}

fn refresh_workspace_after_operation(state: &mut WorkspaceState, root_id: &str) {
    match reload_workspace_state(state) {
        Ok(refreshed) => *state = refreshed,
        Err(error) => {
            mark_root_projects_stale(state, root_id, "refresh_failed");
            state.warnings.push(format!(
                "failed to refresh workspace after operation for {root_id}: {error}"
            ));
        }
    }
}

fn reload_workspace_state(existing: &WorkspaceState) -> Result<WorkspaceState> {
    let repo_sources = crate::projects::load_repo_sources_from_config()?;
    let projects = discover_projects_in(&repo_sources)?;
    let mut refreshed = build_workspace_state(projects);
    let updated_at_ms = now_unix_ms();

    for (project_id, project) in &mut refreshed.projects {
        if let Some(previous) = existing.projects.get(project_id) {
            project.agents = previous.agents.clone();
            project.operation = previous.operation.clone();
        }
        project.freshness = ProjectFreshness {
            state: "fresh".to_string(),
            updated_at_ms,
            stale_reason: None,
        };
    }
    refreshed.warnings = existing.warnings.clone();

    Ok(refreshed)
}

fn mark_root_projects_stale(state: &mut WorkspaceState, root_id: &str, reason: &str) {
    let updated_at_ms = now_unix_ms();

    for project in state.projects.values_mut() {
        if project.id != root_id && project.root_id != root_id {
            continue;
        }

        project.freshness = ProjectFreshness {
            state: "stale".to_string(),
            updated_at_ms,
            stale_reason: Some(reason.to_string()),
        };
    }
}

fn refresh_root_now(state: &mut WorkspaceState, root_id: &str) -> Result<()> {
    let refreshed = reload_workspace_state(state)?;
    let running_operations_by_root = state.running_operations_by_root.clone();
    *state = refreshed;
    state.running_operations_by_root = running_operations_by_root;
    mark_root_projects_fresh(state, root_id);
    Ok(())
}

fn refresh_shared_workspace_now(
    state: &SharedState,
    root_id: &str,
    warning_context: &str,
) -> Result<()> {
    let existing = {
        let workspace = state.lock_workspace();
        workspace.clone()
    };
    let refreshed = reload_workspace_state(&existing);

    let mut workspace = state.lock_workspace();
    match refreshed {
        Ok(mut refreshed) => {
            refreshed.running_operations_by_root = workspace.running_operations_by_root.clone();
            *workspace = refreshed;
            mark_root_projects_fresh(&mut workspace, root_id);
        }
        Err(error) => {
            mark_root_projects_stale(&mut workspace, root_id, "refresh_failed");
            workspace.warnings.push(format!(
                "failed to {warning_context} for {root_id}: {error}"
            ));
        }
    }
    drop(workspace);
    state.notify_snapshot();
    Ok(())
}

fn idle_refresh_backstop() -> Duration {
    IDLE_REFRESH_BACKSTOP
}

fn complete_background_operation(
    state: &SharedState,
    project_id: &str,
    root_id: &str,
    operation: &str,
    result: &Result<()>,
) {
    let journal_result = append_finished_operation_record(root_id, operation, result.is_ok());

    {
        let mut workspace = state.lock_workspace();
        finish_run_operation(&mut workspace, project_id, root_id);
        mark_root_projects_stale(&mut workspace, root_id, "refresh_pending");
        if let Err(error) = result {
            workspace.warnings.push(format!(
                "operation {operation} failed for {root_id}: {error}"
            ));
        }
        if let Err(error) = journal_result {
            workspace.warnings.push(format!(
                "failed to record operation completion for {root_id}: {error}"
            ));
        }
    }

    state.notify_snapshot();
    state.enqueue_root_refresh(root_id);
}

fn spawn_background_operation(
    state: SharedState,
    project_id: String,
    root_project: ProjectSnapshot,
    project: ProjectSnapshot,
    root_id: String,
    operation: String,
    command: String,
    runner: OperationRunner,
) {
    thread::spawn(move || {
        let result = runner(&root_project, &project, &operation, &command);
        complete_background_operation(&state, &project_id, &root_id, &operation, &result);
    });
}

fn mark_root_projects_fresh(state: &mut WorkspaceState, root_id: &str) {
    let updated_at_ms = now_unix_ms();

    for project in state.projects.values_mut() {
        if project.id != root_id && project.root_id != root_id {
            continue;
        }

        project.freshness = ProjectFreshness {
            state: "fresh".to_string(),
            updated_at_ms,
            stale_reason: None,
        };
    }
}

fn restore_operation_journal(state: &mut WorkspaceState) -> Result<()> {
    let path = operation_journal_path()?;
    if !path.exists() {
        return Ok(());
    }

    let journal = fs::read_to_string(&path)
        .with_context(|| format!("failed to read operation journal {}", path.display()))?;
    let mut latest_by_root = BTreeMap::new();

    for line in journal.lines().filter(|line| !line.trim().is_empty()) {
        let record = restore_operation_from_journal_line(line)
            .with_context(|| format!("failed to restore operation journal {}", path.display()))?;
        latest_by_root.insert(record.root_id.clone(), record);
    }

    for record in latest_by_root.into_values() {
        if record.phase == "interrupted" {
            let restored_project_id = if state.projects.contains_key(&record.root_id) {
                Some(record.root_id.clone())
            } else {
                state
                    .projects
                    .iter()
                    .find(|(_, project)| project.root_id == record.root_id)
                    .map(|(project_id, _)| project_id.clone())
            };

            if let Some(project_id) = restored_project_id {
                let project = state
                    .projects
                    .get_mut(&project_id)
                    .expect("restored project should exist");
                project.operation.kind = record.operation_id.clone();
                project.operation.message = "interrupted".to_string();
            }
            state.warnings.push(format!(
                "interrupted operation {} recovered for {}",
                record.operation_id, record.root_id
            ));
        }
    }

    Ok(())
}

fn append_operation_journal_record(record: &JournalOperationRecord) -> Result<()> {
    let path = operation_journal_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create operation journal dir {}",
                parent.display()
            )
        })?;
    }

    let mut journal = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open operation journal {}", path.display()))?;
    let line = crate::daemon::protocol::encode_json_line(record)
        .context("failed to encode operation journal record")?;
    journal
        .write_all(line.as_bytes())
        .with_context(|| format!("failed to append operation journal {}", path.display()))?;
    journal
        .flush()
        .with_context(|| format!("failed to flush operation journal {}", path.display()))?;
    Ok(())
}

fn operation_journal_path() -> Result<PathBuf> {
    if let Ok(path) = env::var("NERVE_CENTER_DATA_DIR") {
        return Ok(PathBuf::from(path).join("operation-journal.jsonl"));
    }

    let home = env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".local/state/nerve_center/operation-journal.jsonl"))
}

fn bind_socket_listener() -> Result<SocketListenerGuard> {
    let path = socket_path()?;
    SocketListenerGuard::bind(path)
}

fn socket_path() -> Result<PathBuf> {
    if let Ok(path) = env::var("NERVE_CENTER_DAEMON_SOCKET") {
        return Ok(PathBuf::from(path));
    }

    if let Ok(path) = env::var("XDG_RUNTIME_DIR") {
        if !path.is_empty() {
            return Ok(PathBuf::from(path).join("nerve_center/daemon.sock"));
        }
    }

    let home = env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".local/state/nerve_center/daemon.sock"))
}

fn spawn_refresh_worker(state: SharedState) {
    thread::spawn(move || {
        while let Some(roots) = next_refresh_roots(&state) {
            if roots.is_empty() {
                continue;
            }

            let existing = {
                let state_guard = state.lock_workspace();
                state_guard.clone()
            };
            let refresh_result = reload_workspace_state(&existing);

            let mut workspace = state.lock_workspace();
            match refresh_result {
                Ok(mut refreshed) => {
                    refreshed.running_operations_by_root =
                        workspace.running_operations_by_root.clone();
                    *workspace = refreshed;
                    for root_id in &roots {
                        mark_root_projects_fresh(&mut workspace, root_id);
                    }
                }
                Err(error) => {
                    for root_id in &roots {
                        mark_root_projects_stale(&mut workspace, root_id, "refresh_failed");
                        workspace.warnings.push(format!(
                            "failed to refresh workspace for {root_id}: {error}"
                        ));
                    }
                }
            }
            drop(workspace);
            state.notify_snapshot();
        }
    });
}

fn next_refresh_roots(state: &SharedState) -> Option<Vec<String>> {
    let mut queue = state
        .refresh
        .queue
        .lock()
        .expect("refresh queue lock should not be poisoned");

    loop {
        if state.shutdown_requested() {
            return None;
        }

        let roots = queue.pop_all();
        if !roots.is_empty() {
            return Some(roots);
        }

        let (guard, timeout) = state
            .refresh
            .wakeup
            .wait_timeout(queue, idle_refresh_backstop())
            .expect("refresh queue wait should not fail");
        queue = guard;

        if state.shutdown_requested() {
            return None;
        }

        if timeout.timed_out() {
            drop(queue);
            state.mark_all_roots_dirty("refresh_pending");
            queue = state
                .refresh
                .queue
                .lock()
                .expect("refresh queue lock should not be poisoned");
        }
    }
}

fn serve_subscription_session(
    mut writer: UnixStream,
    receiver: mpsc::Receiver<crate::workspace::WorkspaceSnapshot>,
) -> Result<()> {
    for snapshot in receiver {
        let encoded =
            crate::daemon::protocol::encode_json_line(&ServerMessage::Snapshot { snapshot })
                .context("failed to encode daemon subscription update")?;
        if let Err(error) = writer.write_all(encoded.as_bytes()) {
            if matches!(
                error.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::UnexpectedEof
            ) {
                return Ok(());
            }
            return Err(error).context("failed to write daemon subscription update");
        }
        if let Err(error) = writer.flush() {
            if matches!(
                error.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::UnexpectedEof
            ) {
                return Ok(());
            }
            return Err(error).context("failed to flush daemon subscription update");
        }
    }

    Ok(())
}

fn serve_shared_client_connection(stream: UnixStream, state: &SharedState) -> Result<()> {
    let stream_reader = stream
        .try_clone()
        .context("failed to clone daemon client stream")?;
    let mut reader = BufReader::new(stream_reader);
    let mut writer = stream;

    loop {
        let mut line = String::new();
        let bytes = reader
            .read_line(&mut line)
            .context("failed to read daemon client message")?;
        if bytes == 0 {
            return Ok(());
        }

        let response = match serde_json::from_str::<ClientMessage>(&line) {
            Ok(ClientMessage::Subscribe) => {
                let (sender, receiver) = mpsc::channel();
                state.add_subscriber(sender);
                let snapshot = state.snapshot();
                let encoded = crate::daemon::protocol::encode_json_line(&ServerMessage::Snapshot {
                    snapshot,
                })
                .context("failed to encode daemon subscription snapshot")?;
                writer
                    .write_all(encoded.as_bytes())
                    .context("failed to write daemon subscription snapshot")?;
                writer
                    .flush()
                    .context("failed to flush daemon subscription snapshot")?;
                return serve_subscription_session(writer, receiver);
            }
            Ok(message) => server_message_for_client_message(state, message),
            Err(error) => ServerMessage::Error {
                message: format!("failed to decode client message: {error}"),
            },
        };

        let encoded = crate::daemon::protocol::encode_json_line(&response)
            .context("failed to encode daemon response")?;
        writer
            .write_all(encoded.as_bytes())
            .context("failed to write daemon response")?;
        writer.flush().context("failed to flush daemon response")?;
    }
}

fn serve_client_connection(stream: UnixStream, state: &mut WorkspaceState) -> Result<()> {
    let shared_state = SharedState::from_workspace_state(std::mem::take(state));
    let result = serve_shared_client_connection(stream, &shared_state);
    let mut restored = shared_state.lock_workspace();
    *state = std::mem::take(&mut *restored);
    result
}

fn server_message_for_client_message(state: &SharedState, message: ClientMessage) -> ServerMessage {
    match process_client_message_with_dependencies(state, message, run_operation) {
        Ok(response) => response,
        Err(error) => ServerMessage::Error {
            message: error.to_string(),
        },
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_millis() as u64
}

fn handle_workspace_client_message(
    state: &mut WorkspaceState,
    message: ClientMessage,
) -> Result<Option<ServerMessage>> {
    match message {
        ClientMessage::RunOperation {
            project_id,
            operation,
            command,
        } => Ok(Some(ServerMessage::Event {
            event: handle_run_operation(state, &project_id, &operation, &command)?,
        })),
        ClientMessage::RefreshProject { project_id } => {
            let root_id = state
                .projects
                .get(&project_id)
                .map(|project| project.root_id.clone())
                .ok_or_else(|| anyhow!("unknown project: {project_id}"))?;
            mark_root_projects_stale(state, &root_id, "refresh_pending");
            refresh_root_now(state, &root_id)?;
            Ok(Some(ServerMessage::Event {
                event: ServerEvent::Updated,
            }))
        }
        ClientMessage::RefreshRoot { root_id } => {
            mark_root_projects_stale(state, &root_id, "refresh_pending");
            refresh_root_now(state, &root_id)?;
            Ok(Some(ServerMessage::Event {
                event: ServerEvent::Updated,
            }))
        }
        ClientMessage::Subscribe => {
            for root_id in state.root_ids() {
                mark_root_projects_stale(state, &root_id, "refresh_pending");
                refresh_root_now(state, &root_id)?;
            }
            Ok(Some(ServerMessage::Event {
                event: ServerEvent::Updated,
            }))
        }
        _ => Ok(None),
    }
}

fn process_client_message_with_dependencies<F>(
    state: &SharedState,
    message: ClientMessage,
    runner: F,
) -> Result<ServerMessage>
where
    F: Fn(&ProjectSnapshot, &ProjectSnapshot, &str, &str) -> Result<()> + Send + Sync + 'static,
{
    process_client_message_with_runner(state, message, Arc::new(runner))
}

fn process_client_message_with_runner(
    state: &SharedState,
    message: ClientMessage,
    runner: OperationRunner,
) -> Result<ServerMessage> {
    if state.shutdown_requested()
        && !matches!(
            message,
            ClientMessage::GetSnapshot | ClientMessage::Shutdown
        )
    {
        bail!("daemon is shutting down; only get_snapshot and shutdown are accepted");
    }

    match message {
        ClientMessage::Hello { .. } => Ok(ServerMessage::Hello {
            protocol_version: 1,
            server_name: "nerve_centerd".to_string(),
        }),
        ClientMessage::GetSnapshot => Ok(ServerMessage::Snapshot {
            snapshot: state.snapshot(),
        }),
        ClientMessage::AgentEvent { event } => {
            {
                let mut workspace = state.lock_workspace();
                workspace.apply_agent_event(event);
            }
            state.notify_snapshot();
            Ok(ServerMessage::Event {
                event: ServerEvent::Updated,
            })
        }
        ClientMessage::RunOperation {
            project_id,
            operation,
            command,
        } => {
            let (root_project, project, root_id) = {
                let mut workspace = state.lock_workspace();
                start_run_operation(&mut workspace, &project_id, &operation)?
            };
            state.notify_snapshot();
            spawn_background_operation(
                state.clone(),
                project_id,
                root_project,
                project,
                root_id,
                operation,
                command,
                runner,
            );

            Ok(ServerMessage::Event {
                event: ServerEvent::Updated,
            })
        }
        ClientMessage::RefreshProject { project_id } => {
            let root_id = {
                let workspace = state.lock_workspace();
                workspace
                    .projects
                    .get(&project_id)
                    .map(|project| project.root_id.clone())
                    .ok_or_else(|| anyhow!("unknown project: {project_id}"))?
            };
            state.mark_root_dirty(&root_id, "refresh_pending");
            refresh_shared_workspace_now(state, &root_id, "refresh project")?;
            Ok(ServerMessage::Event {
                event: ServerEvent::Updated,
            })
        }
        ClientMessage::RefreshRoot { root_id } => {
            state.mark_root_dirty(&root_id, "refresh_pending");
            refresh_shared_workspace_now(state, &root_id, "refresh root")?;
            Ok(ServerMessage::Event {
                event: ServerEvent::Updated,
            })
        }
        ClientMessage::Subscribe => Ok(ServerMessage::Snapshot {
            snapshot: state.snapshot(),
        }),
        ClientMessage::Shutdown => {
            let running_roots = state.running_operation_roots();
            if !running_roots.is_empty() {
                bail!(
                    "shutdown is blocked while operations are still running for: {}",
                    running_roots.join(", ")
                );
            }
            state.request_shutdown();
            Ok(ServerMessage::Event {
                event: ServerEvent::Updated,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_workspace_state, handle_workspace_client_message, load_workspace_state,
        process_client_message_with_dependencies, serve_client_connection,
        server_message_for_client_message, SharedState,
    };
    use crate::app::App;
    use crate::daemon::agent::append_spool_record;
    use crate::daemon::agent::AgentEvent;
    use crate::daemon::protocol::encode_json_line;
    use crate::daemon::protocol::{ClientMessage, ServerEvent, ServerMessage};
    use crate::daemon::state::WorkspaceState;
    use crate::projects::{discover_projects_in, ProjectEntry, ProjectKind, ProjectStatusSummary};
    use crate::workspace::{ProjectGitState, ProjectSnapshot};
    use std::env;
    use std::fs;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn test_dir(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        let path = env::temp_dir().join(format!("nerve-center-server-{label}-{unique}"));
        fs::create_dir_all(&path).expect("test dir should be created");
        path
    }

    #[test]
    fn builds_workspace_state_from_discovered_projects() {
        let state = build_workspace_state(vec![
            ProjectEntry {
                name: "alpha".to_string(),
                cwd: "/repos/alpha".to_string(),
                branch: "main".to_string(),
                status_summary: ProjectStatusSummary::default(),
                root_name: "alpha".to_string(),
                root_cwd: "/repos/alpha".to_string(),
                kind: ProjectKind::Root,
            },
            ProjectEntry {
                name: "feature-x".to_string(),
                cwd: "/repos/alpha-feature-x".to_string(),
                branch: "feature-x".to_string(),
                status_summary: ProjectStatusSummary::default(),
                root_name: "alpha".to_string(),
                root_cwd: "/repos/alpha".to_string(),
                kind: ProjectKind::Worktree,
            },
        ]);

        let snapshot = state.snapshot(99);
        assert_eq!(snapshot.project_order.len(), 2);
        assert_eq!(
            snapshot.projects[snapshot.project_order[0].as_str()].root_id,
            snapshot.project_order[0]
        );
        assert_eq!(
            snapshot.projects[snapshot.project_order[1].as_str()].root_id,
            snapshot.project_order[0]
        );
    }

    #[test]
    fn startup_applies_drained_agent_events_into_workspace_state() {
        let _env_lock = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let data_dir = test_dir("startup-drain");

        unsafe {
            env::set_var("NERVE_CENTER_DATA_DIR", &data_dir);
        }

        append_spool_record(&crate::daemon::agent::AgentEvent {
            project_id: "/repos/alpha".to_string(),
            runtime: "opencode".to_string(),
            pane_id: 44,
            state: "working".to_string(),
            awaiting_user: Some("question".to_string()),
        })
        .expect("spool append should succeed");

        let state = load_workspace_state(vec![ProjectEntry {
            name: "alpha".to_string(),
            cwd: "/repos/alpha".to_string(),
            branch: "main".to_string(),
            status_summary: ProjectStatusSummary::default(),
            root_name: "alpha".to_string(),
            root_cwd: "/repos/alpha".to_string(),
            kind: ProjectKind::Root,
        }])
        .expect("workspace state load should succeed");

        unsafe {
            env::remove_var("NERVE_CENTER_DATA_DIR");
        }

        let snapshot = state.snapshot(99);
        assert_eq!(snapshot.project_order, vec!["/repos/alpha"]);
        assert_eq!(snapshot.projects["/repos/alpha"].agents.len(), 1);
        assert_eq!(
            snapshot.projects["/repos/alpha"].agents[0].status,
            "needs_input"
        );
    }

    #[test]
    fn startup_restores_interrupted_journal_into_project_operation_state() {
        let _env_lock = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let data_dir = test_dir("startup-journal-restore");
        let journal_path = data_dir.join("operation-journal.jsonl");

        unsafe {
            env::set_var("NERVE_CENTER_DATA_DIR", &data_dir);
        }

        fs::write(
            &journal_path,
            "{\"operation_id\":\"op-9\",\"root_id\":\"/repos/alpha\",\"phase\":\"running\"}\n",
        )
        .expect("journal should be written");

        let state = load_workspace_state(vec![ProjectEntry {
            name: "alpha".to_string(),
            cwd: "/repos/alpha".to_string(),
            branch: "main".to_string(),
            status_summary: ProjectStatusSummary::default(),
            root_name: "alpha".to_string(),
            root_cwd: "/repos/alpha".to_string(),
            kind: ProjectKind::Root,
        }])
        .expect("workspace state load should succeed");

        unsafe {
            env::remove_var("NERVE_CENTER_DATA_DIR");
        }

        let snapshot = state.snapshot(99);
        assert_eq!(snapshot.projects["/repos/alpha"].operation.kind, "op-9");
        assert_eq!(
            snapshot.projects["/repos/alpha"].operation.message,
            "interrupted"
        );
    }

    #[test]
    fn serve_client_connection_answers_snapshot_and_agent_event_requests() {
        let socket_dir = test_dir("socket-loop");
        let socket_path = socket_dir.join("daemon.sock");
        let listener = UnixListener::bind(&socket_path).expect("listener should bind");
        let mut state = build_workspace_state(vec![ProjectEntry {
            name: "alpha".to_string(),
            cwd: "/repos/alpha".to_string(),
            branch: "main".to_string(),
            status_summary: ProjectStatusSummary::default(),
            root_name: "alpha".to_string(),
            root_cwd: "/repos/alpha".to_string(),
            kind: ProjectKind::Root,
        }]);

        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("server should accept client");
            serve_client_connection(stream, &mut state)
                .expect("server should handle client connection");
        });

        let mut stream = UnixStream::connect(&socket_path).expect("client should connect");
        let mut reader = BufReader::new(
            stream
                .try_clone()
                .expect("client stream clone should succeed"),
        );

        stream
            .write_all(
                encode_json_line(&ClientMessage::AgentEvent {
                    event: AgentEvent {
                        project_id: "/repos/alpha".to_string(),
                        runtime: "opencode".to_string(),
                        pane_id: 9,
                        state: "working".to_string(),
                        awaiting_user: None,
                    },
                })
                .unwrap()
                .as_bytes(),
            )
            .expect("agent event should be written");
        stream.flush().expect("agent event should flush");

        let mut line = String::new();
        reader
            .read_line(&mut line)
            .expect("agent event ack should read");
        let ack = serde_json::from_str::<ServerMessage>(&line).expect("ack should decode");
        assert_eq!(
            ack,
            ServerMessage::Event {
                event: ServerEvent::Updated
            }
        );

        line.clear();
        stream
            .write_all(
                encode_json_line(&ClientMessage::GetSnapshot)
                    .unwrap()
                    .as_bytes(),
            )
            .expect("snapshot request should be written");
        stream.flush().expect("snapshot request should flush");
        reader
            .read_line(&mut line)
            .expect("snapshot response should read");

        let response =
            serde_json::from_str::<ServerMessage>(&line).expect("snapshot should decode");
        match response {
            ServerMessage::Snapshot { snapshot } => {
                assert_eq!(snapshot.projects["/repos/alpha"].agents.len(), 1);
                assert_eq!(
                    snapshot.projects["/repos/alpha"].agents[0].status,
                    "working"
                );
            }
            other => panic!("unexpected server response: {other:?}"),
        }

        drop(reader);
        drop(stream);
        server.join().expect("server thread should finish");
    }

    #[test]
    fn failed_post_operation_refresh_marks_last_known_good_state_as_stale() {
        let _env_lock = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let sandbox = test_dir("stale-refresh");
        let data_dir = test_dir("stale-refresh-state");
        let home_dir = test_dir("stale-refresh-home");
        let config_dir = home_dir.join(".config/nerve_center");
        let remote = sandbox.join("remote.git");
        let root = sandbox.join("root");

        fs::create_dir_all(&config_dir).expect("config dir should be created");
        fs::write(
            config_dir.join("config.toml"),
            "repo_sources = [\"~/missing\"]\n",
        )
        .expect("config should be written");

        unsafe {
            env::set_var("NERVE_CENTER_DATA_DIR", &data_dir);
            env::set_var("HOME", &home_dir);
        }

        git(&sandbox, &["init", "--bare", path_as_str(&remote)]);
        git(
            &sandbox,
            &["clone", path_as_str(&remote), path_as_str(&root)],
        );
        git(&root, &["switch", "-c", "main"]);
        fs::write(root.join("tracked.txt"), "hello\n").expect("tracked file should be written");
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
        git(&root, &["push", "-u", "origin", "main"]);

        let project_id = path_as_str(&root).to_string();
        let mut state = WorkspaceState::default();
        state.project_order.push(project_id.clone());
        state.projects.insert(
            project_id.clone(),
            ProjectSnapshot {
                id: project_id.clone(),
                name: "root".to_string(),
                cwd: project_id.clone(),
                root_id: project_id.clone(),
                git: ProjectGitState {
                    branch: "main".to_string(),
                    status: "clean".to_string(),
                    status_summary: ProjectStatusSummary::default(),
                },
                ..ProjectSnapshot::default()
            },
        );

        let message = handle_workspace_client_message(
            &mut state,
            ClientMessage::RunOperation {
                project_id: project_id.clone(),
                operation: "git_pull".to_string(),
                command: "git pull".to_string(),
            },
        )
        .expect("git pull operation should still succeed");

        unsafe {
            env::remove_var("NERVE_CENTER_DATA_DIR");
            env::remove_var("HOME");
        }

        assert_eq!(
            message,
            Some(ServerMessage::Event {
                event: ServerEvent::OperationFinished {
                    project_id: project_id.clone(),
                    operation: "git_pull".to_string(),
                }
            })
        );
        let snapshot = state.snapshot(99);
        assert_eq!(
            snapshot.projects[&project_id]
                .freshness
                .stale_reason
                .as_deref(),
            Some("refresh_failed")
        );

        let app = App::from_snapshot(snapshot);
        assert_eq!(app.project_stale_reason(0), Some("refresh_failed"));
    }

    #[test]
    fn builds_unique_project_ids_from_paths() {
        let state = build_workspace_state(vec![
            ProjectEntry {
                name: "alpha".to_string(),
                cwd: "/repos/source-a/alpha".to_string(),
                branch: "main".to_string(),
                status_summary: ProjectStatusSummary::default(),
                root_name: "alpha".to_string(),
                root_cwd: "/repos/source-a/alpha".to_string(),
                kind: ProjectKind::Root,
            },
            ProjectEntry {
                name: "alpha".to_string(),
                cwd: "/repos/source-b/alpha".to_string(),
                branch: "main".to_string(),
                status_summary: ProjectStatusSummary::default(),
                root_name: "alpha".to_string(),
                root_cwd: "/repos/source-b/alpha".to_string(),
                kind: ProjectKind::Root,
            },
        ]);

        let snapshot = state.snapshot(1);
        assert_eq!(
            snapshot.project_order,
            vec!["/repos/source-a/alpha", "/repos/source-b/alpha"]
        );
        assert!(snapshot.projects.contains_key("/repos/source-a/alpha"));
        assert!(snapshot.projects.contains_key("/repos/source-b/alpha"));
    }

    #[test]
    fn second_run_operation_request_for_same_root_returns_error() {
        let mut state = conflict_state();
        state.try_start_operation("root:alpha", "op-1").unwrap();

        let error = handle_workspace_client_message(
            &mut state,
            ClientMessage::RunOperation {
                project_id: "root:worktree".to_string(),
                operation: "wt_land".to_string(),
                command: "wt land".to_string(),
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("already has running operation"));
    }

    #[test]
    fn run_operation_message_dispatches_to_handler_and_releases_root_lock() {
        let _env_lock = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let sandbox = test_dir("git-pull");
        let data_dir = test_dir("git-pull-state");
        let home_dir = test_dir("git-pull-home");
        let config_dir = home_dir.join(".config/nerve_center");
        let remote = sandbox.join("remote.git");
        let root = sandbox.join("root");

        fs::create_dir_all(&config_dir).expect("config dir should be created");
        fs::write(
            config_dir.join("config.toml"),
            format!("repo_sources = [\"{}\"]\n", sandbox.display()),
        )
        .expect("config should be written");

        unsafe {
            env::set_var("NERVE_CENTER_DATA_DIR", &data_dir);
            env::set_var("HOME", &home_dir);
        }

        git(&sandbox, &["init", "--bare", path_as_str(&remote)]);
        git(
            &sandbox,
            &["clone", path_as_str(&remote), path_as_str(&root)],
        );
        git(&root, &["switch", "-c", "main"]);
        fs::write(root.join("tracked.txt"), "hello\n").expect("tracked file should be written");
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
        git(&root, &["push", "-u", "origin", "main"]);

        let project_id = path_as_str(&root).to_string();
        let mut state = WorkspaceState::default();
        state.project_order.push(project_id.clone());
        state.projects.insert(
            project_id.clone(),
            ProjectSnapshot {
                id: project_id.clone(),
                name: "root".to_string(),
                cwd: project_id.clone(),
                root_id: project_id.clone(),
                git: ProjectGitState {
                    branch: "main".to_string(),
                    status: "clean".to_string(),
                    status_summary: ProjectStatusSummary::default(),
                },
                ..ProjectSnapshot::default()
            },
        );

        let message = handle_workspace_client_message(
            &mut state,
            ClientMessage::RunOperation {
                project_id: project_id.clone(),
                operation: "git_pull".to_string(),
                command: "git pull".to_string(),
            },
        )
        .expect("git pull operation should succeed");

        assert_eq!(
            message,
            Some(ServerMessage::Event {
                event: ServerEvent::OperationFinished {
                    project_id: project_id.clone(),
                    operation: "git_pull".to_string(),
                }
            })
        );
        assert!(state.running_operations_by_root.is_empty());
        assert_eq!(state.projects[&project_id].operation, Default::default());

        unsafe {
            env::remove_var("NERVE_CENTER_DATA_DIR");
            env::remove_var("HOME");
        }
    }

    #[test]
    fn failed_running_journal_append_clears_root_lock_and_visible_operation_state() {
        let _env_lock = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let sandbox = test_dir("journal-append-failure-clears-root-lock");
        let blocked_data_dir = sandbox.join("blocked");
        fs::write(&blocked_data_dir, "not a directory").expect("blocker file should exist");
        let project_id = "/repos/alpha".to_string();
        let mut state = WorkspaceState::default();
        state.project_order.push(project_id.clone());
        state.projects.insert(
            project_id.clone(),
            ProjectSnapshot {
                id: project_id.clone(),
                name: "alpha".to_string(),
                cwd: project_id.clone(),
                root_id: project_id.clone(),
                kind: ProjectKind::Root,
                ..ProjectSnapshot::default()
            },
        );

        unsafe {
            env::set_var("NERVE_CENTER_DATA_DIR", &blocked_data_dir);
        }

        let error = handle_workspace_client_message(
            &mut state,
            ClientMessage::RunOperation {
                project_id: project_id.clone(),
                operation: "git_pull".to_string(),
                command: "git pull".to_string(),
            },
        )
        .unwrap_err();

        unsafe {
            env::remove_var("NERVE_CENTER_DATA_DIR");
        }

        assert!(error
            .to_string()
            .contains("failed to create operation journal dir"));
        assert!(state.running_operations_by_root.is_empty());
        assert_eq!(state.projects[&project_id].operation, Default::default());
    }

    #[test]
    fn refresh_root_request_reloads_workspace_and_updates_freshness() {
        let _env_lock = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let sandbox = test_dir("refresh-root-request");
        let home_dir = test_dir("refresh-root-home");
        let config_dir = home_dir.join(".config/nerve_center");
        let root = sandbox.join("alpha");

        fs::create_dir_all(&config_dir).expect("config dir should be created");
        fs::create_dir_all(&root).expect("root dir should be created");
        fs::write(
            config_dir.join("config.toml"),
            format!("repo_sources = [\"{}\"]\n", sandbox.display()),
        )
        .expect("config should be written");

        git(&root, &["init", "-b", "main"]);
        fs::write(root.join("tracked.txt"), "hello\n").expect("tracked file should exist");
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

        unsafe {
            env::set_var("HOME", &home_dir);
        }

        let mut state = load_workspace_state(discover_projects_in(&[sandbox.clone()]).unwrap())
            .expect("workspace should load");
        let project_id = path_as_str(&root).to_string();
        let initial_freshness = state.projects[&project_id].freshness.clone();
        fs::write(root.join("tracked.txt"), "changed\n").expect("tracked file should update");

        let response = handle_workspace_client_message(
            &mut state,
            ClientMessage::RefreshRoot {
                root_id: project_id.clone(),
            },
        )
        .expect("refresh request should succeed");

        unsafe {
            env::remove_var("HOME");
        }

        assert_eq!(
            response,
            Some(ServerMessage::Event {
                event: ServerEvent::Updated
            })
        );
        assert_eq!(state.projects[&project_id].git.status_summary.modified, 1);
        assert_eq!(state.projects[&project_id].freshness.state, "fresh");
        assert!(
            state.projects[&project_id].freshness.updated_at_ms > initial_freshness.updated_at_ms
        );
    }

    #[test]
    fn slow_run_operation_does_not_block_snapshot_reads() {
        let _env_lock = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let data_dir = test_dir("slow-run-operation-journal");
        unsafe {
            env::set_var("NERVE_CENTER_DATA_DIR", &data_dir);
        }

        let shared = SharedState::from_workspace_state(conflict_state());

        let barrier = Arc::new(std::sync::Barrier::new(2));
        let barrier_for_runner = Arc::clone(&barrier);
        let slow_operation = thread::spawn({
            let shared = shared.clone();
            move || {
                process_client_message_with_dependencies(
                    &shared,
                    ClientMessage::RunOperation {
                        project_id: "root:worktree".to_string(),
                        operation: "wt_land".to_string(),
                        command: "wt land".to_string(),
                    },
                    move |_, _, _, _| {
                        barrier_for_runner.wait();
                        thread::sleep(std::time::Duration::from_millis(300));
                        Ok(())
                    },
                )
            }
        });

        barrier.wait();
        let started = std::time::Instant::now();
        let snapshot = process_client_message_with_dependencies(
            &shared,
            ClientMessage::GetSnapshot,
            |_, _, _, _| Ok(()),
        )
        .expect("snapshot request should succeed");
        let elapsed = started.elapsed();

        let operation_response = slow_operation.join().expect("slow operation should finish");

        match snapshot {
            ServerMessage::Snapshot { .. } => {}
            other => panic!("unexpected snapshot response: {other:?}"),
        }
        assert!(elapsed < std::time::Duration::from_millis(150));
        assert!(
            matches!(
                operation_response,
                Ok(ServerMessage::Event {
                    event: ServerEvent::Updated
                })
            ),
            "unexpected operation response: {operation_response:?}"
        );

        unsafe {
            env::remove_var("NERVE_CENTER_DATA_DIR");
        }
    }

    #[test]
    fn run_operation_request_returns_before_background_work_finishes() {
        let _env_lock = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let data_dir = test_dir("async-run-operation-journal");
        unsafe {
            env::set_var("NERVE_CENTER_DATA_DIR", &data_dir);
        }

        let shared = SharedState::from_workspace_state(conflict_state());
        let (subscriber, updates) = std::sync::mpsc::channel();
        shared.add_subscriber(subscriber);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let finish_signal = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let (response_tx, response_rx) = std::sync::mpsc::channel();

        thread::spawn({
            let shared = shared.clone();
            let finish_signal = Arc::clone(&finish_signal);
            move || {
                let response = process_client_message_with_dependencies(
                    &shared,
                    ClientMessage::RunOperation {
                        project_id: "root:worktree".to_string(),
                        operation: "wt_land".to_string(),
                        command: "wt land".to_string(),
                    },
                    move |_, _, _, _| {
                        started_tx
                            .send(())
                            .expect("runner start should notify test");
                        let (lock, wakeup) = &*finish_signal;
                        let mut finished =
                            lock.lock().expect("finish signal lock should not poison");
                        while !*finished {
                            finished = wakeup
                                .wait(finished)
                                .expect("finish signal wait should not fail");
                        }
                        Ok(())
                    },
                );
                response_tx
                    .send(response)
                    .expect("response should be sent back to test");
            }
        });

        let running_snapshot = updates
            .recv_timeout(std::time::Duration::from_millis(150))
            .expect("subscription should see running state promptly");
        assert_eq!(
            running_snapshot.projects["root:worktree"].operation.kind,
            "wt_land"
        );
        assert_eq!(
            running_snapshot.projects["root:worktree"].operation.message,
            "running"
        );
        started_rx
            .recv_timeout(std::time::Duration::from_millis(150))
            .expect("runner should start in background");

        let response = response_rx
            .recv_timeout(std::time::Duration::from_millis(150))
            .expect("run operation request should return before background work finishes");
        assert_eq!(
            response.expect("run operation request should succeed"),
            ServerMessage::Event {
                event: ServerEvent::Updated
            }
        );

        match process_client_message_with_dependencies(
            &shared,
            ClientMessage::GetSnapshot,
            |_, _, _, _| Ok(()),
        )
        .expect("snapshot request should succeed while operation is running")
        {
            ServerMessage::Snapshot { snapshot } => {
                assert_eq!(snapshot.projects["root:worktree"].operation.kind, "wt_land");
            }
            other => panic!("unexpected snapshot response: {other:?}"),
        }

        let (lock, wakeup) = &*finish_signal;
        *lock.lock().expect("finish signal lock should not poison") = true;
        wakeup.notify_one();
        let completed_snapshot = updates
            .recv_timeout(std::time::Duration::from_millis(150))
            .expect("subscription should see completion state promptly");
        assert_eq!(
            completed_snapshot.projects["root:worktree"].operation,
            Default::default()
        );
        assert_eq!(
            completed_snapshot.projects["root:worktree"]
                .freshness
                .stale_reason
                .as_deref(),
            Some("refresh_pending")
        );

        unsafe {
            env::remove_var("NERVE_CENTER_DATA_DIR");
        }
    }

    #[test]
    fn idle_refresh_backstop_uses_production_cadence() {
        assert!(super::idle_refresh_backstop() >= std::time::Duration::from_secs(10));
    }

    #[test]
    fn slow_refresh_root_does_not_block_snapshot_reads() {
        let _env_lock = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let sandbox = test_dir("slow-refresh-root");
        let home_dir = test_dir("slow-refresh-root-home");
        let config_dir = home_dir.join(".config/nerve_center");
        let fake_bin = test_dir("slow-refresh-root-bin");
        let fake_git = fake_bin.join("git");
        let root = sandbox.join("alpha");
        let original_path = env::var("PATH").expect("PATH should be set");
        let actual_git = Command::new("which")
            .arg("git")
            .output()
            .expect("which git should run");
        let actual_git = String::from_utf8(actual_git.stdout)
            .expect("git path should be utf8")
            .trim()
            .to_string();

        fs::create_dir_all(&config_dir).expect("config dir should be created");
        fs::create_dir_all(&root).expect("repo dir should be created");
        fs::create_dir_all(&fake_bin).expect("fake bin dir should be created");
        fs::write(
            config_dir.join("config.toml"),
            format!("repo_sources = [\"{}\"]\n", sandbox.display()),
        )
        .expect("config should be written");
        fs::write(
            &fake_git,
            format!("#!/bin/sh\nsleep 0.3\nexec \"{actual_git}\" \"$@\"\n"),
        )
        .expect("fake git should be written");
        fs::set_permissions(&fake_git, fs::Permissions::from_mode(0o755))
            .expect("fake git should be executable");

        git(&root, &["init", "-b", "main"]);
        fs::write(root.join("tracked.txt"), "hello\n").expect("tracked file should exist");
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

        unsafe {
            env::set_var("HOME", &home_dir);
            env::set_var("PATH", format!("{}:{original_path}", fake_bin.display()));
        }

        let shared = SharedState::from_workspace_state(
            load_workspace_state(discover_projects_in(&[sandbox.clone()]).unwrap())
                .expect("workspace should load"),
        );
        let root_id = path_as_str(&root).to_string();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let barrier_for_refresh = Arc::clone(&barrier);

        let refresh_thread = thread::spawn({
            let shared = shared.clone();
            let root_id = root_id.clone();
            move || {
                barrier_for_refresh.wait();
                process_client_message_with_dependencies(
                    &shared,
                    ClientMessage::RefreshRoot { root_id },
                    |_, _, _, _| Ok(()),
                )
            }
        });

        barrier.wait();
        let started = std::time::Instant::now();
        let snapshot = process_client_message_with_dependencies(
            &shared,
            ClientMessage::GetSnapshot,
            |_, _, _, _| Ok(()),
        )
        .expect("snapshot request should succeed during refresh");
        let elapsed = started.elapsed();
        let refresh_response = refresh_thread.join().expect("refresh thread should join");

        unsafe {
            env::set_var("PATH", original_path);
            env::remove_var("HOME");
        }

        match snapshot {
            ServerMessage::Snapshot { .. } => {}
            other => panic!("unexpected snapshot response: {other:?}"),
        }
        assert!(elapsed < std::time::Duration::from_millis(150));
        assert_eq!(
            refresh_response.expect("refresh request should succeed"),
            ServerMessage::Event {
                event: ServerEvent::Updated
            }
        );
    }

    #[test]
    fn subscribe_keeps_stream_open_and_pushes_snapshot_updates() {
        let shared = SharedState::from_workspace_state(conflict_state());
        let (server_stream, mut client_stream) =
            UnixStream::pair().expect("stream pair should open");

        let server = thread::spawn({
            let shared = shared.clone();
            move || super::serve_shared_client_connection(server_stream, &shared)
        });

        let mut reader = BufReader::new(
            client_stream
                .try_clone()
                .expect("client stream clone should succeed"),
        );

        client_stream
            .write_all(
                encode_json_line(&ClientMessage::Hello {
                    client_name: "tui".to_string(),
                })
                .unwrap()
                .as_bytes(),
            )
            .expect("hello should write");
        client_stream.flush().expect("hello should flush");

        let mut line = String::new();
        reader.read_line(&mut line).expect("hello should read");
        assert_eq!(
            serde_json::from_str::<ServerMessage>(&line).expect("hello should decode"),
            ServerMessage::Hello {
                protocol_version: 1,
                server_name: "nerve_centerd".to_string(),
            }
        );

        line.clear();
        client_stream
            .write_all(
                encode_json_line(&ClientMessage::Subscribe)
                    .unwrap()
                    .as_bytes(),
            )
            .expect("subscribe should write");
        client_stream.flush().expect("subscribe should flush");
        reader
            .read_line(&mut line)
            .expect("initial subscription snapshot should read");

        match serde_json::from_str::<ServerMessage>(&line).expect("snapshot should decode") {
            ServerMessage::Snapshot { snapshot } => {
                assert!(snapshot.projects.contains_key("root:alpha"));
            }
            other => panic!("unexpected subscribe response: {other:?}"),
        }

        process_client_message_with_dependencies(
            &shared,
            ClientMessage::AgentEvent {
                event: AgentEvent {
                    project_id: "root:alpha".to_string(),
                    runtime: "opencode".to_string(),
                    pane_id: 77,
                    state: "working".to_string(),
                    awaiting_user: Some("question".to_string()),
                },
            },
            |_, _, _, _| Ok(()),
        )
        .expect("agent event should succeed");

        line.clear();
        reader
            .read_line(&mut line)
            .expect("subscription update should read");
        match serde_json::from_str::<ServerMessage>(&line).expect("update should decode") {
            ServerMessage::Snapshot { snapshot } => {
                assert_eq!(snapshot.projects["root:alpha"].agents.len(), 1);
                assert_eq!(
                    snapshot.projects["root:alpha"].agents[0].status,
                    "needs_input"
                );
            }
            other => panic!("unexpected subscription update: {other:?}"),
        }

        drop(reader);
        drop(client_stream);
        shared.notify_snapshot();
        server
            .join()
            .expect("server should join")
            .expect("server should serve subscription client");
    }

    #[test]
    fn shutdown_message_sets_exit_signal() {
        let shared = SharedState::from_workspace_state(WorkspaceState::default());

        let response = process_client_message_with_dependencies(
            &shared,
            ClientMessage::Shutdown,
            |_, _, _, _| Ok(()),
        )
        .unwrap();

        assert_eq!(
            response,
            ServerMessage::Event {
                event: ServerEvent::Updated,
            }
        );
        assert!(shared.shutdown_requested());
    }

    #[test]
    fn shutdown_returns_error_while_operation_is_running() {
        let _env_lock = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let data_dir = test_dir("shutdown-blocked");
        unsafe {
            env::set_var("NERVE_CENTER_DATA_DIR", &data_dir);
        }

        let shared = SharedState::from_workspace_state(conflict_state());
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let finish_signal = Arc::new((Mutex::new(false), std::sync::Condvar::new()));

        let operation = thread::spawn({
            let shared = shared.clone();
            let finish_signal = Arc::clone(&finish_signal);
            move || {
                process_client_message_with_dependencies(
                    &shared,
                    ClientMessage::RunOperation {
                        project_id: "root:worktree".to_string(),
                        operation: "wt_land".to_string(),
                        command: "wt land".to_string(),
                    },
                    move |_, _, _, _| {
                        started_tx
                            .send(())
                            .expect("runner start should notify test");
                        let (lock, wakeup) = &*finish_signal;
                        let mut finished =
                            lock.lock().expect("finish signal lock should not poison");
                        while !*finished {
                            finished = wakeup
                                .wait(finished)
                                .expect("finish signal wait should not fail");
                        }
                        Ok(())
                    },
                )
            }
        });

        started_rx
            .recv_timeout(std::time::Duration::from_millis(150))
            .expect("runner should start in background");

        let response = server_message_for_client_message(&shared, ClientMessage::Shutdown);

        let (lock, wakeup) = &*finish_signal;
        *lock.lock().expect("finish signal lock should not poison") = true;
        wakeup.notify_one();
        operation
            .join()
            .expect("operation thread should join")
            .expect("operation request should succeed");

        unsafe {
            env::remove_var("NERVE_CENTER_DATA_DIR");
        }

        assert!(
            matches!(response, ServerMessage::Error { .. }),
            "unexpected response: {response:?}"
        );
        match response {
            ServerMessage::Error { message } => {
                assert!(message.contains("shutdown"), "unexpected error: {message}");
                assert!(message.contains("running"), "unexpected error: {message}");
            }
            other => panic!("unexpected shutdown response: {other:?}"),
        }
        assert!(!shared.shutdown_requested());
    }

    #[test]
    fn run_operation_is_rejected_after_shutdown_is_requested() {
        let shared = SharedState::from_workspace_state(conflict_state());

        let shutdown_response = process_client_message_with_dependencies(
            &shared,
            ClientMessage::Shutdown,
            |_, _, _, _| Ok(()),
        )
        .expect("shutdown request should succeed");
        assert_eq!(
            shutdown_response,
            ServerMessage::Event {
                event: ServerEvent::Updated,
            }
        );

        let response = server_message_for_client_message(
            &shared,
            ClientMessage::RunOperation {
                project_id: "root:worktree".to_string(),
                operation: "wt_land".to_string(),
                command: "wt land".to_string(),
            },
        );

        match response {
            ServerMessage::Error { message } => {
                assert_eq!(
                    message,
                    "daemon is shutting down; only get_snapshot and shutdown are accepted"
                );
            }
            other => panic!("unexpected post-shutdown response: {other:?}"),
        }
    }

    #[test]
    fn shutdown_request_causes_server_loop_to_finish() {
        let socket_dir = test_dir("shutdown-listener");
        let socket_path = socket_dir.join("daemon.sock");
        let listener = UnixListener::bind(&socket_path).expect("test listener should bind");
        let shared = SharedState::from_workspace_state(WorkspaceState::default());

        let server = thread::spawn({
            let shared = shared.clone();
            move || super::run_listener_until_shutdown(&listener, &shared)
        });

        thread::sleep(std::time::Duration::from_millis(50));
        shared.request_shutdown();

        server
            .join()
            .expect("listener thread should join")
            .expect("listener loop should exit cleanly");
    }

    #[test]
    fn request_shutdown_wakes_refresh_waiters() {
        let shared = SharedState::from_workspace_state(WorkspaceState::default());
        let shutdown = shared.clone();

        let waiter = thread::spawn(move || {
            let queue = shared
                .refresh
                .queue
                .lock()
                .expect("refresh queue lock should not be poisoned");
            let (_guard, timeout) = shared
                .refresh
                .wakeup
                .wait_timeout(queue, std::time::Duration::from_millis(100))
                .expect("refresh queue wait should succeed");
            timeout.timed_out()
        });

        thread::sleep(std::time::Duration::from_millis(20));
        shutdown.request_shutdown();

        assert!(!waiter.join().expect("waiter thread should join"));
    }

    #[test]
    fn next_refresh_roots_returns_none_after_shutdown_request() {
        let shared = SharedState::from_workspace_state(WorkspaceState::default());
        let worker_state = shared.clone();

        let waiter = thread::spawn(move || super::next_refresh_roots(&worker_state));

        thread::sleep(std::time::Duration::from_millis(20));
        shared.request_shutdown();

        assert_eq!(waiter.join().expect("waiter thread should join"), None);
    }

    #[test]
    fn daemon_socket_listener_cleans_up_socket_on_drop() {
        let socket_dir = test_dir("socket-cleanup");
        let socket_path = socket_dir.join("daemon.sock");
        let listener = super::SocketListenerGuard::bind(socket_path.clone())
            .expect("socket listener should bind");
        assert!(socket_path.exists());

        drop(listener);

        assert!(!socket_path.exists());
    }

    #[test]
    fn non_run_operation_messages_do_not_emit_server_events() {
        let mut state = WorkspaceState::default();

        let message = handle_workspace_client_message(&mut state, ClientMessage::GetSnapshot)
            .expect("non-operation message should be ignored by this seam");

        assert_eq!(message, None);
    }

    #[test]
    fn rejects_worktree_only_operations_for_root_projects() {
        let _env_lock = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let data_dir = test_dir("reject-worktree-ops");
        let mut state = conflict_state();

        unsafe {
            env::set_var("NERVE_CENTER_DATA_DIR", &data_dir);
        }

        for (operation, command) in [
            ("wt_remove", "wt remove"),
            ("wt_merge", "wt merge"),
            ("wt_pr", "wt pr"),
            ("wt_land", "wt land"),
        ] {
            let error = handle_workspace_client_message(
                &mut state,
                ClientMessage::RunOperation {
                    project_id: "root:alpha".to_string(),
                    operation: operation.to_string(),
                    command: command.to_string(),
                },
            )
            .unwrap_err();
            assert!(error.to_string().contains("requires a worktree project"));
        }

        unsafe {
            env::remove_var("NERVE_CENTER_DATA_DIR");
        }
    }

    #[test]
    fn rejects_root_only_operations_for_worktree_projects() {
        let _env_lock = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let data_dir = test_dir("reject-root-ops");
        let mut state = conflict_state();

        unsafe {
            env::set_var("NERVE_CENTER_DATA_DIR", &data_dir);
        }

        for (operation, command) in [
            ("git_switch", "git switch feature/test"),
            ("git_pull", "git pull"),
        ] {
            let error = handle_workspace_client_message(
                &mut state,
                ClientMessage::RunOperation {
                    project_id: "root:worktree".to_string(),
                    operation: operation.to_string(),
                    command: command.to_string(),
                },
            )
            .unwrap_err();
            assert!(error.to_string().contains("requires a root project"));
        }

        unsafe {
            env::remove_var("NERVE_CENTER_DATA_DIR");
        }
    }

    fn conflict_state() -> WorkspaceState {
        let mut state = WorkspaceState::default();
        state.project_order.push("root:worktree".to_string());
        state.projects.insert(
            "root:worktree".to_string(),
            ProjectSnapshot {
                id: "root:worktree".to_string(),
                name: "feature".to_string(),
                cwd: "/tmp/root.worktree".to_string(),
                root_id: "root:alpha".to_string(),
                kind: ProjectKind::Worktree,
                git: ProjectGitState {
                    branch: "feature".to_string(),
                    status: "clean".to_string(),
                    status_summary: ProjectStatusSummary::default(),
                },
                ..ProjectSnapshot::default()
            },
        );
        state.projects.insert(
            "root:alpha".to_string(),
            ProjectSnapshot {
                id: "root:alpha".to_string(),
                name: "alpha".to_string(),
                cwd: "/tmp/root".to_string(),
                root_id: "root:alpha".to_string(),
                kind: ProjectKind::Root,
                git: ProjectGitState {
                    branch: "main".to_string(),
                    status: "clean".to_string(),
                    status_summary: ProjectStatusSummary::default(),
                },
                ..ProjectSnapshot::default()
            },
        );
        state
    }

    fn git(workdir: &std::path::Path, args: &[&str]) {
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

    fn path_as_str(path: &std::path::Path) -> &str {
        path.to_str().expect("path should be valid utf-8")
    }
}
