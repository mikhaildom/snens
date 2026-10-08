#!/usr/bin/env python3
"""Temporary rollout guard: hold Linux flock locks on legacy session JSONL files.

The old writer does not honor flock; this only prevents new fs2/flock-aware
writers from opening these session IDs until their recorded legacy PIDs exit.
"""
import argparse
import fcntl
import json
import os
from pathlib import Path
import sys
import time


def proc_identity(pid, proc_root=Path("/proc")):
    """Return (starttime, alive), or None if absent/unparseable."""
    try:
        raw = (Path(proc_root) / str(pid) / "stat").read_text()
        # comm is parenthesized and may itself contain spaces or ')' characters.
        fields = raw[raw.rfind(")") + 2 :].split()
        state = fields[0]  # stat field 3
        starttime = fields[19]  # stat field 22
        return starttime, state not in ("Z", "X", "x")
    except (OSError, IndexError, ValueError):
        return None


def identity_is_alive(pid, starttime, proc_root=Path("/proc")):
    identity = proc_identity(pid, proc_root)
    return identity is not None and identity[1] and identity[0] == starttime


def acquire(files):
    held = []
    try:
        for path in files:
            # Opening existing file only; never create, truncate, or write it.
            fd = os.open(path, os.O_RDONLY | getattr(os, "O_CLOEXEC", 0))
            held.append(fd)
            if not os.path.isfile(path):
                raise RuntimeError(f"not a regular session file: {path}")
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BaseException:
        for fd in held:
            os.close(fd)
        raise
    return held


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--session-file", action="append", required=True, metavar="PATH")
    parser.add_argument("--pid", action="append", required=True, type=int)
    args = parser.parse_args()
    if len(args.session_file) != len(args.pid):
        parser.error("supply one --pid for each --session-file, in matching order")

    pairs = list(zip(args.session_file, args.pid))
    identities = []
    for path, pid in pairs:
        identity = proc_identity(pid)
        if identity is None or not identity[1]:
            print(f"legacy PID {pid} is absent, exited, or /proc stat is unreadable", file=sys.stderr)
            return 2
        identities.append((pid, identity[0]))

    held = []
    try:
        held = acquire([path for path, _ in pairs])
        # Recheck after lock acquisition; don't announce a stale/dead PID as guarded.
        def still_same_process(pid, start):
            return identity_is_alive(pid, start)

        if any(not still_same_process(pid, start) for pid, start in identities):
            print("legacy PID exited or changed identity during lock acquisition", file=sys.stderr)
            return 2
        print(json.dumps({"event": "lock-acquired", "sessions": [p for p, _ in pairs]}), flush=True)
        while any(still_same_process(pid, start) for pid, start in identities):
            time.sleep(0.1)
        # If a PID vanished or its starttime changed, exit and let kernel release flock.
        return 0
    except (OSError, RuntimeError) as exc:
        print(f"legacy-writer-guard: {exc}", file=sys.stderr)
        return 2
    finally:
        for fd in held:
            os.close(fd)


if __name__ == "__main__":
    raise SystemExit(main())
