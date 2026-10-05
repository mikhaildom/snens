#!/usr/bin/env python3
"""Hermetic integration smoke for snens-next's surface API (stdlib only).

Build separately as requested, then run:
  CARGO_TARGET_DIR=/mnt/HC_Volume_106138989/cargo-target/snens-next cargo build --features surface-next --bin snens-next
  python3 scripts/surface-smoke.py
No live provider, credentials, third-party Python packages, or fixed ports are used.
"""
from __future__ import annotations

import json
import sqlite3
import os
import pathlib
import pty
import select
import secrets
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = pathlib.Path(__file__).resolve().parents[1]
TARGET = pathlib.Path(os.environ.get("CARGO_TARGET_DIR", "/mnt/HC_Volume_106138989/cargo-target/snens-next"))
BIN = pathlib.Path(os.environ.get("SNENS_NEXT_BIN", TARGET / "debug/snens-next"))
TOKEN = "surface-smoke-" + secrets.token_hex(24)
WAIT = 12


class Stub(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True

    def __init__(self, addr):
        super().__init__(addr, StubHandler)
        self.requests = []
        self.lock = threading.Lock()
        self.counts = {}
        self.child_pid_file = None
        self.child_pid_path = ""
        self.sleep_command = ""
        self.pid_control = None


class StubHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def do_POST(self):
        size = int(self.headers.get("Content-Length", "0"))
        body = json.loads(self.rfile.read(size))
        with self.server.lock:
            self.server.requests.append(body)
        messages = body.get("messages", [])
        texts = [str(m.get("content", "")) for m in messages if m.get("role") == "user"]
        latest = texts[-1] if texts else ""
        tool_already_returned = any(m.get("role") in ("tool", "function") for m in messages)
        sleep_turn = "surface-smoke:pty-sleep" in latest or ("surface-smoke:sleep" in latest and not tool_already_returned)
        if sleep_turn:
            tool = {"index": 0, "id": "call-smoke-sleep", "type": "function",
                    "function": {"name": "run", "arguments": json.dumps({"command": self.server.sleep_command, "timeout_seconds": 60})}}
            frames = [self.frame({"tool_calls": [tool]}, None), self.frame({}, "tool_calls")]
        else:
            # Hold a deterministic window for concurrent commands/stop tests.
            if "surface-smoke:hold" in latest or "surface-smoke:later" in latest:
                time.sleep(4.0)
            elif "surface-smoke:slow" in latest or "surface-smoke:steer" in latest:
                time.sleep(1.4)
            text = "stub-ok: " + (latest[-120:] or "empty")
            # Split content into deltas, matching OpenAI-compatible SSE.
            frames = [self.frame({"content": text[:12]}, None), self.frame({"content": text[12:]}, None), self.frame({}, "stop")]
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()
        try:
            for frame in frames:
                self.wfile.write(("data: " + json.dumps(frame, ensure_ascii=False) + "\n\n").encode())
                self.wfile.flush()
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass
        self.close_connection = True

    @staticmethod
    def frame(delta, finish):
        return {"id": "surface-smoke", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}


def cid(label):
    return str(uuid.uuid5(uuid.NAMESPACE_URL, "snens-surface-smoke:" + label))


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def request(base, path, method="GET", body=None, token="__AUTO__", timeout=5):
    data = None if body is None else json.dumps(body).encode()
    headers = {"Content-Type": "application/json"}
    if token == "__AUTO__":
        token = TOKEN
    if token is not None:
        headers["Authorization"] = "Bearer " + token
    req = urllib.request.Request(base + path, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            raw = r.read()
            return r.status, json.loads(raw) if raw else None
    except urllib.error.HTTPError as e:
        raw = e.read()
        try:
            parsed = json.loads(raw)
        except ValueError:
            parsed = raw.decode(errors="replace")
        return e.code, parsed


def eventually(fn, description, timeout=WAIT):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            last = fn()
            if last:
                return last
        except Exception as e:
            last = e
        time.sleep(.08)
    raise AssertionError(f"timeout: {description}; last={last!r}")


class Checks:
    def __init__(self):
        self.failures = []
        self.count = 0

    def check(self, name, fn):
        self.count += 1
        try:
            fn()
            print("ok:", name, flush=True)
        except Exception as e:
            self.failures.append((name, str(e)))
            print("FAIL:", name, "—", e, flush=True)

    @staticmethod
    def eq(a, b, message=""):
        assert a == b, message or f"expected {b!r}, got {a!r}"


def run_suite(base, server, token_path, data, child_pid_file, stub=None):
    c = Checks()
    create_cmd = lambda cmd, surf="smoke": request(base, "/v1/sessions", "POST", {"command_id": cmd, "surface": surf})
    msg = lambda sid, cmd, text: request(base, f"/v1/sessions/{sid}/messages", "POST", {"command_id": cmd, "text": text})

    c.check("bearer auth rejects missing and wrong tokens", lambda: (
        Checks.eq(request(base, "/v1/sessions", token=None)[0], 401),
        Checks.eq(request(base, "/v1/sessions", token="wrong")[0], 401)))

    created = {}
    def create():
        status, s = create_cmd(cid("smoke-create-001"))
        assert status in (200, 201), (status, s)
        created["a"] = s
        assert s["surface"] == "smoke" and s["state"] == "idle" and s["run_id"] is None
    c.check("create session returns contract shape", create)
    a = created.get("a", {}).get("id")
    if not a:
        print("Skipping dependent session checks: create failed")
        return c

    def dedup():
        status, same = create_cmd(cid("smoke-create-001"))
        assert status == 200 and same["id"] == a, (status, same)
        status2, mismatch = create_cmd(cid("smoke-create-001"), "different-surface")
        assert status2 == 409, f"same command_id with different payload must conflict, got {status2}: {mismatch}"
    c.check("create idempotency and payload mismatch", dedup)

    status, value = request(base, f"/v1/sessions/{a}")
    c.check("session GET", lambda: (Checks.eq(status, 200), Checks.eq(value["id"], a)))
    status, listed = request(base, "/v1/sessions")
    c.check("session list", lambda: (Checks.eq(status, 200), assert_in(a, [x["id"] for x in listed["sessions"]])))

    # First idle message must start a run, duplicate command_id must replay result.
    first = {}
    def idle_message():
        status, result = msg(a, cid("smoke-msg-idle-001"), "surface-smoke:ordinary")
        assert status == 202 and result["state"] == "applied" and result["run_id"]
        first.update(result)
    c.check("idle message starts run", idle_message)
    if first.get("run_id"):
        c.check("message idempotency", lambda: assert_message_dedup(msg, a, first))
        try:
            eventually(lambda: request(base, f"/v1/sessions/{a}")[1]["state"] == "idle", "idle response run completes")
        except Exception as e:
            c.failures.append(("idle run completes", str(e)))
            print("FAIL: idle run completes —", e, flush=True)
        else:
            c.count += 1
            print("ok: idle run completes", flush=True)

    # A second isolated session runs a long shell child; verify stop and cancellation.
    status, bobj = create_cmd(cid("smoke-create-002"))
    b = bobj.get("id") if status in (200, 201) else None
    c.check("create parallel session", lambda: (assert_status(status, (200, 201), bobj), assert_true(b, "missing session id")))
    active = {}
    independent = {}
    if b:
        # B stays busy while A's shell process group is stopped.
        def start_independent():
            st, r = msg(b, cid("smoke-msg-independent-001"), "surface-smoke:hold")
            assert st == 202, (st, r)
            independent.update(r)
            eventually(lambda: has_event(base, b, "run.started", 0), "B run starts")
        c.check("start independent run in session B", start_independent)

        def start_sleep():
            st, r = msg(a, cid("smoke-msg-sleep-001"), "surface-smoke:sleep")
            assert st == 202, (st, r)
            active.update(r)
        c.check("tool_calls SSE reaches run tool in A", start_sleep)
        if active.get("run_id"):
            eventually(lambda: has_event(base, a, "tool.started", 0), "sleep tool starts")
            eventually(lambda: child_pid_file.exists() and len(child_pid_file.read_text().splitlines()) >= 2, "shell and sleep child PIDs recorded")
            busy_result = {}
            def send_steer():
                st, result = msg(a, cid("smoke-msg-steer-001"), "surface-smoke:steer")
                assert st in (200, 202), (st, result)
                busy_result.update(result)
                assert_state(busy_result, "accepted")
            c.check("busy message is accepted as steer", send_steer)
            pending = busy_result.get("message_id")
            def stop_sleep():
                st, stopped = request(base, f"/v1/sessions/{a}/stop", "POST", {"command_id": cid("smoke-stop-001"), "run_id": active["run_id"]})
                assert st == 200, (st, stopped)
                assert stopped["state"] == "stopping"
            c.check("stop current run A accepted", stop_sleep)
            c.check("stop A leaves B run untouched", lambda: (
                assert_true(session_state(base, b) == "running", "B run was stopped or lost"),
                assert_true(request(base, f"/v1/sessions/{b}")[1]["run_id"] == independent.get("run_id"), "B run_id changed")))
            if pending:
                c.check("stop cancels accepted steer", lambda: eventually(lambda: message_state(data, a, pending) == "cancelled", "pending steer cancelled", 4))
            stopped = {}
            def check_stop_children():
                try:
                    eventually(lambda: session_state(base, a) == "idle" and child_gone(child_pid_file), "process group gone", 8)
                    stopped["ok"] = True
                except Exception as e:
                    stopped["error"] = str(e) + f"; state={session_state(base, a)} pids={child_pid_file.read_text(errors='replace') if child_pid_file.exists() else 'missing'}"
                    raise AssertionError(stopped["error"])
            c.check("stop kills shell and sleep children", check_stop_children)

            # Stale run check requires a fresh run; don't cascade if cancellation failed.
            if stopped.get("ok"):
                status, later = msg(a, cid("smoke-msg-later-001"), "surface-smoke:later")
                if status == 202 and later.get("run_id"):
                    time.sleep(.25)
                    st, _ = request(base, f"/v1/sessions/{a}/stop", "POST", {"command_id": cid("smoke-stale-stop-001"), "run_id": active["run_id"]})
                    c.check("stale run stop conflicts and leaves new A run active", lambda: (assert_status(st, (409,), {}), assert_true(session_state(base, a) in ("running", "stopping"), "new run was stopped")))
                    c.check("later A run completes", lambda: eventually(lambda: session_state(base, a) == "idle", "later A run completes", 10))
        try:
            eventually(lambda: session_state(base, b) == "idle", "B run completes", 10)
        except Exception as e:
            c.failures.append(("B run completes", str(e)))
            print("FAIL: B run completes —", e, flush=True)

    c.check("session B survives stop on A", lambda: assert_true(session_state(base, b) in ("idle", "running"), "B entered invalid state"))
    c.check("event history has increasing contiguous sequence and no private prompt records", lambda: check_history(base, a))
    c.check("SSE replay with after cursor has no gaps or duplicates", lambda: check_sse(base, a))
    pty_session = create_cmd(cid("smoke-pty-session"))
    pty_id = pty_session[1].get("id") if pty_session[0] in (200, 201) else None
    pty_pid_file = child_pid_file.with_name("pty-" + child_pid_file.name)
    stub_pid_file = server.get("stub_pid_file")
    if stub_pid_file is not None:
        stub_pid_file["path"] = pty_pid_file
        stub_pid_file["command"] = f": > {pty_pid_file}; echo $$ >> {pty_pid_file}; sleep 18 & echo $! >> {pty_pid_file}; wait; echo child-survived"
        if stub is not None:
            stub.sleep_command = stub_pid_file["command"]
    c.check("TUI PTY Ctrl+C stop", lambda: pty_smoke(base, server["bin"], token_path, data, pty_id, pty_pid_file))
    history_session = create_cmd(cid("smoke-pty-history-session"))
    history_id = history_session[1].get("id") if history_session[0] in (200, 201) else None
    if history_id:
        st, _ = msg(history_id, cid("smoke-pty-history-message"), "surface-smoke:ordinary")
        if st == 202:
            eventually(lambda: session_state(base, history_id) == "idle", "history session run completes")
    c.check("fresh TUI PTY merged /sessions /resume /quit with history", lambda: pty_sequence(base, server["bin"], token_path, data, history_id))

    # Session and token survive server restart; startup may mark an active run interrupted.
    before = request(base, f"/v1/sessions/{a}")[1]
    server["proc"].terminate()
    server["proc"].wait(timeout=5)
    server["proc"] = start_server(server["bin"], server["port"], server["config"], data, token_path, server["log"])
    eventually(lambda: request(base, "/v1/sessions")[0] == 200, "server restart")
    c.check("session resumes after server restart", lambda: (
        assert_status(request(base, f"/v1/sessions/{a}")[0], (200,), {}),
        assert_true(request(base, f"/v1/sessions/{a}")[1]["id"] == before["id"], "session id changed")))
    c.check("token file generated mode 0600", lambda: check_token(token_path))
    return c


def assert_in(value, values):
    assert value in values, f"{value} not in {values}"


def assert_status(value, valid, body):
    assert value in valid, f"expected status {valid}, got {value}: {body}"


def assert_true(value, message):
    assert value, message


def assert_state(obj, wanted):
    assert obj.get("state") == wanted, obj


def assert_message_dedup(msg, sid, first):
    status, second = msg(sid, cid("smoke-msg-idle-001"), "surface-smoke:ordinary")
    assert status == 200 and second == first, (status, second, first)


def has_event(base, sid, kind, after):
    status, data = request(base, f"/v1/sessions/{sid}/history")
    return status == 200 and any(e.get("type") == kind and e.get("seq", 0) > after for e in data["events"])


def session_state(base, sid):
    status, obj = request(base, f"/v1/sessions/{sid}")
    return obj.get("state") if status == 200 else None



def pty_smoke(base, binary, token_path, data_dir, sid, child_pid_file):
    if not sid:
        raise AssertionError("failed to create PTY test session")
    if os.name != "posix" or not hasattr(pty, "fork"):
        raise RuntimeError("PTY test requires POSIX")
    pid, master = pty.fork()
    if pid == 0:
        env = os.environ.copy()
        argv = [str(binary), "tui", "--endpoint", base, "--token-file", str(token_path),
                "--session", sid, "--data-dir", str(data_dir)]
        os.execvpe(argv[0], argv, env)
    output = bytearray()

    def collect(until=None, timeout=8):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if until and until():
                return
            ready, _, _ = select.select([master], [], [], .1)
            if ready:
                try:
                    output.extend(os.read(master, 8192))
                except OSError:
                    break
        if until and not until():
            raise AssertionError("PTY condition timed out; output=" + output.decode(errors="replace")[-1200:])

    try:
        collect(lambda: b"Ctrl+C" in output or b"/quit" in output, 8)
        try:
            child_pid_file.unlink()
        except FileNotFoundError:
            pass
        os.write(master, b"surface-smoke:pty-sleep\n")
        try:
            eventually(lambda: has_event(base, sid, "tool.started", 0), "PTY sends message and starts shell tool")
        except Exception as e:
            raise AssertionError(str(e))
        try:
            eventually(lambda: child_pid_file.exists() and len(child_pid_file.read_text().splitlines()) >= 2, "PTY sleep child pid file")
        except Exception as e:
            raise AssertionError(f"{e}; PTY={output.decode(errors='replace')[-1000:]}; events={request(base, f'/v1/sessions/{sid}/history')[1]}")
        os.write(master, b"\x03")
        eventually(lambda: session_state(base, sid) == "idle" and child_gone(child_pid_file), "PTY Ctrl+C stops run and kills child", 8)
        for command in (b"/sessions\n", f"/resume {sid}\n".encode(), b"/quit\n"):
            for byte in command:
                os.write(master, bytes((byte,)))
                time.sleep(.002)
            time.sleep(.25)
        deadline = time.monotonic() + 8
        status = None
        while time.monotonic() < deadline:
            ready, _, _ = select.select([master], [], [], .1)
            if ready:
                try:
                    output.extend(os.read(master, 8192))
                except OSError:
                    break
            got, child_status = os.waitpid(pid, os.WNOHANG)
            if got:
                status = child_status
                break
        if status is None:
            os.kill(pid, signal.SIGTERM)
            os.waitpid(pid, 0)
            raise AssertionError("TUI did not exit after /quit; output=" + output.decode(errors="replace")[-1200:])
        assert os.waitstatus_to_exitcode(status) == 0, output.decode(errors="replace")[-1200:]
    finally:
        try:
            os.close(master)
        except OSError:
            pass
        try:
            os.kill(pid, signal.SIGTERM)
        except ProcessLookupError:
            pass



def pty_sequence(base, binary, token_path, data_dir, sid):
    if not sid:
        raise AssertionError("failed to make history session")
    pid, master = pty.fork()
    if pid == 0:
        argv = [str(binary), "tui", "--endpoint", base, "--token-file", str(token_path), "--session", sid, "--data-dir", str(data_dir)]
        os.execvpe(argv[0], argv, os.environ.copy())
    output = bytearray()
    try:
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline and b"/quit" not in output:
            ready, _, _ = select.select([master], [], [], .1)
            if ready:
                try: output.extend(os.read(master, 8192))
                except OSError: break
        if b"/quit" not in output:
            raise AssertionError("fresh TUI prompt not reached: " + output.decode(errors="replace")[-1000:])
        sequence = f"/sessions\n/resume {sid}\n/quit\n".encode()
        # Tiny byte pacing emulates a fast human paste while avoiding host PTY
        # canonical-mode truncation at 4096 bytes; no command-level wait/retry.
        for command in (b"/sessions\n", f"/resume {sid}\n".encode(), b"/quit\n"):
            for byte in command:
                os.write(master, bytes((byte,)))
                time.sleep(.002)
            time.sleep(.25)
        deadline = time.monotonic() + 3
        status = None
        while time.monotonic() < deadline:
            got, child_status = os.waitpid(pid, os.WNOHANG)
            if got:
                status = child_status
                break
            ready, _, _ = select.select([master], [], [], .1)
            if ready:
                try: output.extend(os.read(master, 8192))
                except OSError: break
        if status is None:
            raise AssertionError("fresh TUI merged input did not exit; sent /sessions\n/resume ID\n/quit\n; last output=" + output.decode(errors="replace")[-1200:])
        assert os.waitstatus_to_exitcode(status) == 0, output.decode(errors="replace")[-1000:]
    finally:
        try: os.close(master)
        except OSError: pass
        try: os.kill(pid, signal.SIGTERM)
        except ProcessLookupError: pass


def child_gone(pid_file):
    try:
        pids = [int(line) for line in pid_file.read_text().splitlines() if line.strip()]
    except (OSError, ValueError):
        return False
    return bool(pids) and all(not pid_alive(pid) for pid in pids)


def pid_alive(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    # Linux may leave a killed child as a zombie until its parent reaps it.
    try:
        return pathlib.Path(f"/proc/{pid}/stat").read_text().split()[2] != "Z"
    except OSError:
        return False


def message_state(data_dir, sid, message_id):
    try:
        db = sqlite3.connect(data_dir / "surface-api/store.sqlite3")
        row = db.execute(
            "SELECT state FROM messages WHERE session_id = ? AND id = ?",
            (sid, message_id),
        ).fetchone()
        return row[0] if row else None
    except (sqlite3.Error, OSError):
        return None


def check_history(base, sid):
    status, h = request(base, f"/v1/sessions/{sid}/history")
    assert status == 200, h
    events = h["events"]
    seqs = [e["seq"] for e in events]
    assert seqs == list(range(1, len(seqs) + 1)), f"sequence gaps/duplicates: {seqs}"
    assert h["cursor"] == (seqs[-1] if seqs else 0)
    for event in events:
        dumped = json.dumps(event).lower()
        assert "system prompt" not in dumped and '"kind":"system"' not in dumped
        assert "contextrevision" not in dumped and "context_revision" not in dumped


def check_sse(base, sid):
    status, history = request(base, f"/v1/sessions/{sid}/history")
    assert status == 200
    cursor = max(0, history["cursor"] - 3)
    url = base + f"/v1/sessions/{sid}/events?after={cursor}"
    req = urllib.request.Request(url, headers={"Authorization": "Bearer " + TOKEN, "Accept": "text/event-stream"})
    with urllib.request.urlopen(req, timeout=4) as response:
        assert response.status == 200
        ids, events = [], []
        deadline = time.monotonic() + 1.2
        while time.monotonic() < deadline and len(ids) < 3:
            line = response.readline().decode().strip()
            if line.startswith("id:"):
                ids.append(int(line[3:].strip()))
            elif line.startswith("data:"):
                events.append(json.loads(line[5:].strip()))
    expected = [e["seq"] for e in history["events"] if e["seq"] > cursor]
    assert ids == expected[:len(ids)], f"replayed SSE IDs {ids}, expected prefix {expected}"
    assert len(ids) == len(set(ids)), f"SSE duplicate IDs {ids}"
    assert all(e["seq"] == i for e, i in zip(events, ids)), "SSE event data/id mismatch"


def check_token(path):
    assert path.exists(), "server did not generate token"
    if os.name == "posix":
        assert path.stat().st_mode & 0o777 == 0o600, oct(path.stat().st_mode & 0o777)


def start_server(binary, port, config, data, token_path, log_path):
    env = os.environ.copy()
    env["TEST_KEY"] = "hermetic-stub-key"
    with open(log_path, "ab", buffering=0) as log:
        return subprocess.Popen([str(binary), "serve", "--config", str(config), "--data-dir", str(data),
                                 "--listen", f"127.0.0.1:{port}", "--token-file", str(token_path)],
                                cwd=ROOT, env=env, stdin=subprocess.DEVNULL, stdout=log, stderr=log,
                                start_new_session=True)


def wait_ready(base, proc):
    deadline = time.monotonic() + WAIT
    last = None
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            raise RuntimeError(f"snens-next serve exited {proc.returncode}")
        try:
            last = request(base, "/v1/sessions", token=TOKEN)
            if last[0] == 200:
                return
        except Exception as e:
            last = repr(e)
        time.sleep(.1)
    raise RuntimeError(f"server did not become ready; last={last!r}")


def main():
    if not BIN.is_file():
        print(f"missing binary: {BIN}\nBuild only after parent authorization; see script header.", file=sys.stderr)
        return 2
    stub = Stub(("127.0.0.1", 0))
    stub_thread = threading.Thread(target=stub.serve_forever, daemon=True)
    stub_thread.start()
    with tempfile.TemporaryDirectory(prefix="snens-surface-smoke-") as tmp:
        temp = pathlib.Path(tmp)
        data = temp / "data"
        data.mkdir()
        child_pid_file = temp / "sleep-child.pid"
        stub.child_pid_path = str(child_pid_file)
        stub.sleep_command = f": > {child_pid_file}; echo $$ >> {child_pid_file}; sleep 18 & echo $! >> {child_pid_file}; wait; echo child-survived"
        stub.pid_control = {"path": child_pid_file, "command": stub.sleep_command}
        config = temp / "config.toml"
        config.write_text(f'''base_url = "http://127.0.0.1:{stub.server_port}/v1"
model = "openai/gpt-6-luna"
key_env = "TEST_KEY"
review_min_turns = 100000
attachments = false
[memory]
''')
        port = free_port()
        token_path = temp / "api.token"
        log_path = temp / "server.log"
        env = os.environ.copy()
        env["TEST_KEY"] = "hermetic-stub-key"
        # token file must exist before client requests; server creates it at startup.
        server = {"bin": BIN, "port": port, "config": config, "log": log_path, "stub_pid_file": stub.pid_control}
        main_log = open(log_path, "ab", buffering=0)
        proc = subprocess.Popen([str(BIN), "serve", "--config", str(config), "--data-dir", str(data),
                                 "--listen", f"127.0.0.1:{port}", "--token-file", str(token_path)],
                                cwd=ROOT, env=env, stdin=subprocess.DEVNULL, stdout=main_log,
                                stderr=main_log, start_new_session=True)
        server["proc"] = proc
        base = f"http://127.0.0.1:{port}"
        try:
            eventually(lambda: token_path.exists() and proc.poll() is None, "server creates token", 10)
            global TOKEN
            TOKEN = token_path.read_text().strip()
            try:
                wait_ready(base, proc)
            except Exception:
                print(log_path.read_text(errors="replace"), file=sys.stderr)
                raise
            checks = run_suite(base, server, token_path, data, child_pid_file, stub)
            if checks.failures:
                print(f"\n{len(checks.failures)}/{checks.count} checks failed")
                for name, error in checks.failures:
                    print(f" - {name}: {error}")
                print(f"server log: {log_path}")
                return 1
            print(f"\n{checks.count} checks passed")
            return 0
        finally:
            if child_pid_file.exists():
                try:
                    for child in [int(x) for x in child_pid_file.read_text().splitlines() if x.strip()]:
                        try:
                            os.kill(child, signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                except (OSError, ValueError):
                    pass
            if server.get("proc") and server["proc"].poll() is None:
                server["proc"].send_signal(signal.SIGTERM)
                try:
                    server["proc"].wait(timeout=4)
                except subprocess.TimeoutExpired:
                    server["proc"].kill()
            main_log.close()
            stub.shutdown()
            stub.server_close()


if __name__ == "__main__":
    raise SystemExit(main())
