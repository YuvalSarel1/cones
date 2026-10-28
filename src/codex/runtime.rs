//! Read-only runtime state from the already-running local Codex app-server.
//! No thread resume, event subscription, approval response, or daemon launch.
use crate::{
    fleet::{Session, Usage},
    observe,
};
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

/// Applies each daemon thread's native state, and returns the usage Codex reports for the
/// commands a thread runs, for the threads where it reports any.
pub(super) fn apply(home: &Path, rows: &mut [Session]) -> HashMap<String, Usage> {
    let ids: Vec<_> = rows
        .iter()
        .filter(|r| r.harness == "codex" && r.kind.as_deref() == Some("daemon"))
        .map(|r| r.session_id.as_str())
        .collect();
    if ids.is_empty() {
        return HashMap::new();
    }
    let result = observe::read("native_status", || read(home, &ids), Result::is_ok);
    // Keep the current rollout-derived state when the runtime cannot answer. Never
    // cache a previous input wait across refreshes or across native homes.
    let Ok((states, usage)) = result else {
        return HashMap::new();
    };
    for row in rows {
        if row.harness == "codex"
            && row.kind.as_deref() == Some("daemon")
            && let Some(state) = states.get(&row.session_id)
        {
            row.state = (*state).into();
        }
    }
    usage
}

/// A websocket on the daemon's control socket, with nothing sent yet. It starts no daemon.
pub(super) fn connect(home: &Path, timeout: Duration) -> io::Result<WebSocket<UnixStream>> {
    let stream = UnixStream::connect(home.join("app-server-control/app-server-control.sock"))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    tungstenite::client::client_with_config("ws://localhost/", stream, Some(config))
        .map(|(socket, _)| socket)
        .map_err(io::Error::other)
}

type Native = (HashMap<String, &'static str>, HashMap<String, Usage>);

fn read(home: &Path, ids: &[&str]) -> io::Result<Native> {
    let deadline = Instant::now() + DEADLINE;
    let mut socket = connect(home, DEADLINE)?;
    send(
        &mut socket,
        &json!({"id": 0, "method": "initialize", "params": {"clientInfo": {"name": "cones_status", "version": env!("CARGO_PKG_VERSION")}}}),
    )?;
    if response(&mut socket, 0, deadline)?.is_null() {
        return Err(io::Error::other("Codex refused status initialization"));
    }
    send(&mut socket, &json!({"method": "initialized"}))?;
    let mut states = HashMap::new();
    let mut usage = HashMap::new();
    for (index, id) in ids.iter().enumerate() {
        remaining(&socket, deadline)?;
        let request = 2 * index as u64 + 1;
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
        // Codex lists a thread's running commands with their processor share and resident
        // size. 0.165 sends both as null, and older daemons lack the method; either reports
        // nothing.
        remaining(&socket, deadline)?;
        send(
            &mut socket,
            &json!({"id": request + 1, "method": "thread/backgroundTerminals/list", "params": {"threadId": id}}),
        )?;
        let terminals = response(&mut socket, request + 1, deadline)?;
        if let Some(u) = terminals_usage(&terminals["data"]) {
            usage.insert((*id).to_owned(), u);
        }
    }
    Ok((states, usage))
}

fn terminals_usage(data: &Value) -> Option<Usage> {
    let terminals = data.as_array()?;
    let reported: Vec<_> = terminals
        .iter()
        .filter_map(|t| Some((t["cpuPercent"].as_f64()?, t["rssKb"].as_u64()?)))
        .collect();
    // A share for some commands and not others would understate the thread.
    (!reported.is_empty() && reported.len() == terminals.len()).then(|| Usage {
        cpu: reported.iter().map(|(cpu, _)| *cpu as f32).sum(),
        rss: reported.iter().map(|(_, kb)| kb * 1024).sum(),
        children: reported.len() as u32,
    })
}

pub(super) fn remaining(socket: &WebSocket<UnixStream>, deadline: Instant) -> io::Result<()> {
    let left = deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "Codex status read timed out"))?;
    socket.get_ref().set_read_timeout(Some(left))?;
    socket.get_ref().set_write_timeout(Some(left))
}

pub(super) fn send(socket: &mut WebSocket<UnixStream>, value: &Value) -> io::Result<()> {
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
        let terminals = states.iter().map(|_| json!({"data": []})).collect();
        server_with(home, states, terminals, inject_request)
    }

    /// `terminals` answers each `thread/backgroundTerminals/list` in turn: a result, or an
    /// error where it holds `{"error": ...}`.
    fn server_with(
        home: &Path,
        states: Vec<Value>,
        terminals: Vec<Value>,
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
            let mut terminals = terminals.into_iter();
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
                    Some("thread/backgroundTerminals/list") => {
                        let answer = terminals.next().unwrap();
                        let reply = match answer.get("error") {
                            Some(error) => json!({"id": request["id"], "error": error}),
                            None => json!({"id": request["id"], "result": answer}),
                        };
                        send(&mut socket, &reply).unwrap()
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
    fn a_thread_reports_its_commands_usage_only_when_codex_reports_every_command() {
        let root = tempfile::Builder::new()
            .prefix("cones-usage-")
            .tempdir_in("/tmp")
            .unwrap();
        let terminal = |cpu: Value, rss: Value| {
            json!({"itemId": "c1", "processId": "1", "command": "make", "cwd": "/fixture",
                   "osPid": null, "cpuPercent": cpu, "rssKb": rss})
        };
        let native = server_with(
            root.path(),
            vec![json!({"type":"idle"}); 4],
            vec![
                json!({"data": [terminal(json!(30.5), json!(1000)), terminal(json!(20), json!(2000))]}),
                json!({"data": [terminal(Value::Null, Value::Null)]}),
                json!({"data": [terminal(json!(10), json!(1000)), terminal(Value::Null, Value::Null)]}),
                json!({"error": {"code": -32601, "message": "unknown method"}}),
            ],
            false,
        );
        let mut rows = vec![row("reported"), row("null"), row("partial"), row("older")];
        let usage = apply(root.path(), &mut rows);
        native.join().unwrap();
        assert_eq!(
            usage,
            HashMap::from([(
                "reported".to_owned(),
                Usage {
                    cpu: 50.5,
                    rss: 3000 * 1024,
                    children: 2
                }
            )]),
            "Codex 0.165's nulls, a partial report and a daemon without the method report nothing"
        );
        assert!(
            rows.iter().all(|r| r.state == "idle"),
            "a missing terminal list keeps the thread's state"
        );
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
