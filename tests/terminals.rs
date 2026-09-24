//! The real detached host, using disposable processes and no model calls.
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

fn send(stream: &mut impl Write, value: &Value) {
    let bytes = serde_json::to_vec(value).unwrap();
    stream
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .unwrap();
    stream.write_all(&bytes).unwrap();
}

fn receive(stream: &mut impl Read) -> Value {
    let mut len = [0; 4];
    stream.read_exact(&mut len).unwrap();
    let len = u32::from_be_bytes(len) as usize;
    assert!(len < 8 * 1024 * 1024);
    let mut bytes = vec![0; len];
    stream.read_exact(&mut bytes).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

struct Host {
    child: Child,
    root: tempfile::TempDir,
    id: String,
    socket: PathBuf,
    record: Value,
}

impl Host {
    fn start(program: &str, args: &[&str], env: Value) -> Self {
        let root = tempfile::Builder::new()
            .prefix("cones-host-test-")
            .tempdir_in("/tmp")
            .unwrap();
        fs::create_dir(root.path().join("terminals")).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let socket = root.path().join("socket");
        let record = json!({
            "id":id, "socket":socket, "what":"fixture",
            "session":{"session_id":format!("terminal:{id}"),"harness":"terminal","cwd":root.path(),"state":"-"}
        });
        let launch = json!({
            "program":program.as_bytes(), "args":args.iter().map(|a| a.as_bytes()).collect::<Vec<_>>(),
            "env":env, "cwd":root.path(), "rows":12,"cols":80,
            "colors":{"fg":"rgb:e4e4/e4e4/e4e4","bg":"rgb:1414/1414/1414"},
            "shell":true, "record":record, "state":root.path()
        });
        let mut child = Command::new(env!("CARGO_BIN_EXE_cones"))
            .arg("__terminal-host")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        send(&mut child.stdin.take().unwrap(), &launch);
        let mut descriptor = libc::pollfd {
            fd: child.stdout.as_ref().unwrap().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut descriptor, 1, 5000) } <= 0 {
            let _ = child.kill();
            let _ = child.wait();
            panic!("host did not send its readiness reply");
        }
        let response = receive(child.stdout.as_mut().unwrap());
        assert!(response.get("Ok").is_some(), "{response}");
        let record = response["Ok"].clone();
        Self {
            child,
            root,
            id,
            socket,
            record,
        }
    }

    fn stream(&self) -> UnixStream {
        let stream = UnixStream::connect(&self.socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
    }

    fn attach(&self) -> Client {
        let mut stream = self.stream();
        send(&mut stream, &json!({"Attach":{"version":1,"id":self.id}}));
        let reply = receive(&mut stream);
        assert_eq!(
            reply["Attached"]["session"]["pid"],
            self.record["session"]["pid"]
        );
        Client {
            stream,
            parser: vt100::Parser::new(12, 80, 1000),
            last: Value::Null,
        }
    }

    fn stop(&mut self) {
        let mut stream = self.stream();
        send(&mut stream, &json!({"Stop":{"id":self.id}}));
        assert_eq!(receive(&mut stream), "Stopped");
        let pid = self.record["session"]["pid"].as_u64().unwrap() as i32;
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            -1,
            "stop acknowledgement must follow native exit"
        );
        assert!(
            !self
                .root
                .path()
                .join(format!("terminals/{}.json", self.id))
                .exists(),
            "stop acknowledgement must follow removal from discovery"
        );
        let deadline = Instant::now() + Duration::from_secs(4);
        while self.child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "host did not terminate");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !self
                .root
                .path()
                .join(format!("terminals/{}.json", self.id))
                .exists()
        );
        assert!(!self.socket.exists());
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none()
            && let Some(pid) = self.record["session"]["pid"].as_u64()
        {
            // This PID came from the disposable host we just launched.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Client {
    stream: UnixStream,
    parser: vt100::Parser,
    last: Value,
}

impl Client {
    fn until(&mut self, expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            let reply = receive(&mut self.stream);
            if let Some(screen) = reply.get("Screen") {
                if screen["reset"] == true {
                    self.parser.process(b"\x1bc");
                }
                self.parser.screen_mut().set_size(
                    screen["rows"].as_u64().unwrap() as u16,
                    screen["cols"].as_u64().unwrap() as u16,
                );
                let bytes: Vec<u8> = serde_json::from_value(screen["bytes"].clone()).unwrap();
                self.parser.process(&bytes);
                self.last = screen.clone();
            }
            if self.parser.screen().contents().contains(expected) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "missing {expected:?}: {}",
                self.parser.screen().contents()
            );
        }
    }

    fn input(&mut self, text: &str) {
        send(&mut self.stream, &json!({"Input":text.as_bytes()}));
    }
}

#[test]
fn detached_native_process_keeps_its_identity_draft_output_and_stop_control() {
    let code = r#"
import os, signal, sys
signal.signal(signal.SIGUSR1, lambda *_: print("OFFLINE_COMPLETE", flush=True))
signal.signal(signal.SIGWINCH, lambda *_: print("SIZE:%dx%d" % (os.get_terminal_size().lines, os.get_terminal_size().columns), flush=True))
print("READY:%d" % os.getpid(), flush=True)
for line in sys.stdin:
    print("RECEIVED:" + line.strip(), flush=True)
"#;
    let mut host = Host::start("/usr/bin/python3", &["-u", "-c", code], json!([]));
    let pid = host.record["session"]["pid"].as_u64().unwrap() as i32;
    let mut first = host.attach();
    first.until(&format!("READY:{pid}"));
    first.input("draft-kept");
    first.until("draft-kept");
    let mut second = host.stream();
    send(&mut second, &json!({"Attach":{"version":1,"id":host.id}}));
    assert!(
        receive(&mut second)["Error"]
            .as_str()
            .unwrap()
            .contains("another dashboard")
    );
    drop(second);
    drop(first);
    // The native child, not any client, produces output while detached.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGUSR1) }, 0);
    std::thread::sleep(Duration::from_millis(50));
    let mut returned = host.attach();
    returned.until("OFFLINE_COMPLETE");
    returned.input("\n");
    returned.until("RECEIVED:draft-kept");
    send(&mut returned.stream, &json!({"Resize":[15,93]}));
    returned.until("SIZE:15x93");
    host.stop();
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "explicit stop must reap the native process"
    );
}

#[test]
fn host_restores_stdin_and_environment_without_exporting_the_transport_marker() {
    let code = "import os,sys; print('BODY:' + sys.stdin.read(), flush=True); print('ENV:' + os.environ['CUSTOM_NATIVE_HOME'], flush=True); print('MARKER:' + str('CONES_LAUNCH_STDIN' in os.environ), flush=True)";
    let marker = "CONES_LAUNCH_STDIN".as_bytes();
    let input = "literal $HOME `text` and\nsecond line";
    let mut host = Host::start(
        "/usr/bin/python3",
        &["-u", "-c", code],
        json!([
            [marker, input.as_bytes()],
            ["CUSTOM_NATIVE_HOME".as_bytes(), b"/fixture/custom/home"]
        ]),
    );
    let mut client = host.attach();
    client.until("MARKER:False");
    assert!(
        client
            .parser
            .screen()
            .contents()
            .contains("BODY:literal $HOME `text` and")
    );
    assert!(client.parser.screen().contents().contains("second line"));
    assert!(
        client
            .parser
            .screen()
            .contents()
            .contains("ENV:/fixture/custom/home")
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while host.child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !host.socket.exists(),
        "normal native exit releases the host endpoint"
    );
}

#[test]
fn incompatible_clients_and_broken_streams_do_not_stop_work() {
    let mut host = Host::start("/bin/cat", &[], json!([]));
    let mut bad = host.stream();
    send(&mut bad, &json!({"Attach":{"version":99,"id":host.id}}));
    assert!(receive(&mut bad).get("Error").is_some());
    drop(bad);
    let mut bad = host.stream();
    bad.write_all(&u32::MAX.to_be_bytes()).unwrap();
    drop(bad);
    let mut valid = host.attach();
    valid.input("STILL_ALIVE\n");
    valid.until("STILL_ALIVE");
    host.stop();
}

/// One part of `scripts/check-terminals.py` against the real dashboard; returns its stdout.
#[cfg(target_os = "macos")]
fn real_dashboard(part: &str) -> String {
    let output = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/scripts/check-terminals.py"
        ))
        .arg(env!("CARGO_BIN_EXE_cones"))
        .arg(part)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
#[cfg(target_os = "macos")]
fn real_dashboard_quit_and_crash_preserve_the_same_shell_and_draft() {
    assert!(real_dashboard("shell").contains("same PID"));
}

#[test]
#[cfg(target_os = "macos")]
fn real_dashboard_exits_on_hangup_during_incomplete_input() {
    assert!(real_dashboard("hangup").contains("terminal hangup exits"));
}
