//! Read-only runtime state from the already-running local Codex app-server.
//! No thread resume, event subscription, approval response, or daemon launch.
use crate::{fleet::Session, observe};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io,
    os::unix::net::UnixStream,
    path::Path,
    time::{Duration, Instant},
};
use tungstenite::{Message, WebSocket, protocol::WebSocketConfig};

const DEADLINE: Duration = Duration::from_millis(500);
const MAX_MESSAGE: usize = 2 * 1024 * 1024;

pub(super) fn apply(home: &Path, rows: &mut [Session]) {
    let ids: Vec<_> = rows
        .iter()
        .filter(|r| r.harness == "codex" && r.kind.as_deref() == Some("daemon"))
        .map(|r| r.session_id.as_str())
        .collect();
    if ids.is_empty() {
        return;
    }
    let result = observe::read("native_status", || read(home, &ids), Result::is_ok);
    // Keep the current rollout-derived state when the runtime cannot answer. Never
    // cache a previous input wait across refreshes or across native homes.
    if let Ok(states) = result {
        for row in rows {
            if row.harness == "codex"
                && row.kind.as_deref() == Some("daemon")
                && let Some(state) = states.get(&row.session_id)
            {
                row.state = (*state).into();
            }
        }
    }
}

fn read(home: &Path, ids: &[&str]) -> io::Result<HashMap<String, &'static str>> {
    let deadline = Instant::now() + DEADLINE;
    let stream = UnixStream::connect(home.join("app-server-control/app-server-control.sock"))?;
    stream.set_read_timeout(Some(DEADLINE))?;
    stream.set_write_timeout(Some(DEADLINE))?;
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    let (mut socket, _) =
        tungstenite::client::client_with_config("ws://localhost/", stream, Some(config))
            .map_err(io::Error::other)?;
    send(
        &mut socket,
        &json!({"id": 0, "method": "initialize", "params": {"clientInfo": {"name": "cones_status", "version": env!("CARGO_PKG_VERSION")}}}),
    )?;
    if response(&mut socket, 0, deadline)?.is_null() {
        return Err(io::Error::other("Codex refused status initialization"));
    }
    send(&mut socket, &json!({"method": "initialized"}))?;
    let mut states = HashMap::new();
    for (index, id) in ids.iter().enumerate() {
        remaining(&socket, deadline)?;
        let request = index as u64 + 1;
        send(
            &mut socket,
            &json!({"id": request, "method": "thread/read", "params": {"threadId": id, "includeTurns": false}}),
        )?;
        let result = response(&mut socket, request, deadline)?;
        let thread = &result["thread"];
        if thread["id"].as_str() == Some(id)
            && let Some(state) = state(&thread["status"])
        {
            states.insert((*id).to_owned(), state);
        }
    }
    Ok(states)
}

fn remaining(socket: &WebSocket<UnixStream>, deadline: Instant) -> io::Result<()> {
    let left = deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "Codex status read timed out"))?;
    socket.get_ref().set_read_timeout(Some(left))?;
    socket.get_ref().set_write_timeout(Some(left))
}

fn send(socket: &mut WebSocket<UnixStream>, value: &Value) -> io::Result<()> {
    socket
        .send(Message::Text(value.to_string().into()))
        .map_err(io::Error::other)
}

fn response(socket: &mut WebSocket<UnixStream>, id: u64, deadline: Instant) -> io::Result<Value> {
    for _ in 0..128 {
        remaining(socket, deadline)?;
        let message = socket.read().map_err(io::Error::other)?;
        let Message::Text(text) = message else {
            continue;
        };
        let value: Value = serde_json::from_str(&text).map_err(io::Error::other)?;
        // A server-initiated request is never ours to answer, even if its id
        // collides with a metadata read. No subscription was requested.
        if value.get("method").is_some() || value["id"].as_u64() != Some(id) {
            continue;
        }
        if value.get("error").is_some() {
            // One missing or archived thread must not hide another thread's wait.
            return Ok(Value::Null);
        }
        return value
            .get("result")
            .cloned()
            .ok_or_else(|| io::Error::other("missing native status result"));
    }
    Err(io::Error::other(
        "too many unrelated native status messages",
    ))
}

fn state(value: &Value) -> Option<&'static str> {
    match value["type"].as_str()? {
        "active" => {
            let flags = value["activeFlags"].as_array()?;
            if !flags.iter().all(Value::is_string) {
                return None;
            }
            if flags
                .iter()
                .any(|f| matches!(f.as_str(), Some("waitingOnApproval" | "waitingOnUserInput")))
            {
                Some("blocked")
            } else if flags.is_empty() {
                Some("active")
            } else {
                None
            }
        }
        "idle" => Some("idle"),
        "systemError" => Some("failed"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::net::UnixListener, thread};

    fn row(id: &str) -> Session {
        serde_json::from_value(json!({"session_id": id, "harness": "codex", "kind": "daemon", "cwd": "/fixture", "state": "active", "title": "keep me"})).unwrap()
    }

    fn server(
        home: &Path,
        states: Vec<Value>,
        inject_request: bool,
    ) -> thread::JoinHandle<Vec<Value>> {
        let dir = home.join("app-server-control");
        fs::create_dir_all(&dir).unwrap();
        let listener = UnixListener::bind(dir.join("app-server-control.sock")).unwrap();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            let mut seen = Vec::new();
            let mut statuses = states.into_iter();
            while let Ok(Message::Text(text)) = socket.read() {
                let request: Value = serde_json::from_str(&text).unwrap();
                seen.push(request.clone());
                match request["method"].as_str() {
                    Some("initialize") => {
                        send(&mut socket, &json!({"id": request["id"], "result": {}})).unwrap()
                    }
                    Some("initialized") => {}
                    Some("thread/read") => {
                        assert_eq!(request["params"]["includeTurns"], false);
                        if inject_request {
                            send(&mut socket, &json!({"id": request["id"], "method": "item/commandExecution/requestApproval", "params": {}})).unwrap();
                        }
                        let status = statuses.next().unwrap();
                        send(&mut socket, &json!({"id": request["id"], "result": {"thread": {"id": request["params"]["threadId"], "status": status}}})).unwrap();
                    }
                    method => panic!("observer sent an unexpected method: {method:?}"),
                }
            }
            seen
        })
    }

    #[test]
    fn native_waits_reach_rows_without_receiving_or_answering_approvals() {
        let root = tempfile::Builder::new()
            .prefix("cones-status-")
            .tempdir_in("/tmp")
            .unwrap();
        let native = server(
            root.path(),
            vec![
                json!({"type":"active","activeFlags":["waitingOnApproval"]}),
                json!({"type":"active","activeFlags":["waitingOnUserInput"]}),
                json!({"type":"idle"}),
            ],
            true,
        );
        let mut rows = vec![row("approval"), row("question"), row("idle")];
        apply(root.path(), &mut rows);
        assert_eq!(
            rows.iter().map(|r| r.state.as_str()).collect::<Vec<_>>(),
            ["blocked", "blocked", "idle"]
        );
        assert!(rows.iter().all(|r| r.title.as_deref() == Some("keep me")));
        let calls = native.join().unwrap();
        assert_eq!(
            calls
                .iter()
                .filter(|r| r["method"] == "thread/read")
                .count(),
            3
        );
        assert!(
            calls.iter().all(|r| r.get("method").is_some()),
            "the observer must send no approval response"
        );
        fs::remove_file(
            root.path()
                .join("app-server-control/app-server-control.sock"),
        )
        .unwrap();
        let mut fresh_rows = vec![row("approval")];
        apply(root.path(), &mut fresh_rows);
        assert_eq!(
            fresh_rows[0].state, "active",
            "a previous wait must not survive a failed read"
        );
    }

    #[test]
    fn homes_and_concurrent_observers_are_independent() {
        let one = tempfile::Builder::new()
            .prefix("cones-status-")
            .tempdir_in("/tmp")
            .unwrap();
        let two = tempfile::Builder::new()
            .prefix("cones-status-")
            .tempdir_in("/tmp")
            .unwrap();
        let a = server(
            one.path(),
            vec![json!({"type":"active","activeFlags":["waitingOnUserInput"]})],
            false,
        );
        let b = server(
            two.path(),
            vec![json!({"type":"active","activeFlags":[]})],
            false,
        );
        thread::scope(|scope| {
            let x = scope.spawn(|| read(one.path(), &["same-id"]).unwrap());
            let y = scope.spawn(|| read(two.path(), &["same-id"]).unwrap());
            assert_eq!(x.join().unwrap()["same-id"], "blocked");
            assert_eq!(y.join().unwrap()["same-id"], "active");
        });
        a.join().unwrap();
        b.join().unwrap();
    }

    #[test]
    fn absent_or_unrecognized_runtime_state_leaves_rollout_state_available() {
        for value in [
            json!({"type":"notLoaded"}),
            json!({"type":"active"}),
            json!({"type":"active","activeFlags":["new-flag"]}),
            json!({"type":"future"}),
        ] {
            assert_eq!(state(&value), None, "{value}");
        }
        assert_eq!(state(&json!({"type":"systemError"})), Some("failed"));
        let root = tempfile::tempdir().unwrap();
        let mut external = row("external");
        external.kind = None;
        apply(root.path(), std::slice::from_mut(&mut external));
        assert_eq!(external.state, "active");
    }
}
