#!/usr/bin/env python3
"""Check real dashboard quit/reopen and crash/reopen with a disposable zsh.

No harness or model is called. The only persistent processes belong to this
fixture and are stopped in finally, including after a failed assertion.
"""
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import time


def main(binary, parts):
    with tempfile.TemporaryDirectory(prefix="cones-terminal-ui-") as temporary:
        root = Path(temporary).resolve()
        state = root / "state"
        state.mkdir()
        home = root / "home"
        home.mkdir()
        project = root / "project"
        project.mkdir()
        (state / "folders").write_text(str(project) + "\n")
        config = root / "jobs.yaml"
        harnesses = ["claude", "codex", "pi", "opencode", "gemini", "cursor",
                     "copilot", "amp", "droid", "kimi"]
        config.write_text("version: 4\ndefaults:\n" +
                          "".join(f"  {h}_enabled: false\n" for h in harnesses) +
                          "jobs: []\n")
        env = dict(os.environ, HOME=str(home), SHELL="/bin/zsh", TERM="xterm-256color",
                   CLAUDE_CONFIG_DIR=str(home / ".claude"), CODEX_HOME=str(home / ".codex"),
                   PI_CODING_AGENT_DIR=str(home / ".pi"), XDG_DATA_HOME=str(home / ".local/share"))
        for name in ["ZDOTDIR", "CONES_ZDOTDIR", "OPENCODE_DB", "OPENCODE_TUI_CONFIG"]:
            env.pop(name, None)
        children = []
        screens = []
        hosts = {}

        def start(controlling_tty=False):
            master, slave = pty.openpty()
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 35, 140, 0, 0))
            child = subprocess.Popen([binary, "--jobs", str(config), "--state-dir", str(state), "--debug"],
                                     cwd=project, env=env, stdin=slave, stdout=slave, stderr=slave,
                                     start_new_session=True,
                                     preexec_fn=(lambda: fcntl.ioctl(0, termios.TIOCSCTTY, 0))
                                     if controlling_tty else None)
            os.close(slave)
            children.append((child, master))
            return child, master

        def drain(fd, timeout=0.05):
            if select.select([fd], [], [], timeout)[0]:
                try:
                    data = os.read(fd, 65536)
                    screens.append(data)
                    return data
                except OSError:
                    pass
            return b""

        def records():
            found = []
            for path in (state / "terminals").glob("*.json"):
                record = json.loads(path.read_text())
                found.append(record)
                hosts[record["id"]] = record
            return found

        def until(fd, predicate, message):
            deadline = time.monotonic() + 8
            while not predicate():
                drain(fd)
                assert time.monotonic() < deadline, message + "\n" + b"".join(screens)[-3000:].decode(errors="replace")

        def wait_draw(fd):
            until(fd, lambda: b"terminal (zsh)" in b"".join(screens), "dashboard did not draw")
            for _ in range(5):
                drain(fd)

        def quit_dashboard(child, fd, crash=False):
            os.write(fd, b"\x1a")  # leave native shell without sending it a signal
            for _ in range(4):
                drain(fd)
            if crash:
                child.kill()
            else:
                os.write(fd, b"\x03\x03")
            until(fd, lambda: child.poll() is not None, "dashboard did not quit")

        try:
            if "shell" in parts:
                child, fd = start()
                wait_draw(fd)
                os.write(fd, b"\r")
                until(fd, lambda: len(records()) == 1, "shell host did not start")
                record = records()[0]
                native_pid = record["session"]["pid"]
                os.write(fd, b"print READY > ready\r")
                until(fd, lambda: (project / "ready").exists(), "shell input was not delivered")
                os.write(fd, b"print DRAFT_SURVIVED > result")  # deliberately no Enter
                for _ in range(4):
                    drain(fd)
                quit_dashboard(child, fd)
                os.kill(native_pid, 0)
                assert not (project / "result").exists(), "leaving the dashboard submitted the draft"
                screens.clear()
                child, fd = start()
                wait_draw(fd)
                os.write(fd, b"\r")
                until(fd, lambda: b"DRAFT_SURVIVED" in b"".join(screens), "reconnect lost the native editor draft")
                assert len(records()) == 1 and records()[0]["session"]["pid"] == native_pid
                os.write(fd, b"\r")
                until(fd, lambda: (project / "result").exists(), "resumed shell did not submit")
                assert (project / "result").read_text().strip() == "DRAFT_SURVIVED"
                quit_dashboard(child, fd, crash=True)
                os.kill(native_pid, 0)
                screens.clear()
                child, fd = start()
                wait_draw(fd)
                os.write(fd, b"\r")
                for _ in range(6):
                    drain(fd)
                os.write(fd, b"print AFTER_CRASH > crash-result\r")
                until(fd, lambda: (project / "crash-result").exists(), "crash/reopen lost the shell")
                assert (project / "crash-result").read_text().strip() == "AFTER_CRASH"
                os.write(fd, b"\x1a")
                for _ in range(4):
                    drain(fd)
                os.write(fd, b"\x18\x18")  # deliberate stop
                until(fd, lambda: not records(), "stop kept the host alive")
                try:
                    os.kill(native_pid, 0)
                except ProcessLookupError:
                    pass
                else:
                    raise AssertionError("stop left the native shell alive")
                print("PASS: native shell draft, quit/reopen, crash/reopen, same PID, and explicit stop")
                quit_dashboard(child, fd)
            if "hangup" in parts:
                for controlling_tty in [False, True]:
                    screens.clear()
                    child, fd = start(controlling_tty)
                    wait_draw(fd)
                    # Interrupt an incomplete input event while crossterm is reading it.
                    # Closing a controlling tty sends SIGHUP; closing a fixture tty need not.
                    os.write(fd, b"\x1b[200~unfinished paste")
                    for _ in range(4):
                        drain(fd)
                    children[-1] = (child, None)
                    os.close(fd)
                    try:
                        code = child.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        raise AssertionError(
                            f"dashboard survived terminal hangup (controlling={controlling_tty})"
                        ) from None
                    assert code in [0, 1], f"dashboard died without cleanup: {code}"
                print("PASS: terminal hangup exits during incomplete input, with and without SIGHUP")
        finally:
            for record in hosts.values():
                try:
                    with socket.socket(socket.AF_UNIX) as stream:
                        stream.settimeout(1)
                        stream.connect(record["socket"])
                        message = json.dumps({"Stop": {"id": record["id"]}}).encode()
                        stream.sendall(struct.pack(">I", len(message)) + message)
                        stream.recv(4096)
                except OSError:
                    pass
            for child, fd in children:
                if child.poll() is None:
                    child.kill()
                child.wait(timeout=5)
                if fd is not None:
                    os.close(fd)


if __name__ == "__main__":
    # Name `shell` or `hangup` to run one part, so the suite can run the two in parallel.
    main(str(Path(sys.argv[1]).resolve()), set(sys.argv[2:]) or {"shell", "hangup"})
