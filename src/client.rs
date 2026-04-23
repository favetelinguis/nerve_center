#[cfg(test)]
use std::collections::VecDeque;
use std::env;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

use crate::daemon::agent::AgentEvent;
use crate::daemon::protocol::{ClientMessage, ServerMessage};
use crate::workspace::WorkspaceSnapshot;

pub struct Client {
    transport: ClientTransport,
}

pub struct Subscription {
    transport: SubscriptionTransport,
}

enum ClientTransport {
    Unix(UnixStream),
    #[cfg(test)]
    Stub {
        sent_messages: Vec<ClientMessage>,
        snapshot: WorkspaceSnapshot,
        snapshots: VecDeque<WorkspaceSnapshot>,
    },
}

enum SubscriptionTransport {
    Live {
        receiver: Receiver<Result<WorkspaceSnapshot, String>>,
    },
    #[cfg(test)]
    Stub {
        pending: VecDeque<WorkspaceSnapshot>,
    },
}

impl Client {
    fn connect() -> Result<Self> {
        let mut stream = connect_socket()?;
        perform_hello(&mut stream, "nerve_center")?;
        Ok(Self {
            transport: ClientTransport::Unix(stream),
        })
    }

    pub fn connect_or_spawn() -> Result<Self> {
        let mut stream =
            connect_with_autostart(connect_socket, spawn_daemon_process, wait_for_socket)?;
        perform_hello(&mut stream, "nerve_center")?;

        Ok(Self {
            transport: ClientTransport::Unix(stream),
        })
    }

    pub fn connect_subscription_or_spawn() -> Result<(WorkspaceSnapshot, Subscription)> {
        let mut stream =
            connect_with_autostart(connect_socket, spawn_daemon_process, wait_for_socket)?;
        perform_hello(&mut stream, "nerve_center-tui")?;
        write_client_message(&mut stream, &ClientMessage::Subscribe)?;

        let mut reader = BufReader::new(stream);
        let initial_snapshot = read_subscription_snapshot(&mut reader)?;
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || read_subscription_updates(reader, sender));

        Ok((
            initial_snapshot,
            Subscription {
                transport: SubscriptionTransport::Live { receiver },
            },
        ))
    }

    pub fn reconnect(&mut self) -> Result<WorkspaceSnapshot> {
        if matches!(&self.transport, ClientTransport::Unix(_)) {
            let mut stream =
                connect_with_autostart(connect_socket, spawn_daemon_process, wait_for_socket)?;
            perform_hello(&mut stream, "nerve_center")?;
            self.transport = ClientTransport::Unix(stream);
        }
        self.get_snapshot()
    }

    pub fn get_snapshot(&mut self) -> Result<WorkspaceSnapshot> {
        match &mut self.transport {
            ClientTransport::Unix(stream) => {
                match send_round_trip(stream, &ClientMessage::GetSnapshot)
                    .context("failed to fetch daemon snapshot")?
                {
                    ServerMessage::Snapshot { snapshot } => Ok(snapshot),
                    ServerMessage::Error { message } => Err(anyhow!(message)),
                    message => Err(anyhow!("unexpected daemon response: {message:?}")),
                }
            }
            #[cfg(test)]
            ClientTransport::Stub {
                snapshot,
                snapshots,
                ..
            } => {
                if let Some(next) = snapshots.pop_front() {
                    *snapshot = next;
                }
                Ok(snapshot.clone())
            }
        }
    }

    pub fn send(&mut self, message: ClientMessage) -> Result<()> {
        match &mut self.transport {
            ClientTransport::Unix(stream) => {
                match send_round_trip(stream, &message).context("failed to send daemon request")? {
                    ServerMessage::Error { message } => Err(anyhow!(message)),
                    _ => Ok(()),
                }
            }
            #[cfg(test)]
            ClientTransport::Stub { sent_messages, .. } => {
                sent_messages.push(message);
                Ok(())
            }
        }
    }

    #[cfg(test)]
    pub fn sent_messages(&self) -> &[ClientMessage] {
        match &self.transport {
            ClientTransport::Stub { sent_messages, .. } => sent_messages,
            ClientTransport::Unix(_) => panic!("sent_messages is only available on stub clients"),
        }
    }

    #[cfg(test)]
    pub fn stub() -> Self {
        Self::stub_with_snapshot(WorkspaceSnapshot {
            protocol_version: 1,
            generated_at_ms: 0,
            projects: Default::default(),
            project_order: Vec::new(),
            warnings: Vec::new(),
        })
    }

    #[cfg(test)]
    pub fn stub_with_snapshot(snapshot: WorkspaceSnapshot) -> Self {
        Self {
            transport: ClientTransport::Stub {
                sent_messages: Vec::new(),
                snapshot,
                snapshots: VecDeque::new(),
            },
        }
    }

    #[cfg(test)]
    pub fn stub_with_snapshots(snapshots: Vec<WorkspaceSnapshot>) -> Self {
        let snapshots = VecDeque::from(snapshots);
        let snapshot = WorkspaceSnapshot {
            protocol_version: 1,
            generated_at_ms: 0,
            projects: Default::default(),
            project_order: Vec::new(),
            warnings: Vec::new(),
        };
        Self {
            transport: ClientTransport::Stub {
                sent_messages: Vec::new(),
                snapshot,
                snapshots,
            },
        }
    }
}

impl Subscription {
    pub fn try_recv_latest(&mut self) -> Result<Option<WorkspaceSnapshot>> {
        match &mut self.transport {
            SubscriptionTransport::Live { receiver } => {
                let mut latest = None;
                loop {
                    match receiver.try_recv() {
                        Ok(Ok(snapshot)) => latest = Some(snapshot),
                        Ok(Err(error)) => return Err(anyhow!(error)),
                        Err(TryRecvError::Empty) => return Ok(latest),
                        Err(TryRecvError::Disconnected) => {
                            return Err(anyhow!("daemon subscription disconnected"))
                        }
                    }
                }
            }
            #[cfg(test)]
            SubscriptionTransport::Stub { pending } => Ok(pending.pop_back()),
        }
    }

    #[cfg(test)]
    pub fn stub_with_updates(updates: Vec<WorkspaceSnapshot>) -> Self {
        Self {
            transport: SubscriptionTransport::Stub {
                pending: VecDeque::from(updates),
            },
        }
    }
}

fn connect_socket() -> Result<UnixStream> {
    let path = daemon_socket_path()?;
    UnixStream::connect(&path)
        .with_context(|| format!("failed to connect to daemon socket {}", path.display()))
}

fn connect_with_autostart<C, S, W>(connect: C, spawn: S, wait: W) -> Result<UnixStream>
where
    C: FnOnce() -> Result<UnixStream>,
    S: FnOnce() -> Result<()>,
    W: FnOnce() -> Result<UnixStream>,
{
    match connect() {
        Ok(stream) => Ok(stream),
        Err(initial_error) => {
            if let Ok(stream) = wait_for_existing_socket() {
                return Ok(stream);
            }
            spawn()?;
            wait().with_context(|| {
                format!("failed to connect to daemon after spawn attempt: {initial_error}")
            })
        }
    }
}

fn wait_for_existing_socket() -> Result<UnixStream> {
    let mut last_error = None;
    for _ in 0..5 {
        match connect_socket() {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
        thread::sleep(Duration::from_millis(25));
    }

    Err(last_error.unwrap_or_else(|| anyhow!("daemon did not become reachable")))
}

fn wait_for_socket() -> Result<UnixStream> {
    let mut last_error = None;
    for _ in 0..50 {
        match connect_socket() {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
        thread::sleep(Duration::from_millis(50));
    }

    Err(last_error.unwrap_or_else(|| anyhow!("daemon did not become reachable")))
}

fn spawn_daemon_process() -> Result<()> {
    let exe = current_executable()?;
    Command::new(exe)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to spawn nerve_center daemon")?;
    Ok(())
}

fn current_executable() -> Result<OsString> {
    Ok(env::current_exe()
        .context("failed to resolve current executable")?
        .into_os_string())
}

fn daemon_socket_path() -> Result<PathBuf> {
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

fn perform_hello(stream: &mut UnixStream, client_name: &str) -> Result<()> {
    match send_round_trip(
        stream,
        &ClientMessage::Hello {
            client_name: client_name.to_string(),
        },
    )
    .context("failed to perform daemon hello")?
    {
        ServerMessage::Hello {
            protocol_version: 1,
            ..
        } => Ok(()),
        ServerMessage::Hello {
            protocol_version, ..
        } => Err(anyhow!(
            "unsupported daemon protocol version {protocol_version}"
        )),
        ServerMessage::Error { message } => Err(anyhow!(message)),
        message => Err(anyhow!("unexpected daemon hello response: {message:?}")),
    }
}

fn write_client_message(stream: &mut UnixStream, message: &ClientMessage) -> Result<()> {
    let line = crate::daemon::protocol::encode_json_line(message)
        .context("failed to encode daemon client message")?;
    stream
        .write_all(line.as_bytes())
        .context("failed to write daemon client message")?;
    stream
        .flush()
        .context("failed to flush daemon client message")
}

fn send_round_trip(stream: &mut UnixStream, message: &ClientMessage) -> Result<ServerMessage> {
    write_client_message(stream, message)?;
    let mut line = String::new();
    let mut reader = BufReader::new(
        stream
            .try_clone()
            .context("failed to clone daemon stream for response read")?,
    );
    let bytes = reader
        .read_line(&mut line)
        .context("failed to read daemon response")?;
    if bytes == 0 {
        bail!("daemon closed connection before sending response")
    }

    decode_server_message(&line).context("failed to decode daemon response")
}

fn read_subscription_snapshot(reader: &mut BufReader<UnixStream>) -> Result<WorkspaceSnapshot> {
    let mut line = String::new();
    let bytes = reader
        .read_line(&mut line)
        .context("failed to read daemon subscription snapshot")?;
    if bytes == 0 {
        bail!("daemon closed subscription before sending initial snapshot")
    }

    match decode_server_message(&line).context("failed to decode subscription snapshot")? {
        ServerMessage::Snapshot { snapshot } => Ok(snapshot),
        ServerMessage::Error { message } => Err(anyhow!(message)),
        message => Err(anyhow!(
            "unexpected daemon subscription response: {message:?}"
        )),
    }
}

fn read_subscription_updates(
    mut reader: BufReader<UnixStream>,
    sender: mpsc::Sender<Result<WorkspaceSnapshot, String>>,
) {
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => {
                let _ = sender.send(Err("daemon subscription disconnected".to_string()));
                return;
            }
            Ok(_) => match decode_server_message(&line) {
                Ok(ServerMessage::Snapshot { snapshot }) => {
                    if sender.send(Ok(snapshot)).is_err() {
                        return;
                    }
                }
                Ok(ServerMessage::Error { message }) => {
                    let _ = sender.send(Err(message));
                    return;
                }
                Ok(message) => {
                    let _ = sender.send(Err(format!(
                        "unexpected daemon subscription message: {message:?}"
                    )));
                    return;
                }
                Err(error) => {
                    let _ = sender.send(Err(format!(
                        "failed to decode daemon subscription message: {error}"
                    )));
                    return;
                }
            },
            Err(error) => {
                let _ = sender.send(Err(format!(
                    "failed to read daemon subscription message: {error}"
                )));
                return;
            }
        }
    }
}

pub fn decode_server_message(line: &str) -> serde_json::Result<ServerMessage> {
    serde_json::from_str(line)
}

fn attempt_send_agent_event(client: &mut Client, event: &AgentEvent) -> Result<()> {
    client.send(ClientMessage::AgentEvent {
        event: event.clone(),
    })
}

pub fn send_agent_event(event: &AgentEvent) -> Result<()> {
    let mut client = Client::connect()?;
    attempt_send_agent_event(&mut client, event)
}

pub fn run_daemon_start() -> Result<()> {
    if run_daemon_start_with(
        connect_socket,
        wait_for_existing_socket,
        spawn_daemon_process,
        wait_for_socket,
    )? {
        println!("daemon started");
    } else {
        println!("daemon already running");
    }

    Ok(())
}

fn run_daemon_start_with<C, E, S, W>(
    connect: C,
    wait_for_existing: E,
    spawn: S,
    wait_for_startup: W,
) -> Result<bool>
where
    C: FnOnce() -> Result<UnixStream>,
    E: FnOnce() -> Result<UnixStream>,
    S: FnOnce() -> Result<()>,
    W: FnOnce() -> Result<UnixStream>,
{
    match connect() {
        Ok(_) => Ok(false),
        Err(_) => match wait_for_existing() {
            Ok(_) => Ok(false),
            Err(_) => {
                spawn()?;
                let _ = wait_for_startup()?;
                Ok(true)
            }
        },
    }
}

pub fn run_daemon_stop() -> Result<()> {
    let mut client = Client::connect().context("daemon is not running")?;
    run_daemon_stop_with_client(&mut client)?;
    wait_for_socket_shutdown()
}

pub fn run_daemon_restart() -> Result<()> {
    run_daemon_restart_with(
        connect_and_request_shutdown,
        wait_for_socket_shutdown,
        wait_for_socket,
        spawn_daemon_process,
    )
}

fn connect_and_request_shutdown() -> Result<()> {
    let mut client = Client::connect().context("daemon is not running")?;
    run_daemon_stop_with_client(&mut client)
}

fn run_daemon_stop_with_client(client: &mut Client) -> Result<()> {
    client.send(ClientMessage::Shutdown)
}

fn run_daemon_restart_with<C, H, W, S>(
    connect_and_shutdown: C,
    wait_for_shutdown: H,
    wait_for_startup: W,
    spawn: S,
) -> Result<()>
where
    C: FnOnce() -> Result<()>,
    H: FnOnce() -> Result<()>,
    W: FnOnce() -> Result<UnixStream>,
    S: FnOnce() -> Result<()>,
{
    connect_and_shutdown()?;
    wait_for_shutdown()?;
    spawn()?;
    let _ = wait_for_startup()?;
    Ok(())
}

fn wait_for_socket_shutdown() -> Result<()> {
    let path = daemon_socket_path()?;
    for _ in 0..50 {
        match UnixStream::connect(&path) {
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                return Err(anyhow!("daemon shutdown could not be confirmed: {error}"));
            }
        }
        thread::sleep(Duration::from_millis(50));
    }

    Err(anyhow!("daemon did not shut down"))
}

#[cfg(test)]
mod tests {
    use super::{attempt_send_agent_event, decode_server_message, Client};
    use crate::daemon::agent::AgentEvent;
    use crate::daemon::protocol::{ClientMessage, ServerMessage};
    use crate::workspace::WorkspaceSnapshot;
    use anyhow::anyhow;
    use std::cell::Cell;
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn test_dir(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        let path = env::temp_dir().join(format!("nerve-center-client-{label}-{unique}"));
        fs::create_dir_all(&path).expect("test dir should be created");
        path
    }

    #[test]
    fn decodes_server_message_from_single_line() {
        let line = r#"{"type":"hello","protocol_version":1,"server_name":"nerve_centerd"}"#;
        let decoded = decode_server_message(line).unwrap();
        assert_eq!(
            decoded,
            ServerMessage::Hello {
                protocol_version: 1,
                server_name: "nerve_centerd".to_string(),
            }
        );
    }

    #[test]
    fn agent_events_use_typed_client_message_path() {
        let mut client = Client::stub();
        let event = AgentEvent {
            project_id: "/tmp/alpha".to_string(),
            runtime: "opencode".to_string(),
            pane_id: 55,
            state: "working".to_string(),
            awaiting_user: Some("question".to_string()),
        };

        attempt_send_agent_event(&mut client, &event).unwrap();

        assert_eq!(
            client.sent_messages(),
            &[ClientMessage::AgentEvent {
                event: event.clone(),
            }]
        );
    }

    #[test]
    fn reconnect_returns_current_snapshot_for_stub_transport() {
        let snapshot = WorkspaceSnapshot {
            protocol_version: 1,
            generated_at_ms: 42,
            projects: Default::default(),
            project_order: vec!["root:alpha".to_string()],
            warnings: Vec::new(),
        };
        let mut client = Client::stub_with_snapshot(snapshot.clone());

        let reconnected = client.reconnect().unwrap();

        assert_eq!(reconnected, snapshot);
    }

    #[test]
    fn connect_with_autostart_spawns_and_waits_after_connect_failure() {
        let connect_attempts = Rc::new(Cell::new(0));
        let spawn_calls = Rc::new(Cell::new(0));
        let wait_calls = Rc::new(Cell::new(0));
        let (expected_stream, _peer) = std::os::unix::net::UnixStream::pair().unwrap();

        let connect_attempts_for_connect = Rc::clone(&connect_attempts);
        let spawn_calls_for_spawn = Rc::clone(&spawn_calls);
        let wait_calls_for_wait = Rc::clone(&wait_calls);

        let stream = super::connect_with_autostart(
            move || {
                connect_attempts_for_connect.set(connect_attempts_for_connect.get() + 1);
                Err(anyhow!("missing daemon"))
            },
            move || {
                spawn_calls_for_spawn.set(spawn_calls_for_spawn.get() + 1);
                Ok(())
            },
            move || {
                wait_calls_for_wait.set(wait_calls_for_wait.get() + 1);
                Ok(expected_stream)
            },
        )
        .unwrap();

        assert_eq!(connect_attempts.get(), 1);
        assert_eq!(spawn_calls.get(), 1);
        assert_eq!(wait_calls.get(), 1);
        let _ = stream;
    }

    #[test]
    fn start_does_not_spawn_when_socket_appears_during_grace_window() {
        let connect_attempts = Rc::new(Cell::new(0));
        let wait_existing_calls = Rc::new(Cell::new(0));
        let spawn_calls = Rc::new(Cell::new(0));
        let wait_startup_calls = Rc::new(Cell::new(0));
        let (expected_stream, _peer) = std::os::unix::net::UnixStream::pair().unwrap();

        let connect_attempts_for_connect = Rc::clone(&connect_attempts);
        let wait_existing_calls_for_wait = Rc::clone(&wait_existing_calls);
        let spawn_calls_for_spawn = Rc::clone(&spawn_calls);
        let wait_startup_calls_for_wait = Rc::clone(&wait_startup_calls);

        let started = super::run_daemon_start_with(
            move || {
                connect_attempts_for_connect.set(connect_attempts_for_connect.get() + 1);
                Err(anyhow!("daemon still starting"))
            },
            move || {
                wait_existing_calls_for_wait.set(wait_existing_calls_for_wait.get() + 1);
                Ok(expected_stream)
            },
            move || {
                spawn_calls_for_spawn.set(spawn_calls_for_spawn.get() + 1);
                Ok(())
            },
            move || {
                wait_startup_calls_for_wait.set(wait_startup_calls_for_wait.get() + 1);
                Ok(std::os::unix::net::UnixStream::pair().unwrap().0)
            },
        )
        .unwrap();

        assert!(!started);
        assert_eq!(connect_attempts.get(), 1);
        assert_eq!(wait_existing_calls.get(), 1);
        assert_eq!(spawn_calls.get(), 0);
        assert_eq!(wait_startup_calls.get(), 0);
    }

    #[test]
    fn restart_errors_when_daemon_is_missing() {
        let result = super::run_daemon_restart_with(
            || Err(anyhow!("missing")),
            || Ok(()),
            || Ok(std::os::unix::net::UnixStream::pair().unwrap().0),
            || Ok(()),
        );

        assert!(result.is_err());
    }

    #[test]
    fn stop_sends_shutdown_message() {
        let mut client = Client::stub();

        super::run_daemon_stop_with_client(&mut client).unwrap();

        assert!(matches!(client.sent_messages(), [ClientMessage::Shutdown]));
    }

    #[test]
    fn wait_for_socket_shutdown_errors_on_ambiguous_connect_failure() {
        let _env_lock = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let socket_root = test_dir("ambiguous-shutdown");
        let parent = socket_root.join("not-a-directory");
        fs::write(&parent, "blocked").expect("test file should be written");

        unsafe {
            env::set_var(
                "NERVE_CENTER_DAEMON_SOCKET",
                parent.join("daemon.sock").as_os_str(),
            );
        }

        let result = super::wait_for_socket_shutdown();

        unsafe {
            env::remove_var("NERVE_CENTER_DAEMON_SOCKET");
        }

        assert!(result.is_err(), "unexpected result: {result:?}");
    }
}
