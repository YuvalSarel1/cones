"""Own and reap dashboard processes used by terminal fixtures."""
from contextlib import contextmanager
import os
import signal
import subprocess


def _interrupted(signum, _frame):
    raise SystemExit(128 + signum)


@contextmanager
def child_process(command, **kwargs):
    """Kill and wait for the child on success, exceptions, SIGTERM and SIGHUP.

    Use from the main thread. A separate process group also covers preparation
    commands. Closing tmux alone is insufficient for a dashboard that ignores HUP.
    """
    signals = (signal.SIGTERM, signal.SIGHUP)
    previous = {sig: signal.getsignal(sig) for sig in signals}
    child = None
    try:
        for sig in signals:
            signal.signal(sig, _interrupted)
        child = subprocess.Popen(command, start_new_session=True, **kwargs)
        yield child
    finally:
        # A second termination request must not interrupt reaping.
        for sig in signals:
            signal.signal(sig, signal.SIG_IGN)
        try:
            if child is not None:
                if child.poll() is None:
                    try:
                        os.killpg(child.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                child.wait(timeout=5)
        finally:
            for sig, handler in previous.items():
                signal.signal(sig, handler)
