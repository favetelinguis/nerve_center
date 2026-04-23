use serde::{Deserialize, Serialize};

use crate::daemon::agent::AgentEvent;
use crate::workspace::WorkspaceSnapshot;

pub fn encode_json_line<T: Serialize>(message: &T) -> serde_json::Result<String> {
    let mut line = serde_json::to_string(message)?;
    line.push('\n');
    Ok(line)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello {
        client_name: String,
    },
    GetSnapshot,
    Subscribe,
    RefreshProject {
        project_id: String,
    },
    RefreshRoot {
        root_id: String,
    },
    RunOperation {
        project_id: String,
        operation: String,
        command: String,
    },
    AgentEvent {
        event: AgentEvent,
    },
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Hello {
        protocol_version: u32,
        server_name: String,
    },
    Snapshot {
        snapshot: WorkspaceSnapshot,
    },
    Event {
        event: ServerEvent,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    Updated,
    OperationFinished {
        project_id: String,
        operation: String,
    },
}

#[cfg(test)]
mod tests {
    use super::{encode_json_line, ClientMessage, ServerMessage};

    #[test]
    fn protocol_messages_round_trip_as_json_lines() {
        let hello = ClientMessage::Hello {
            client_name: "tui".to_string(),
        };
        let json = serde_json::to_string(&hello).unwrap();
        let decoded: ClientMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, hello);

        let hello_reply = ServerMessage::Hello {
            protocol_version: 1,
            server_name: "nerve_centerd".to_string(),
        };
        let json = serde_json::to_string(&hello_reply).unwrap();
        let decoded: ServerMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, hello_reply);
    }

    #[test]
    fn encodes_messages_as_newline_terminated_json_lines() {
        let hello = ClientMessage::Hello {
            client_name: "tui".to_string(),
        };

        let line = encode_json_line(&hello).unwrap();

        assert!(line.ends_with('\n'));

        let decoded: ClientMessage = serde_json::from_str(&line).unwrap();
        assert_eq!(decoded, hello);
    }

    #[test]
    fn run_operation_round_trips_with_full_command() {
        let message = ClientMessage::RunOperation {
            project_id: "root:alpha".to_string(),
            operation: "git_switch".to_string(),
            command: "git switch feature/demo".to_string(),
        };

        let json = serde_json::to_string(&message).unwrap();
        let decoded: ClientMessage = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded, message);
    }

    #[test]
    fn shutdown_message_round_trips_as_json() {
        let message = ClientMessage::Shutdown;

        let json = serde_json::to_string(&message).unwrap();
        let decoded: ClientMessage = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded, message);
    }
}
