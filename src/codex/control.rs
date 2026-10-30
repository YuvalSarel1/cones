//! End, name, compact or fork one thread over the already-running local Codex app-server, or
//! trust a folder.
//!
//! Codex has no stop, rename or compact subcommand. Its app-server protocol interrupts a turn
//! by id, closes the terminals a thread left running, names a thread the way its own `/rename`
//! does, loaded or not, and compacts one the way its own `/compact` does; all of these keep the
//! conversation. It forks a thread the way `codex fork` does and answers with the new thread's
//! id, taking the permission settings a remote `codex fork` refuses. It also saves a folder's
//! trust the way the client's own "Trust and continue" does. Nothing here starts a daemon or
//! answers a request the daemon sends, and only a compaction loads a thread the daemon is not
//! holding, as a client joining it would.
use super::runtime::{connect, remaining, send};
use serde_json::{Value, json};
use std::{
    io,
    os::unix::net::UnixStream,
    path::Path,
    time::{Duration, Instant},
};
use tungstenite::{Message, WebSocket};

/// The whole exchange, including the wait for the daemon to report what it ended.
const DEADLINE: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(100);

/// What a stop found running and ended. Both false is a thread that was already quiet.
/// `queued` is a note waiting behind the interrupted turn that the stop then started.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stopped {
    pub turn: bool,
    pub terminals: bool,
    pub queued: bool,
}

struct Client {
    socket: WebSocket<UnixStream>,
    deadline: Instant,
    next: u64,
}

impl Client {
    fn call(&mut self, method: &str, params: Value) -> io::Result<Value> {
        let id = self.next;
        self.next += 1;
        send(
            &mut self.socket,
            &json!({"id": id, "method": method, "params": params}),
        )?;
        loop {
            remaining(&self.socket, self.deadline)?;
            let Message::Text(text) = self.socket.read().map_err(io::Error::other)? else {
                continue;
            };
            let reply: Value = serde_json::from_str(&text).map_err(io::Error::other)?;
            // Notifications and the daemon's own requests are not replies, whatever their id.
            if reply.get("method").is_some() || reply["id"].as_u64() != Some(id) {
                continue;
            }
            if let Some(error) = reply.get("error") {
                return Err(io::Error::other(format!(
                    "{method}: {}",
                    error["message"].as_str().unwrap_or("refused")
                )));
            }
            return Ok(reply["result"].clone());
        }
    }

    /// The turn `thread` is running now, if any. Turns list newest first.
    fn running(&mut self, thread: &str) -> io::Result<Option<String>> {
        let turns = self.call("thread/turns/list", json!({"threadId": thread, "limit": 1}))?;
        let turn = &turns["data"][0];
        Ok((turn["status"] == "inProgress")
            .then(|| turn["id"].as_str().map(str::to_owned))
            .flatten())
    }

    fn terminals(&mut self, thread: &str) -> io::Result<bool> {
        let list = self.call(
            "thread/backgroundTerminals/list",
            json!({"threadId": thread}),
        )?;
        Ok(list["data"].as_array().is_some_and(|open| !open.is_empty()))
    }
}

fn client(home: &Path, name: &str) -> io::Result<Client> {
    let mut client = Client {
        socket: connect(home, DEADLINE)?,
        deadline: Instant::now() + DEADLINE,
        next: 0,
    };
    // Background terminals are behind the protocol's experimental capability.
    client.call(
        "initialize",
        json!({
            "clientInfo": {"name": name, "version": env!("CARGO_PKG_VERSION")},
            "capabilities": {"experimentalApi": true},
        }),
    )?;
    send(&mut client.socket, &json!({"method": "initialized"}))?;
    Ok(client)
}

/// Name `thread` through `thread/name/set`, which writes Codex's own name records and tells
/// every client showing the thread.
pub fn rename(home: &Path, thread: &str, name: &str) -> io::Result<()> {
    client(home, "cones_rename")?
        .call("thread/name/set", json!({"threadId": thread, "name": name}))
        .map(drop)
}

/// Compact `thread` through `thread/compact/start`, which Codex's own `/compact` calls, and
/// return once the daemon lists the compaction's turn as ended, within `within`. A thread the
/// daemon has not loaded is loaded first with `thread/resume`, as a client opening it does.
pub fn compact(home: &Path, thread: &str, within: Duration) -> io::Result<()> {
    let mut client = client(home, "cones_compact")?;
    let loaded = client.call("thread/loaded/list", json!({}))?;
    if !loaded["data"]
        .as_array()
        .is_some_and(|ids| ids.iter().any(|id| id == thread))
    {
        client.call("thread/resume", json!({"threadId": thread}))?;
    }
    let newest = |client: &mut Client| -> io::Result<Value> {
        let turns = client.call("thread/turns/list", json!({"threadId": thread, "limit": 1}))?;
        Ok(turns["data"][0].clone())
    };
    let before = newest(&mut client)?["id"].clone();
    client.call("thread/compact/start", json!({"threadId": thread}))?;
    client.deadline = Instant::now() + within;
    loop {
        let turn = newest(&mut client)?;
        if turn["id"] != before && turn["status"] != "inProgress" {
            return match turn["status"].as_str() {
                Some("completed") => Ok(()),
                status => Err(io::Error::other(format!(
                    "the compaction ended {}: {}",
                    status.unwrap_or("without a status"),
                    turn["error"]["message"]
                        .as_str()
                        .unwrap_or("Codex gave no reason")
                ))),
            };
        }
        std::thread::sleep(POLL);
    }
}

/// Fork `thread` into a new thread in `cwd` through `thread/fork`, which `codex fork` calls, and
/// return the new thread's id. The daemon holds the fork from here, before any turn. `bypass`
/// asks for the fork without approvals or sandbox, which a remote client refuses to pass:
/// "Permission overrides are not supported when forking a remote task".
pub fn fork(home: &Path, thread: &str, cwd: &Path, bypass: bool) -> io::Result<String> {
    let mut params = json!({"threadId": thread, "cwd": cwd, "excludeTurns": true});
    if bypass {
        params["approvalPolicy"] = json!("never");
        params["sandbox"] = json!("danger-full-access");
    }
    let forked = client(home, "cones_fork")?.call("thread/fork", params)?;
    forked["thread"]["id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| io::Error::other("thread/fork answered with no thread id"))
}

/// Save `dir` as trusted in the daemon's own config, as the client's trust question does when
/// answered, so a client cones starts there does not stop on that question. A folder the
/// config already names, trusted or not, keeps its setting.
pub fn trust(home: &Path, dir: &Path) -> io::Result<()> {
    let mut client = client(home, "cones_trust")?;
    let dir = dir.to_string_lossy();
    let config = client.call("config/read", json!({}))?;
    if !config["config"]["projects"][dir.as_ref()].is_null() {
        return Ok(());
    }
    client
        .call(
            "config/batchWrite",
            json!({"edits": [{
                "keyPath": "projects",
                "mergeStrategy": "upsert",
                "value": {dir.as_ref(): {"trust_level": "trusted"}},
            }]}),
        )
        .map(drop)
}

/// Interrupt the turn `thread` is running and close the terminals it left open, returning
/// once the daemon reports both gone. A thread the daemon has not loaded runs nothing.
pub fn stop(home: &Path, thread: &str) -> io::Result<Stopped> {
    let mut client = client(home, "cones_stop")?;
    let loaded = client.call("thread/loaded/list", json!({}))?;
    if !loaded["data"]
        .as_array()
        .is_some_and(|ids| ids.iter().any(|id| id == thread))
    {
        return Ok(Stopped::default());
    }
    let mut stopped = Stopped::default();
    if let Some(turn) = client.running(thread)? {
        client.call(
            "turn/interrupt",
            json!({"threadId": thread, "turnId": turn}),
        )?;
        // The reply acknowledges the request. The turn has ended when the daemon lists it so;
        // the deadline `call` enforces bounds the wait.
        while client.running(thread)?.as_deref() == Some(&turn) {
            std::thread::sleep(POLL);
        }
        stopped.turn = true;
    }
    // An interrupted command keeps running as a background terminal, as does one a finished
    // turn left behind.
    if client.terminals(thread)? {
        client.call(
            "thread/backgroundTerminals/clean",
            json!({"threadId": thread}),
        )?;
        while client.terminals(thread)? {
            std::thread::sleep(POLL);
        }
        stopped.terminals = true;
    }
    // The daemon starts a queued note when a turn completes but not when one is interrupted, so
    // a note sent just before the stop would wait for the owner's next prompt and answer after
    // it. Starting the queue delivers the note right after the interrupt, as it was meant to
    // follow it. A daemon without the queue methods has nothing queued.
    if stopped.turn
        && client
            .call("thread/queue/list", json!({"threadId": thread, "limit": 1}))
            .is_ok_and(|queue| queue["data"].as_array().is_some_and(|q| !q.is_empty()))
    {
        client.call("thread/queue/start", json!({"threadId": thread}))?;
        stopped.queued = true;
    }
    Ok(stopped)
}
