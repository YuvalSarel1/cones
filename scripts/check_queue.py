#!/usr/bin/env python3
"""Shared check slots across checkouts, using the kernel lock available on macOS.

Compiling and running tests take different leases. Two builds may run at once,
at background priority; a test run is exclusive, because wall-clock tests flake
under load. `check.lock` is the lock every older wrapper takes exclusively for its
whole gate, so builds hold it shared and test runs hold it exclusively.
"""
import fcntl
import json
import os
from pathlib import Path
import resource
import signal
import subprocess
import sys
import tempfile
import time

BUILD_SLOTS = 2
GATE_SLOTS = 2
TASKPOLICY = "/usr/sbin/taskpolicy"


def signal_group(pid, signum):
    try:
        os.killpg(pid, signum)
    except ProcessLookupError:
        pass
    except PermissionError:
        # Darwin can return EPERM for a group with no remaining live members.
        groups = subprocess.check_output(
            ["/bin/ps", "-axo", "pgid=,stat="], text=True
        )
        if any(
            len(fields) == 2 and fields[0] == str(pid) and not fields[1].startswith("Z")
            for fields in (line.split() for line in groups.splitlines())
        ):
            raise


def state_dir():
    root = Path(os.environ.get("CONES_CHECK_STATE_DIR", Path.home() / ".cones"))
    root.mkdir(mode=0o700, parents=True, exist_ok=True)
    return root


def open_lock(path):
    # Never unlink these inodes: waiters must all lock the same file.
    return os.open(path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)


def holder(paths):
    """The live PID and checkout recorded in the first of these lock files."""
    for path in paths:
        try:
            recorded = json.loads(path.read_text())
            if recorded["pid"] == os.getpid():
                continue
            os.kill(recorded["pid"], 0)
            return f"PID {recorded['pid']} in {recorded['checkout']}"
        except (OSError, ValueError, KeyError, TypeError):
            continue
    return "another check"


def record(fd):
    os.lseek(fd, 0, os.SEEK_SET)
    os.ftruncate(fd, 0)
    os.write(fd, json.dumps({"pid": os.getpid(), "checkout": os.getcwd()}).encode())


def console(message):
    # The stage's own output goes to its log; the wrapper names the descriptor
    # it keeps for its terminal.
    fd = os.environ.get("CONES_CHECK_CONSOLE_FD")
    if fd:
        os.write(int(fd), f"check: {message}\n".encode())
    else:
        print(f"check: {message}", flush=True)


class Waiter:
    """Prints who a lease is waiting for, once per holder."""

    def __init__(self):
        self.waiting_for = None

    def wait(self, description):
        if description != self.waiting_for:
            console(f"waiting for {description}")
            self.waiting_for = description
        time.sleep(0.1)


def take(fd, mode, waiter, paths):
    while True:
        try:
            fcntl.flock(fd, mode | fcntl.LOCK_NB)
            return
        except BlockingIOError:
            waiter.wait(holder(paths))


def test_threads():
    # The test slot is exclusive and runs at normal priority, on the performance
    # cores. Measured on 8 of them: 2 threads 50s+56s, 6 threads 21s+32s, and 8
    # or 12 no faster, with a wall-clock failure at 12.
    try:
        count = int(subprocess.check_output(
            ["/usr/sbin/sysctl", "-n", "hw.perflevel0.logicalcpu"], text=True
        ))
    except (OSError, subprocess.CalledProcessError, ValueError):
        count = os.cpu_count() or 2
    return str(min(6, max(2, count)))


def build_jobs():
    # Background QoS keeps a build on the efficiency cores, so more jobs than
    # there are of those only queue inside rustc.
    try:
        count = subprocess.check_output(
            ["/usr/sbin/sysctl", "-n", "hw.perflevel1.logicalcpu"], text=True
        )
        return str(max(2, int(count)))
    except (OSError, subprocess.CalledProcessError, ValueError):
        return "2"


def lease(kind, stage, command):
    """Take a build or test lease, then exec the command holding it."""
    root = state_dir()
    requested = time.monotonic()
    waiter = Waiter()
    lock_path = root / "check.lock"
    slot_paths = [root / f"build.{n}.lock" for n in range(BUILD_SLOTS)]
    turnstile = open_lock(root / "turnstile.lock")
    lock = open_lock(lock_path)
    held = [lock]
    env = os.environ.copy()
    if kind == "build":
        slots = [open_lock(path) for path in slot_paths]
        slot = None
        while slot is None:
            for fd in slots:
                try:
                    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    slot = fd
                    break
                except BlockingIOError:
                    pass
            else:
                waiter.wait(holder(slot_paths))
        record(slot)
        held.append(slot)
        # A waiting test run holds the turnstile, so builds cannot starve it.
        take(turnstile, fcntl.LOCK_EX, waiter, [lock_path, *slot_paths])
        take(lock, fcntl.LOCK_SH, waiter, [lock_path])
        fcntl.flock(turnstile, fcntl.LOCK_UN)
        env.setdefault("CARGO_BUILD_JOBS", build_jobs())
        if os.path.exists(TASKPOLICY):
            command = [TASKPOLICY, "-b", *command]
    else:
        take(turnstile, fcntl.LOCK_EX, waiter, [lock_path, *slot_paths])
        take(lock, fcntl.LOCK_EX, waiter, [lock_path, *slot_paths])
        fcntl.flock(turnstile, fcntl.LOCK_UN)
        record(lock)
    waited = time.monotonic() - requested
    if waiter.waiting_for is not None:
        console(f"acquired {kind} slot (PID {os.getpid()})")
    log_dir = os.environ.get("CONES_CHECK_LOG_DIR")
    if log_dir:
        with open(Path(log_dir) / "leases.jsonl", "a") as leases:
            leases.write(json.dumps({"stage": stage, "kind": kind, "wait_s": waited,
                                     "build_jobs": env.get("CARGO_BUILD_JOBS")}) + "\n")
    # The command inherits the lease, so killing a supervisor cannot admit
    # another check while Cargo or one of its descendants still runs.
    for fd in held:
        os.set_inheritable(fd, True)
    if "CONES_CHECK_CONSOLE_FD" in env:
        os.set_inheritable(int(env.pop("CONES_CHECK_CONSOLE_FD")), False)
    os.execvpe(command[0], command, env)


def leases(log_dir):
    try:
        lines = (log_dir / "leases.jsonl").read_text().splitlines()
    except FileNotFoundError:
        return []
    return [json.loads(line) for line in lines]


def gate_target(checkout, mode, env):
    """A warm target dir for a worktree that has none of its own, held for the gate.

    A fresh worktree would otherwise compile every dependency. Registry
    dependencies are immutable, so reusing them is safe. The workspace's own path
    packages are judged fresh by mtime, so another checkout's artifacts for them
    could pass for this one's; they are cleaned when the slot changes checkout.
    Returns the lock descriptor, which the gate and everything it runs inherit, and
    the time spent waiting for it.
    """
    if (mode not in ("all", "clippy", "test", "stress") or "CARGO_TARGET_DIR" in env
            or not (checkout / ".git").is_file() or (checkout / "target").exists()):
        return None, 0.0
    root = state_dir()
    paths = [root / f"gate.{n}.lock" for n in range(GATE_SLOTS)]
    fds = [open_lock(path) for path in paths]

    def last_checkout(path):
        try:
            return json.loads(path.read_text())["checkout"]
        except (OSError, ValueError, KeyError, TypeError):
            return None

    # The slot this checkout used last keeps its incremental build; an unused one
    # evicts nobody.
    order = sorted(range(GATE_SLOTS), key=lambda n: {str(checkout): 0, None: 1}.get(
        last_checkout(paths[n]), 2))
    requested = time.monotonic()
    waiter = Waiter()
    while True:
        for n in order:
            try:
                fcntl.flock(fds[n], fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                continue
            for other in fds:
                if other != fds[n]:
                    os.close(other)
            target = root / f"gate-target.{n}"
            # The path packages, named: a new path dependency joins this list.
            if last_checkout(paths[n]) != str(checkout) and target.exists():
                subprocess.run(["cargo", "clean", "--quiet", "-p", "cones", "-p", "vt100",
                                "--target-dir", str(target)], cwd=checkout, env=env, check=True)
            os.lseek(fds[n], 0, os.SEEK_SET)
            os.ftruncate(fds[n], 0)
            os.write(fds[n], json.dumps({"pid": os.getpid(), "checkout": str(checkout)}).encode())
            env["CARGO_TARGET_DIR"] = str(target)
            console(f"building in {target}")
            return fds[n], time.monotonic() - requested
        waiter.wait(holder(paths))


def main():
    if sys.argv[1] == "lease":
        return lease(sys.argv[2], sys.argv[3], sys.argv[4:])
    script = Path(sys.argv[1]).resolve()
    state_dir()
    started = time.monotonic()
    cancelled = 0

    def cancel(signum, _frame):
        nonlocal cancelled
        cancelled = signum

    env = os.environ.copy()
    env["CONES_CHECK_PARENT"] = str(os.getpid())
    env.setdefault("RUST_TEST_THREADS", test_threads())
    log_dir = Path(tempfile.mkdtemp(prefix="cones-check.", dir=env.get("TMPDIR")))
    env["CONES_CHECK_LOG_DIR"] = str(log_dir)
    slot, waited = gate_target(script.parent.parent, sys.argv[2], env)
    if slot is not None:
        with open(log_dir / "leases.jsonl", "a") as leases_file:
            leases_file.write(json.dumps({"stage": "gate target", "kind": "gate",
                                          "wait_s": waited, "build_jobs": None}) + "\n")
    for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(signum, cancel)
    child = subprocess.Popen(
        ["/bin/bash", str(script), *sys.argv[2:]],
        env=env,
        start_new_session=True,
        pass_fds=() if slot is None else (slot,),
    )
    # A soak is asked for explicitly and runs as long as it was asked to, with the
    # five-minute cap left in place for the ordinary stress gate around it. Time
    # spent waiting for a lease does not count against it.
    stress_cap = 300 + 2 * float(env.get("CONES_STRESS_SECONDS", 0))
    stress = sys.argv[2] == "stress"
    try:
        while child.poll() is None and not cancelled:
            if stress and time.monotonic() - started >= stress_cap + sum(
                entry["wait_s"] for entry in leases(log_dir)
            ):
                print(
                    f"check: stress exceeded {stress_cap:.0f}s; stopping its process group",
                    flush=True,
                )
                cancelled = signal.SIGTERM
                break
            time.sleep(0.05)
        if cancelled:
            # Interrupt the entire owned gate, not just its shell, and escalate
            # for children ignoring TERM.
            for sig in (signal.SIGTERM, signal.SIGKILL):
                signal_group(child.pid, sig)
                if sig == signal.SIGTERM:
                    time.sleep(0.2)
            child.wait()
            return 128 + cancelled
        return child.returncode if child.returncode >= 0 else 128 - child.returncode
    finally:
        if child.poll() is None:
            signal_group(child.pid, signal.SIGKILL)
            child.wait()
        usage = resource.getrusage(resource.RUSAGE_CHILDREN)
        stages = leases(log_dir)
        queue_wait = sum(entry["wait_s"] for entry in stages)
        metrics = {
            "queue_wait_s": queue_wait,
            "elapsed_s": time.monotonic() - started - queue_wait,
            "child_cpu_s": usage.ru_utime + usage.ru_stime,
            "max_child_rss_bytes": usage.ru_maxrss * (1 if sys.platform == "darwin" else 1024),
            "build_jobs": next(
                (entry["build_jobs"] for entry in stages if entry["kind"] == "build"),
                env.get("CARGO_BUILD_JOBS"),
            ),
            "test_threads": env["RUST_TEST_THREADS"],
            "child_exit": child.returncode,
            "leases": stages,
            "scope": "waited child resource usage; maximum child RSS is not total tree RSS",
        }
        (log_dir / "usage.json").write_text(json.dumps(metrics, indent=2) + "\n")


if __name__ == "__main__":
    try:
        sys.exit(main())
    except OSError as error:
        print(f"check: {error}", file=sys.stderr)
        sys.exit(1)
