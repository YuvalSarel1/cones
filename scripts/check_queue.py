#!/usr/bin/env python3
"""One shared check slot across checkouts, using the kernel lock available on macOS."""
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


def main():
    script = Path(sys.argv[1]).resolve()
    root = Path(os.environ.get("CONES_CHECK_STATE_DIR", Path.home() / ".cones"))
    root.mkdir(mode=0o700, parents=True, exist_ok=True)
    requested = time.monotonic()
    cancelled = 0

    def cancel(signum, _frame):
        nonlocal cancelled
        cancelled = signum

    for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(signum, cancel)

    # Never unlink this inode: waiters must all lock the same file. The child
    # inherits the lease so SIGKILL of this supervisor cannot admit another gate
    # while Cargo (or one of its descendants) is still running.
    fd = os.open(root / "check.lock", os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, "r+") as lease:
        waiting_for = None
        while not cancelled:
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                lease.seek(0)
                try:
                    holder = json.load(lease)
                    description = f"PID {holder['pid']} in {holder['checkout']}"
                except (ValueError, KeyError):
                    description = "another check"
                if description != waiting_for:
                    print(f"check: waiting for {description}", flush=True)
                    waiting_for = description
                time.sleep(0.1)
        if cancelled:
            return 128 + cancelled

        lease.seek(0)
        lease.truncate()
        json.dump({"pid": os.getpid(), "checkout": str(script.parent.parent)}, lease)
        lease.flush()
        if waiting_for is not None:
            print(f"check: acquired slot (PID {os.getpid()})", flush=True)
        env = os.environ.copy()
        env["CONES_CHECK_PARENT"] = str(os.getpid())
        env.setdefault("CARGO_BUILD_JOBS", "2")
        env.setdefault("RUST_TEST_THREADS", "2")
        log_dir = Path(tempfile.mkdtemp(prefix="cones-check.", dir=env.get("TMPDIR")))
        env["CONES_CHECK_LOG_DIR"] = str(log_dir)
        admitted = time.monotonic()
        child = subprocess.Popen(
            ["/bin/bash", str(script), *sys.argv[2:]],
            env=env,
            start_new_session=True,
            pass_fds=(fd,),
        )
        # A soak is asked for explicitly and runs as long as it was asked to, with the
        # five-minute cap left in place for the ordinary stress gate around it.
        stress_cap = 300 + 2 * float(env.get("CONES_STRESS_SECONDS", 0))
        deadline = (
            time.monotonic() + stress_cap if sys.argv[2] == "stress" else float("inf")
        )
        try:
            while child.poll() is None and not cancelled:
                if time.monotonic() >= deadline:
                    print(
                        f"check: stress exceeded {stress_cap:.0f}s; stopping its process group",
                        flush=True,
                    )
                    cancelled = signal.SIGTERM
                    break
                time.sleep(0.05)
            if cancelled:
                # Interrupt the entire owned gate, not just its shell. Hold the
                # lease through teardown, and escalate for children ignoring TERM.
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
            metrics = {
                "queue_wait_s": admitted - requested,
                "elapsed_s": time.monotonic() - admitted,
                "child_cpu_s": usage.ru_utime + usage.ru_stime,
                "max_child_rss_bytes": usage.ru_maxrss * (1 if sys.platform == "darwin" else 1024),
                "build_jobs": env["CARGO_BUILD_JOBS"],
                "test_threads": env["RUST_TEST_THREADS"],
                "child_exit": child.returncode,
                "scope": "waited child resource usage; maximum child RSS is not total tree RSS",
            }
            (log_dir / "usage.json").write_text(json.dumps(metrics, indent=2) + "\n")


if __name__ == "__main__":
    try:
        sys.exit(main())
    except OSError as error:
        print(f"check: {error}", file=sys.stderr)
        sys.exit(1)
