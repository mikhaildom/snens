#!/usr/bin/env python3
"""Isolated Linux subprocess tests for legacy-writer-guard.py; never touches production."""
import fcntl
import importlib.util
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time

GUARD = Path(__file__).with_name("legacy-writer-guard.py")


def wait_marker(proc):
    line = proc.stdout.readline()
    if not line:
        raise AssertionError(f"guard exited before marker: {proc.stderr.read()}")
    assert '"event": "lock-acquired"' in line, line


def main():
    assert sys.platform.startswith("linux"), "requires Linux /proc and flock"
    with tempfile.TemporaryDirectory(prefix="legacy-writer-guard-test-") as tmp:
        root = Path(tmp)
        log = root / "session.jsonl"
        initial = b'{"old":"record"}\n'
        log.write_bytes(initial)
        legacy = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
        guard = None
        try:
            guard = subprocess.Popen(
                [sys.executable, str(GUARD), "--session-file", str(log), "--pid", str(legacy.pid)],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, bufsize=1,
            )
            wait_marker(guard)

            # Legacy writer ignores flock, and must remain able to append.
            with log.open("ab", buffering=0) as oldwriter:
                oldwriter.write(b'{"old":"append"}\n')
            assert log.read_bytes() == initial + b'{"old":"append"}\n'

            # A distinct flock contender (same BSD flock contract used by fs2) must fail.
            fd = os.open(log, os.O_RDONLY)
            try:
                try:
                    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                except BlockingIOError:
                    pass
                else:
                    raise AssertionError("contender acquired guard-held flock")
            finally:
                os.close(fd)

            # Guard drops lock promptly after the recorded process exits.
            legacy.terminate()
            legacy.wait(timeout=3)
            guard.wait(timeout=3)
            assert guard.returncode == 0, guard.stderr.read()
            fd = os.open(log, os.O_RDONLY)
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                fcntl.flock(fd, fcntl.LOCK_UN)
            finally:
                os.close(fd)

            # Missing files fail without creating them; nonexistent PID fast-fails.
            missing = root / "must-not-appear.jsonl"
            result = subprocess.run(
                [sys.executable, str(GUARD), "--session-file", str(missing), "--pid", str(os.getpid())],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, timeout=3,
            )
            assert result.returncode == 2 and not missing.exists()
            result = subprocess.run(
                [sys.executable, str(GUARD), "--session-file", str(log), "--pid", "2147483647"],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, timeout=3,
            )
            assert result.returncode == 2

            # Explicit identity mismatch / missing / zombie are all considered exited.
            spec = importlib.util.spec_from_file_location("legacy_guard", GUARD)
            mod = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(mod)
            assert mod.proc_identity(2147483647) is None
            fake = root / "proc" / "123"
            fake.mkdir(parents=True)
            # stat: pid (comm) state + fields 4..21 + starttime(field 22)
            (fake / "stat").write_text("123 (with ) tricky name) Z " + " ".join(["0"] * 18 + ["987"]) + "\n")
            assert mod.proc_identity(123, root / "proc") == ("987", False)
            assert not mod.identity_is_alive(123, "987", root / "proc")
            assert not mod.identity_is_alive(123, "986", root / "proc")  # PID reused
        finally:
            if legacy.poll() is None:
                legacy.kill()
                legacy.wait()
            if guard is not None and guard.poll() is None:
                guard.kill()
                guard.wait()
    print("PASS: append-under-lock, contender exclusion, release-on-exit, fail-fast, no-create, proc identity")


if __name__ == "__main__":
    main()
