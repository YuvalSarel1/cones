import os
import signal
import subprocess
import sys
import unittest

from tui_fixture import child_process


SLEEPER = [
    sys.executable,
    "-c",
    "import signal,time; "
    "signal.signal(signal.SIGTERM, signal.SIG_IGN); "
    "signal.signal(signal.SIGHUP, signal.SIG_IGN); "
    "print('ready', flush=True); time.sleep(60)",
]


class ChildProcessTest(unittest.TestCase):
    def assert_reaped(self, child, status=-signal.SIGKILL):
        self.assertEqual(child.returncode, status)
        with self.assertRaises(ChildProcessError):
            os.waitpid(child.pid, os.WNOHANG)
        child.stdout.close()

    def test_normal_exit_kills_and_reaps_a_child_that_ignores_term(self):
        with child_process(SLEEPER, stdout=subprocess.PIPE) as child:
            self.assertEqual(child.stdout.readline(), b"ready\n")
        self.assert_reaped(child)

    def test_an_assertion_failure_still_kills_and_reaps(self):
        with self.assertRaisesRegex(AssertionError, "fixture failure"):
            with child_process(SLEEPER, stdout=subprocess.PIPE) as child:
                self.assertEqual(child.stdout.readline(), b"ready\n")
                raise AssertionError("fixture failure")
        self.assert_reaped(child)

    def test_a_finished_child_keeps_its_exit_status(self):
        with child_process(
            [sys.executable, "-c", "raise SystemExit(7)"], stdout=subprocess.PIPE
        ) as child:
            self.assertEqual(child.wait(timeout=5), 7)
        self.assert_reaped(child, 7)

    def test_termination_signals_reap_before_restoring_the_handler(self):
        for sig in (signal.SIGTERM, signal.SIGHUP):
            with self.subTest(signal=sig):
                previous = signal.getsignal(sig)
                with self.assertRaises(SystemExit) as stopped:
                    with child_process(SLEEPER, stdout=subprocess.PIPE) as child:
                        self.assertEqual(child.stdout.readline(), b"ready\n")
                        os.kill(os.getpid(), sig)
                self.assertEqual(stopped.exception.code, 128 + sig)
                self.assert_reaped(child)
                self.assertEqual(signal.getsignal(sig), previous)


if __name__ == "__main__":
    unittest.main()
