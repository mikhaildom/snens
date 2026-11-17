#!/usr/bin/env python3
"""Offline promotion smoke for the production snens serve/TUI entrypoints.

Build (coordinated by the parent) into the existing shared cargo target, then:
  SNENS_BIN=/path/to/target/debug/snens python3 scripts/production-smoke.py
Only stdlib Python is required. Starts local mock HTTP servers and temporary
snens processes; never installs a binary or contacts a real provider/service.
"""
from __future__ import annotations

import json
import os
import pathlib
import pty
import select
import signal
import sqlite3
import socket
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = pathlib.Path(__file__).resolve().parents[1]
def _target_dir() -> pathlib.Path | None:
    """Каталог сборки спрашиваем у cargo, а не хардкодим: у каждой машины свой.

    Вызывается лениво (только если `SNENS_BIN` не задан): прогон по готовому
    бинарю не должен требовать cargo в PATH.
    """
    if os.environ.get("SNENS_BIN"):
        return None
    if os.environ.get("CARGO_TARGET_DIR"):
        return pathlib.Path(os.environ["CARGO_TARGET_DIR"])
    meta = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"],
        cwd=ROOT, capture_output=True, text=True, check=True,
    )
    return pathlib.Path(json.loads(meta.stdout)["target_directory"])


TARGET = _target_dir()
BIN = pathlib.Path(os.environ.get("SNENS_BIN") or TARGET / "debug/snens")
WAIT = 10


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def http_json(base, path, token, timeout=4):
    req = urllib.request.Request(base + path, headers={"Authorization": "Bearer " + token})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as response:
            raw = response.read()
            return response.status, json.loads(raw) if raw else None
    except urllib.error.HTTPError as error:
        raw = error.read()
        try:
            value = json.loads(raw)
        except Exception:
            value = raw.decode(errors="replace")
        return error.code, value


def eventually(predicate, label, timeout=WAIT):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            last = predicate()
            if last:
                return last
        except Exception as error:
            last = repr(error)
        time.sleep(.05)
    raise AssertionError(f"timeout waiting for {label}; last={last!r}")


def rec(kind, ts, **fields):
    return {"kind": kind, "ts": ts, **fields}


def fixture_log(session_id):
    calls = [{"id": "call-1", "name": "run", "args": "{\"command\":\"true\"}"}]
    rows = [
        rec("meta", 1, model="mock/model", parent="", surface="legacy"),
        rec("system", 2, text="private system prompt must not appear in transcript"),
        rec("user", 10, text="legacy-user-marker", source="cli", parts=[]),
        rec("usage", 11, in_tok=3, cached_tok=0, out_tok=1, cost_micro=0,
            upstream_id="", fingerprint="", finish_reason="stop", reason_tok=0,
            encrypted_reasoning=False),
        rec("assistant", 12, text="legacy-assistant-marker", tool_calls=[], reasoning="private reasoning"),
        rec("assistant", 13, text=None, tool_calls=calls, reasoning=None),
        rec("tool_result", 14, call_id="call-1", tool="run", argv=["true"], exit=0,
            bytes=2, ms=1, body={"t": "inline", "text": "legacy-tool-marker"}, parts=[]),
        rec("done", 15, status="ok", turns=1),
    ]
    data = b"".join((json.dumps(row, separators=(",", ":")) + "\n").encode() for row in rows)
    return data, rows


class MockLLM(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True

    def __init__(self, addr):
        super().__init__(addr, MockLLMHandler)
        self.requests = []
        self.lock = threading.Lock()
        self.delta_sent = threading.Event()
        # `release` выставляет main() в finally: иначе висящий ответ
        # (release.wait()) держит поток и `shutdown()` не дожидается его.
        self.release = threading.Event()


class MockLLMHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def do_POST(self):
        size = int(self.headers.get("Content-Length", "0"))
        body = json.loads(self.rfile.read(size))
        with self.server.lock:
            self.server.requests.append(body)
        crash_turn = any(message.get("role") == "user" and "cursor-crash-marker" in str(message.get("content")) for message in body.get("messages", []))
        frames = [
            {"id": "offline-mock", "choices": [{"index": 0, "delta": {"content": "mock-ok"}, "finish_reason": None}]},
            {"id": "offline-mock", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
        ]
        if crash_turn:
            first = ("data: " + json.dumps(frames[0]) + "\n\n").encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Connection", "close")
            self.end_headers()
            try:
                self.wfile.write(first)
                self.wfile.flush()
                self.server.delta_sent.set()
                self.server.release.wait()
            except (BrokenPipeError, ConnectionResetError):
                pass
            self.close_connection = True
            return
        payload = b"".join(("data: " + json.dumps(frame) + "\n\n").encode() for frame in frames) + b"data: [DONE]\n\n"
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


class Checks:
    def __init__(self):
        self.count = 0
        self.failures = []

    def check(self, name, fn):
        self.count += 1
        try:
            fn()
            print("ok:", name, flush=True)
        except Exception as error:
            self.failures.append((name, str(error)))
            print("FAIL:", name, "—", repr(error), flush=True)


def write_config(path, llm, dirs):
    path.write_text("\n".join([
        f'base_url = "http://127.0.0.1:{llm.server_port}/v1"',
        'model = "openai/gpt-6-luna"',
        'key_env = "PROMOTION_MOCK_KEY"',
        f'sessions_dir = "{dirs["sessions"]}"',
        f'tools_dir = "{dirs["tools"]}"',
        f'tickets_dir = "{dirs["tickets"]}"',
        f'mail_dir = "{dirs["mail"]}"',
        f'skills_dir = "{dirs["skills"]}"',
        'review_min_turns = 100000',
        'attachments = false',
        '[memory]',
        'cmd = ""',
        '',
    ]))


def start_serve(config, data, port, token_path, log_path):
    env = os.environ.copy()
    env["PROMOTION_MOCK_KEY"] = "offline-placeholder"
    log = open(log_path, "ab", buffering=0)
    proc = subprocess.Popen(
        [str(BIN), "serve", "--config", str(config), "--listen", f"127.0.0.1:{port}", "--token-file", str(token_path)],
        cwd=ROOT, env=env, stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True)
    return proc, log


def wait_server(base, token, proc):
    def ready():
        if proc.poll() is not None:
            raise RuntimeError(f"serve exited {proc.returncode}")
        return http_json(base, "/v1/sessions", token)[0] == 200
    eventually(ready, "authenticated production server")


def check_projection(base, token, sid, raw_before, raw_path):
    status, history = http_json(base, f"/v1/sessions/{sid}/history", token)
    assert status == 200, (status, history)
    assert set(history) == {"cursor", "events", "messages"}, history.keys()
    assert isinstance(history["cursor"], int) and isinstance(history["events"], list)
    messages = history["messages"]
    assert [m["role"] for m in messages] == ["user", "assistant", "assistant", "tool"]
    assert [m["seq"] for m in messages] == [3, 5, 6, 7], messages
    assert [m["ts"] for m in messages] == [10, 12, 13, 14]
    assert messages[0]["content"] == "legacy-user-marker" and messages[0]["source"] == "cli"
    assert messages[1]["content"] == "legacy-assistant-marker"
    assert messages[1]["reasoning"] == "private reasoning"
    assert messages[2]["content"] is None and messages[2]["tool_calls"] == [
        {"id": "call-1", "type": "function", "function": {"name": "run", "arguments": '{"command":"true"}'}}
    ]
    assert messages[3]["content"] == "legacy-tool-marker"
    assert messages[3]["tool_call_id"] == "call-1" and messages[3]["name"] == "run" and messages[3]["exit"] == 0
    assert all("private system prompt" not in json.dumps(message) for message in messages)
    assert history["cursor"] == 0 and history["events"] == [], "legacy transcript must not be imported as API delivery events"
    db_path = raw_path.parents[1] / "surface-api" / "store.sqlite3"
    with sqlite3.connect(db_path) as connection:
        counts = {table: connection.execute(f"SELECT COUNT(*) FROM {table} WHERE {column}=?", (sid,)).fetchone()[0]
                  for table, column in (("sessions", "id"), ("messages", "session_id"), ("events", "session_id"))}
        assert counts["messages"] == 0 and counts["events"] == 0, f"legacy transcript imported into SQLite: {counts}"
        # Session metadata may be materialized for an explicitly opened legacy ID;
        # the transcript itself must remain JSONL-authoritative.
    assert raw_path.read_bytes() == raw_before, "server rewrote authoritative JSONL"


def post_json(base, path, token, payload):
    data = json.dumps(payload).encode()
    request = urllib.request.Request(base + path, data=data, headers={"Authorization": "Bearer " + token, "Content-Type": "application/json"}, method="POST")
    try:
        with urllib.request.urlopen(request, timeout=4) as response:
            return response.status, json.loads(response.read())
    except urllib.error.HTTPError as error:
        try:
            return error.code, json.loads(error.read())
        except Exception:
            return error.code, None


def create_session(base, token):
    payload = json.dumps({"command_id": str(uuid.uuid4()), "surface": "production-smoke"}).encode()
    req = urllib.request.Request(base + "/v1/sessions", data=payload, headers={"Authorization": "Bearer " + token, "Content-Type": "application/json"}, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=4) as response:
            return response.status, json.loads(response.read())
    except urllib.error.HTTPError as error:
        return error.code, json.loads(error.read())


def production_server_smoke(temp, checks, llm):
    data = temp / "instance"
    dirs = {name: data / name for name in ("sessions", "tools", "tickets", "mail", "skills")}
    for directory in dirs.values():
        directory.mkdir(parents=True)
    config = temp / "config.toml"
    write_config(config, llm, dirs)
    sid = "1791490000-12345"  # deployed legacy IDs are timestamp-counter, not UUIDs
    raw, rows = fixture_log(sid)
    log_path = dirs["sessions"] / f"{sid}.jsonl"
    log_path.write_bytes(raw)
    uuid_sid = str(uuid.uuid4())
    uuid_raw, _ = fixture_log(uuid_sid)
    uuid_path = dirs["sessions"] / f"{uuid_sid}.jsonl"
    uuid_path.write_bytes(uuid_raw)
    invalid = dirs["sessions"] / "not-a-session.jsonl"
    invalid.write_text('{"kind":"user"}\n')
    invalid_suffix = dirs["sessions"] / f"{sid}junk.jsonl"
    invalid_suffix.write_text('{"kind":"meta"}\n')
    (dirs["sessions"] / f"{uuid.uuid4()}.attachments").mkdir()
    token_path = temp / "api.token"
    port = free_port()
    base = f"http://127.0.0.1:{port}"
    server_log = temp / "serve.log"
    proc, log = start_serve(config, data, port, token_path, server_log)
    try:
        eventually(lambda: token_path.exists() and proc.poll() is None, "token creation")
        token = token_path.read_text().strip()
        assert len(token) >= 32
        wait_server(base, token, proc)

        def discovery():
            status, response = http_json(base, "/v1/sessions", token)
            assert status == 200, response
            matches = [row for row in response["sessions"] if row["id"] == sid]
            # `legacy` — историческое имя метки; discovery отдаёт такие сессии как
            # `local` (см. build_session: «surface local вместо legacy»). Остальные
            # поля в строке есть, поэтому сверяем подмножество, а не всю запись.
            assert len(matches) == 1, matches
            row = matches[0]
            assert row["id"] == sid, row
            assert row["surface"] == "local", row
            assert row["state"] == "idle" and row["run_id"] is None, row
            ids = {row["id"] for row in response["sessions"]}
            assert sid in ids and uuid_sid in ids, ids
            assert invalid.stem not in ids and invalid_suffix.stem not in ids
            assert all(row["surface"] == "local" for row in response["sessions"] if row["id"] in (sid, uuid_sid))
        checks.check("production serve discovers timestamp-counter and UUID JSONL, rejects invalid basenames", discovery)
        checks.check("JSONL authoritative history projection preserves physical sequence and raw bytes",
                     lambda: check_projection(base, token, sid, raw, log_path))
        check_tui_flows(config, base, token_path, token, sid, llm, checks)

        def late_legacy_post_without_discovery():
            late_id = f"{int(time.time())}-987654"
            late_path = dirs["sessions"] / f"{late_id}.jsonl"
            prefix = (json.dumps(rec("meta", int(time.time()), model="legacy", parent=""), separators=(",", ":")) + "\n").encode()
            late_path.write_bytes(prefix)
            command_id = str(uuid.uuid4())
            payload = {"command_id": command_id, "text": "late-legacy-post-marker"}
            status, first = post_json(base, f"/v1/sessions/{late_id}/messages", token, payload)
            assert status == 202, (status, first)
            status2, replay = post_json(base, f"/v1/sessions/{late_id}/messages", token, payload)
            assert status2 == 200 and replay == first, (status2, replay, first)
            wait_idle(base, token, late_id)
            raw_after = late_path.read_bytes()
            assert raw_after.startswith(prefix), "append rewrote preexisting legacy JSONL bytes"
            assert raw_after.count(b"late-legacy-post-marker") == 1, raw_after
            status, history = http_json(base, f"/v1/sessions/{late_id}/history", token)
            assert status == 200, history
            # Accepted API messages append to JSONL; the mock assistant reply is
            # also durable. Assert the user projection and exactly one raw append,
            # without assuming a user-only transcript.
            user_messages = [m for m in history["messages"] if m.get("role") == "user"]
            assert [m.get("content") for m in user_messages] == ["late-legacy-post-marker"], history
            status, listed = http_json(base, "/v1/sessions", token)
            assert status == 200 and any(s["id"] == late_id and s["surface"] == "local" for s in listed["sessions"]), listed
        checks.check("late legacy JSONL POST hydrates on demand without prior GET/list; retry is idempotent", late_legacy_post_without_discovery)

        def invalid_history():
            assert http_json(base, "/v1/sessions/not-a-session/history", token)[0] == 400
            assert http_json(base, f"/v1/sessions/{sid}junk/history", token)[0] == 400
            assert http_json(base, f"/v1/sessions/{uuid.uuid4()}/history", token)[0] == 404
        checks.check("history distinguishes invalid and unknown IDs", invalid_history)

        def external_writer_busy_guard():
            # Эксклюзивность пишущего держит отдельный `<id>.lock`, а не сам
            # журнал (на Windows flock журнала запрещал чтение — см. 1cf1e76).
            # Лочим тот же файл, который лочит ядро.
            import fcntl
            lock_path = log_path.with_suffix(".lock")
            lock_file = open(lock_path, "ab")
            fcntl.flock(lock_file.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
            before_size = log_path.stat().st_size
            try:
                payload = json.dumps({"command_id": str(uuid.uuid4()), "text": "must-not-start"}).encode()
                req = urllib.request.Request(base + f"/v1/sessions/{sid}/messages", data=payload, headers={"Authorization": "Bearer " + token, "Content-Type": "application/json"}, method="POST")
                try:
                    with urllib.request.urlopen(req, timeout=4) as response:
                        status, body = response.status, response.read()
                except urllib.error.HTTPError as error:
                    status, body = error.code, error.read()
                assert status == 409, (status, body)
                assert log_path.stat().st_size == before_size, "busy legacy session JSONL was modified"
                state_code, state = http_json(base, f"/v1/sessions/{sid}", token)
                assert state_code == 200 and state["state"] == "idle" and state["run_id"] is None, state
            finally:
                fcntl.flock(lock_file.fileno(), fcntl.LOCK_UN)
                lock_file.close()
        checks.check("session writer lock rejects API run without leaving running state or JSONL changes", external_writer_busy_guard)

        def transient_cursor_survives_crash_restart():
            nonlocal proc, log
            status, response = create_session(base, token)
            assert status in (200, 201), response
            session = response.get("session", response)
            crash_id = session["id"]
            _, before_history = http_json(base, f"/v1/sessions/{crash_id}/history", token)
            initial_cursor = before_history["cursor"]
            body = json.dumps({"command_id": str(uuid.uuid4()), "text": "cursor-crash-marker"}).encode()
            req = urllib.request.Request(base + f"/v1/sessions/{crash_id}/messages", data=body, headers={"Authorization": "Bearer " + token, "Content-Type": "application/json"}, method="POST")
            with urllib.request.urlopen(req, timeout=4) as response:
                assert response.status == 202
            eventually(lambda: llm.delta_sent.is_set(), "mock provider sends delta and holds stream")
            def transient_cursor():
                code, history = http_json(base, f"/v1/sessions/{crash_id}/history", token)
                assert code == 200, history
                durable = max((event["seq"] for event in history["events"]), default=initial_cursor)
                if history["cursor"] <= durable:
                    return None
                return history["cursor"]
            observed = eventually(transient_cursor, "volatile SSE cursor beyond durable delivery history")
            proc.kill()
            proc.wait(timeout=5)
            log.close()
            restarted, restarted_log = start_serve(config, data, port, token_path, server_log)
            try:
                wait_server(base, token, restarted)
                code, recovered = http_json(base, f"/v1/sessions/{crash_id}/history", token)
                assert code == 200 and recovered["cursor"] > observed, (observed, code, recovered)
                assert any(event["type"] == "run.finished" and event["seq"] > observed for event in recovered["events"]), recovered
            except Exception:
                if restarted.poll() is None:
                    restarted.terminate(); restarted.wait(timeout=5)
                restarted_log.close()
                raise
            # Leave the replacement process running for the surrounding suite cleanup.
            proc, log = restarted, restarted_log
        checks.check("transient SSE cursor is not reused after crash and durable recovery", transient_cursor_survives_crash_restart)

        def empty_session_history():
            status, response = create_session(base, token)
            assert status in (200, 201), response
            session = response.get("session", response)
            empty_id = session["id"]
            status, empty = http_json(base, f"/v1/sessions/{empty_id}/history", token)
            assert status == 200 and empty["messages"] == [], (status, empty)
            assert isinstance(empty["cursor"], int) and isinstance(empty["events"], list)
            db = data / "surface-api" / "store.sqlite3"
            with sqlite3.connect(db) as connection:
                count = connection.execute("SELECT COUNT(*) FROM sessions WHERE id=?", (empty_id,)).fetchone()[0]
                assert count == 1, "API session delivery row missing"
                transcript_tables = [row[0] for row in connection.execute("SELECT name FROM sqlite_master WHERE type='table' AND name LIKE '%transcript%'")]
                assert not transcript_tables, transcript_tables
        checks.check("empty transcript remains JSONL-authoritative with no SQLite transcript table", empty_session_history)
        checks.check("token permissions are private", lambda: check_token(token_path))

        proc.terminate()
        proc.wait(timeout=5)
        log.close()
        proc, log = start_serve(config, data, port, token_path, server_log)
        wait_server(base, token, proc)
        checks.check("legacy discovery/history and raw JSONL survive server restart",
                     lambda: (check_projection(base, token, sid, raw, log_path),
                              assert_discovered(base, token, sid)))
        checks.check("legacy run-child still executes with explicit temp config and mock provider",
                     lambda: check_run_child(config, dirs, llm))
    finally:
        if proc.poll() is None:
            proc.send_signal(signal.SIGTERM)
            try:
                proc.wait(timeout=4)
            except subprocess.TimeoutExpired:
                proc.kill()
        if not log.closed:
            log.close()


def check_run_child(config, dirs, llm):
    child_id = str(uuid.uuid4())
    env = os.environ.copy()
    env["SNENS_CONFIG"] = str(config)
    env["PROMOTION_MOCK_KEY"] = "offline-placeholder"
    env["SNENS_PROMPT"] = "run-child offline compatibility marker"
    env["SNENS_NEXT_INTERNAL"] = "1"
    completed = subprocess.run([str(BIN), "run-child", "--session", child_id], cwd=ROOT, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=12, check=False)
    assert completed.returncode == 0, (completed.returncode, completed.stdout[-1000:], completed.stderr[-1000:])
    log_path = dirs["sessions"] / f"{child_id}.jsonl"
    assert log_path.is_file(), "run-child did not create JSONL session"
    rows = [json.loads(line) for line in log_path.read_text().splitlines()]
    assert any(row.get("kind") == "user" and row.get("source") == "agent" and row.get("text") == env["SNENS_PROMPT"] for row in rows), rows
    assert any(row.get("kind") == "assistant" and row.get("text") == "mock-ok" for row in rows), rows
    assert llm.requests, "run-child did not contact local mock provider"


def assert_discovered(base, token, sid):
    status, result = http_json(base, "/v1/sessions", token)
    assert status == 200 and any(s["id"] == sid for s in result["sessions"]), result


def run_tui(config, endpoint, token_path, session_id, commands, timeout=8):
    env = os.environ.copy()
    env["SNENS_CONFIG"] = str(config)
    child, master = pty.fork()
    if child == 0:
        argv = [str(BIN), "tui", "--config", str(config), "--endpoint", endpoint, "--token-file", str(token_path)]
        if session_id is not None:
            argv.extend(["--session", session_id])
        # `execve` может не состояться (нет бинаря, нет прав, плохой argv).
        # Исключение вылетело бы в ветку родителя, и форк ушёл бы дальше по тесту
        # с копией его состояния — вплоть до дедлока в llm.shutdown().
        try:
            os.execve(str(BIN), argv, env)
        finally:
            os._exit(127)
    output = bytearray()
    deadline = time.monotonic() + timeout
    sent = False
    try:
        while time.monotonic() < deadline:
            ready, _, _ = select.select([master], [], [], .1)
            if ready:
                try:
                    chunk = os.read(master, 8192)
                except OSError:
                    break
                if not chunk:
                    break
                output.extend(chunk)
                if not sent and "сессия ".encode() in output:
                    time.sleep(.15)
                    for command in commands:
                        if command.startswith("@wait:"):
                            marker = command[len("@wait:"):].encode()
                            gate = time.monotonic() + timeout
                            while marker not in output and time.monotonic() < gate:
                                ready_more, _, _ = select.select([master], [], [], .1)
                                if ready_more:
                                    try:
                                        output.extend(os.read(master, 8192))
                                    except OSError:
                                        break
                            if marker not in output:
                                raise AssertionError(f"PTY did not render marker {marker!r}: {output[-1500:]!r}")
                            continue
                        os.write(master, command.encode() + b"\n")
                        time.sleep(.25)
                    sent = True
            done, _status = os.waitpid(child, os.WNOHANG)
            if done:
                child = None
                break
        return bytes(output).decode(errors="replace")
    finally:
        # Ребёнка убираем в finally, а не в теле try: любое исключение выше
        # (AssertionError про маркер, ошибка записи в pty) оставляло бы живой
        # процесс TUI — а он держит сессию и ломает следующие проверки.
        if child is not None:
            try:
                os.kill(child, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                os.waitpid(child, 0)
            except ChildProcessError:
                pass
        os.close(master)


def wait_idle(base, token, sid):
    def idle():
        status, state = http_json(base, f"/v1/sessions/{sid}", token)
        return status == 200 and state.get("state") == "idle"
    eventually(idle, f"session {sid} idle")


def check_tui_flows(config, base, token_path, token, legacy_id, llm, checks):
    def legacy_resume():
        text = run_tui(config, base, token_path, legacy_id, ["/sessions", f"/resume {legacy_id}", "/quit"])
        assert f"ты: legacy-user-marker" in text, text[-1800:]
        expected_call = '[run] ' + json.dumps('{"command":"true"}')
        assert "legacy-assistant-marker" in text and expected_call in text, text[-1800:]
        assert "[run] exit=0 legacy-tool-marker" in text, text[-1800:]
        assert text.count("legacy-user-marker") == 1, f"explicit --session open renders each transcript row once: {text[-1800:]}"
        assert "/sessions" in text, text[-1800:]
        assert "private system prompt" not in text, text[-1800:]
    checks.check("production TUI resumes JSONL transcript once per open and lists legacy discovery", legacy_resume)

    def live_transcript():
        status, response = create_session(base, token)
        assert status in (200, 201), response
        session = response.get("session", response)
        sid = session["id"]
        output = run_tui(config, base, token_path, sid, ["production-smoke-live-marker", "@wait:mock-ok", "/quit"])
        wait_idle(base, token, sid)
        assert llm.requests, "mock provider was not used"
        status, history = http_json(base, f"/v1/sessions/{sid}/history", token)
        assert status == 200 and history["cursor"] > 0 and history["events"], history
        assert [message["role"] for message in history["messages"]] == ["user", "assistant"]
        assert output.count("mock-ok") == 1, output[-1800:]
    checks.check("TUI renders authoritative transcript once and does not replay transcript from API events", live_transcript)

    def omitted_session_discovers():
        text = run_tui(config, base, token_path, None, ["/sessions", "/quit"])
        status, sessions = http_json(base, "/v1/sessions", token)
        assert status == 200 and legacy_id in {item["id"] for item in sessions["sessions"]}
        assert "нет TUI-сессии" in text and "/sessions" in text, text[-1600:]
    checks.check("TUI without --session discovers sessions from API", omitted_session_discovers)


def check_token(path):
    assert path.exists()
    if os.name == "posix":
        assert path.stat().st_mode & 0o777 == 0o600, oct(path.stat().st_mode & 0o777)


def main():
    if not BIN.is_file():
        print(f"missing binary: {BIN}; build only after parent coordination", file=os.sys.stderr)
        return 2
    checks = Checks()
    llm = MockLLM(("127.0.0.1", 0))
    thread = threading.Thread(target=llm.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix="snens-production-smoke-") as root:
            production_server_smoke(pathlib.Path(root), checks, llm)
    finally:
        # Сначала отпускаем висящий SSE-ответ: `shutdown()` иначе ждёт поток,
        # который стоит в `release.wait()`.
        llm.release.set()
        llm.shutdown()
        llm.server_close()
    if checks.failures:
        print(f"\n{len(checks.failures)}/{checks.count} checks failed")
        for name, error in checks.failures:
            print(" -", name, ":", error)
        return 1
    print(f"\n{checks.count} checks passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
