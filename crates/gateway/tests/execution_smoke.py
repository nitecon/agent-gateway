#!/usr/bin/env python3
"""Isolated execution smoke: cargo build -p gateway && python3 crates/gateway/tests/execution_smoke.py.

Only fake headless clients run. Fixtures stay under this repository's target/.
Pass --hold to keep the fixture gateway available for browser verification.
"""
import json
import base64
import hashlib
import http.client
import os
from pathlib import Path
import shutil
import socket
import sqlite3
import subprocess
import struct
import sys
import tempfile
import time
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parents[3]
FIXTURE = Path(tempfile.mkdtemp(prefix="execution-smoke-", dir=ROOT / "target"))
TOKEN = "isolated-execution-test-token"
with socket.socket() as sock:
    sock.bind(("127.0.0.1", 0))
    PORT = sock.getsockname()[1]
URL = f"http://127.0.0.1:{PORT}"


def api(path, body=None, method=None):
    request = urllib.request.Request(
        URL + path,
        data=json.dumps(body).encode() if body is not None else None,
        method=method or ("POST" if body is not None else "GET"),
        headers={"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json", "X-Agent-Id": "fixture-agent"},
    )
    with urllib.request.urlopen(request, timeout=5) as response:
        return json.load(response)


def wait_run(project, task=None, run_id=None):
    deadline = time.monotonic() + 25
    while time.monotonic() < deadline:
        for run in api(f"/v1/projects/{project}/execution/runs"):
            if (task is None or run["task_id"] == task) and (run_id is None or run["id"] == run_id) and run["status"] not in ("queued", "running"):
                return run
        time.sleep(0.2)
    raise AssertionError(f"execution did not finish: {project}")


def delegated_task(project, title):
    return api("/v1/projects/source/tasks/delegate", {"target_project_ident": project, "title": title})["target_task"]


def git(*args, cwd):
    subprocess.run(["git", *args], cwd=cwd, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)


class ExecutorSocket:
    """Minimal RFC6455 test peer using only Python's standard library."""
    def __init__(self):
        self.http = http.client.HTTPConnection("127.0.0.1", PORT, timeout=10)
        key = base64.b64encode(os.urandom(16)).decode()
        self.http.request("GET", "/v1/execution/connect", headers={"Authorization": f"Bearer {TOKEN}", "Connection": "Upgrade", "Upgrade": "websocket", "Sec-WebSocket-Version": "13", "Sec-WebSocket-Key": key})
        response = self.http.getresponse()
        assert response.status == 101, response.status
        expected = base64.b64encode(hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
        assert response.getheader("Sec-WebSocket-Accept") == expected
        self.sock = self.http.sock

    def send(self, value):
        payload = json.dumps(value).encode()
        mask = os.urandom(4)
        header = bytes([0x81, 0x80 | len(payload)]) if len(payload) < 126 else bytes([0x81, 0xFE]) + struct.pack("!H", len(payload))
        self.sock.sendall(header + mask + bytes(byte ^ mask[i % 4] for i, byte in enumerate(payload)))

    def read(self, length):
        result = b""
        while len(result) < length:
            chunk = self.sock.recv(length-len(result))
            assert chunk, "websocket closed"
            result += chunk
        return result

    def receive(self, kind):
        while True:
            opcode, size = self.read(2)
            length = size & 0x7F
            if length == 126:
                length = struct.unpack("!H", self.read(2))[0]
            elif length == 127:
                length = struct.unpack("!Q", self.read(8))[0]
            assert not size & 0x80
            payload = self.read(length)
            assert opcode & 0x0F == 1, opcode
            value = json.loads(payload)
            if value["type"] == kind:
                return value
            assert value["type"] in ("heartbeat", "heartbeat_ack"), value
            if value["type"] == "heartbeat":
                self.send({"type": "heartbeat"})

    def close(self):
        self.sock.close()
        self.http.close()


def main():
    checkout = FIXTURE / "checkout"
    checkout.mkdir()
    git("init", cwd=checkout)
    git("-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "--allow-empty", "-m", "fixture", cwd=checkout)
    binaries = FIXTURE / "bin"
    binaries.mkdir()
    client = r'''#!/usr/bin/python3
import json, os, re, sys, time, urllib.request
from pathlib import Path
prompt = sys.stdin.read()
print(json.dumps({"cwd": os.getcwd(), "argv": sys.argv[1:], "prompt": prompt}))
if Path(sys.argv[0]).name == "claude":
    print(json.dumps({"type": "result", "result": "Provider unavailable", "is_error": True}), flush=True)
    print("simulated provider outage", file=sys.stderr)
    sys.exit(1)
match = re.search(r"A new task ([0-9a-f-]+) was allocated to project ([a-z0-9-]+)", prompt)
if match:
    task, project = match.groups()
    print(json.dumps({"type": "item.started", "item": {"type": "command_execution"}}), flush=True)
    if project == "mapped":
        time.sleep(1.5)
    if project == "failure":
        print(json.dumps({"type": "turn.failed", "error": {"message": "fixture failure"}}), flush=True)
        sys.exit(3)
    if project == "unfinished":
        print(json.dumps({"type": "item.completed", "item": {"type": "agent_message", "text": "Blocked: need repository access. Task remains open."}}), flush=True)
        print(json.dumps({"type": "turn.completed"}), flush=True)
        sys.exit(0)
    url = os.environ["GATEWAY_EXECUTION_URL"] + f"/v1/projects/{project}/tasks/{task}"
    for status in ["in_progress", "done"]:
        req = urllib.request.Request(url, data=json.dumps({"status": status}).encode(), method="PATCH", headers={"Authorization": "Bearer " + os.environ["GATEWAY_EXECUTION_API_KEY"], "Content-Type": "application/json", "X-Agent-Id": "fixture-agent"})
        urllib.request.urlopen(req).close()
    print(json.dumps({"type": "item.completed", "item": {"type": "agent_message", "text": "Implemented fixture work. Validation passed. No blockers. " + os.environ["GATEWAY_EXECUTION_API_KEY"]}}), flush=True)
    print(json.dumps({"type": "turn.completed"}), flush=True)
'''
    for name in ["claude", "codex"]:
        path = binaries / name
        path.write_text(client)
        path.chmod(0o700)
    # Environment is explicitly isolated: no real provider credentials or clients.
    env = {
        "PATH": f"{binaries}:/usr/bin:/bin", "HOME": str(FIXTURE),
        "GATEWAY_API_KEY": TOKEN, "GATEWAY_HOST": "127.0.0.1", "GATEWAY_PORT": str(PORT),
        "DATABASE_PATH": str(FIXTURE / "gateway.db"), "GATEWAY_UI_AUTH": "on" if "--browser-auth" in sys.argv else "off",
        # Exercise a real clone without network access or another project checkout.
        "GIT_CONFIG_COUNT": "1", "GIT_CONFIG_KEY_0": f"url.{checkout.as_uri()}.insteadOf",
        "GIT_CONFIG_VALUE_0": "https://fixture.invalid/repository.git",
    }
    if "--browser-auth" in sys.argv:
        for username, flags in [("fixture-admin", ["--admin"]), ("fixture-member", [])]:
            subprocess.run([str(ROOT / "target/debug/gateway"), "user", "add", username, *flags], cwd=FIXTURE, env={**env, "GATEWAY_USER_PASSWORD": "fixture-password-123"}, check=True, capture_output=True)
    log = (FIXTURE / "gateway.log").open("w")
    process = subprocess.Popen([str(ROOT / "target/debug/gateway")], cwd=FIXTURE, env=env, stdout=log, stderr=log)
    try:
        for _ in range(100):
            if process.poll() is not None:
                raise AssertionError((FIXTURE / "gateway.log").read_text())
            try:
                api("/v1/execution/settings")
                break
            except OSError:
                time.sleep(0.1)
        else:
            raise AssertionError("gateway did not start")
        clients = api("/v1/execution/clients")
        assert all(item["path"].startswith(str(binaries)) for item in clients)
        candidates = [{"client": "claude", "model": "primary"}, {"client": "claude", "model": "secondary"}, {"client": "codex", "model": "fallback"}]
        api("/v1/execution/settings", {"data_directory": str(FIXTURE / "managed"), "candidates": candidates}, "PUT")
        instructions = api("/v1/execution/templates")
        instructions["task"] += "\nEdited task instructions for {{project}} / {{task_id}}."
        instructions["cadence"] += "\nEdited scheduled instructions for {{project}} / {{task_id}}."
        api("/v1/execution/templates", instructions, "PUT")
        assert api("/v1/execution/templates") == instructions
        for project in ["mapped", "cloned", "source", "disabled", "cadence", "failure", "unfinished", "legacy"]:
            api("/v1/projects", {"ident": project})
        api("/v1/projects/mapped/execution", {"local_path": str(checkout), "enabled": True, "on_task_received": True}, "PUT")
        api("/v1/projects/source/execution", {"local_path": str(checkout), "enabled": True, "on_task_received": True}, "PUT")
        planning = api("/v1/projects/mapped/tasks", {"title": "Ordinary planning must not execute"})
        with sqlite3.connect(FIXTURE / "gateway.db") as conn:
            conn.execute("UPDATE tasks SET created_at=0 WHERE id=?", (planning["id"],))
        task = delegated_task("mapped", "Mapped delegated execution")
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            live = api("/v1/projects/mapped/execution/runs")
            if live and live[0]["status"] == "running" and live[0]["progress"] == "Running command":
                assert live[0]["client"] == "codex"
                assert live[0]["started_at"] is not None
                assert live[0]["finished_at"] is None
                assert len(live[0]["attempts"]) == 2
                break
            time.sleep(0.05)
        else:
            raise AssertionError("progress did not become visible before client exit")
        run = wait_run("mapped", task["id"])
        assert run["status"] == "completed", run
        assert [a["candidate"] for a in run["attempts"]] == candidates
        assert [a["success"] for a in run["attempts"]] == [False, False, True]
        assert str(checkout) in run["attempts"][-1]["output"]
        assert f'Edited task instructions for mapped / {task["id"]}.' in run["attempts"][-1]["output"]
        assert "Edited scheduled instructions" not in run["attempts"][-1]["output"]
        assert run["summary_source"] == "client_final"
        assert "Validation passed" in run["summary"]
        assert TOKEN not in json.dumps(run)
        assert "[redacted]" in run["summary"]
        assert run["finished_at"] >= run["started_at"] >= run["created_at"]
        assert api(f'/v1/projects/mapped/execution/runs?task_id={task["id"]}')[0]["id"] == run["id"]
        assert api("/v1/execution/runs")[0]["id"] == run["id"]
        api("/v1/projects/cloned/execution", {"allow_checkout": True, "clone_url": "https://fixture.invalid/repository.git", "enabled": True, "on_task_received": True}, "PUT")
        cloned_task = delegated_task("cloned", "Checkout execution")
        assert wait_run("cloned", cloned_task["id"])["status"] == "completed"
        assert len(list((FIXTURE / "managed").iterdir())) == 1
        second = delegated_task("cloned", "Reuse managed checkout")
        assert wait_run("cloned", second["id"])["status"] == "completed"
        assert len(list((FIXTURE / "managed").iterdir())) == 1
        delegated = api("/v1/projects/source/tasks/delegate", {"target_project_ident": "mapped", "title": "Delegated incident"})
        assert wait_run("mapped", delegated["target_task"]["id"])["status"] == "completed"
        parent = api("/v1/projects/source/tasks", {"title": "Parent work"})
        child = api(f'/v1/projects/source/tasks/{parent["id"]}/subtasks', {"title": "Security review", "target_project_ident": "mapped"})
        children = api(f'/v1/projects/source/tasks/{parent["id"]}/subtasks')
        assert children[0]["status"] == "todo"
        api("/v1/projects/cadence/execution", {"local_path": str(checkout), "enabled": True, "cadence_seconds": 60}, "PUT")
        ordinary_cadence = api("/v1/projects/cadence/tasks", {"title": "Cadence must ignore planning"})
        cadence_task = delegated_task("cadence", "Scheduled delegated work")
        cadence_run = wait_run("cadence", cadence_task["id"])
        assert cadence_run["status"] == "completed"
        assert f'Edited scheduled instructions for cadence / {cadence_task["id"]}.' in cadence_run["attempts"][-1]["output"]
        assert "Edited task instructions" not in cadence_run["attempts"][-1]["output"]
        api("/v1/projects/disabled/tasks", {"title": "Do not execute"})
        for project, status in [("failure", "failed"), ("unfinished", "needs_attention")]:
            api(f"/v1/projects/{project}/execution", {"local_path": str(checkout), "enabled": True, "on_task_received": True}, "PUT")
            task = delegated_task(project, "Leave open on unsuccessful work")
            run = wait_run(project, task["id"])
            assert run["status"] == status, run
            assert api(f'/v1/projects/{project}/tasks/{task["id"]}')["status"] == "todo"
            if project == "unfinished":
                assert "Task remains open" in run["summary"]
        assert api("/v1/projects/disabled/execution/runs") == []
        assert len(api("/v1/projects/mapped/execution/runs")) == 2
        assert api("/v1/projects/source/execution/runs") == []
        assert api(f'/v1/projects/mapped/tasks/{planning["id"]}')["status"] == "todo"
        assert api(f'/v1/projects/mapped/tasks/{child["id"]}')["status"] == "todo"
        assert api(f'/v1/projects/cadence/tasks/{ordinary_cadence["id"]}')["status"] == "todo"
        assert len(api("/v1/projects/cadence/execution/runs")) == 1
        # Old-version queue entries must not bypass delegation checks after upgrade.
        api("/v1/projects/legacy/execution", {"local_path": str(checkout), "enabled": True, "on_task_received": True, "cadence_seconds": 60}, "PUT")
        old_task = api("/v1/projects/legacy/tasks", {"title": "Previously queued planning task"})
        for task_id, trigger in [(old_task["id"], "task"), (None, "cadence")]:
            run_id = str(uuid.uuid4())
            with sqlite3.connect(FIXTURE / "gateway.db") as conn:
                conn.execute("INSERT INTO execution_runs(id,project_ident,task_id,trigger,dedup_key,status,created_at) VALUES (?,'legacy',?,?,?,'queued',?)", (run_id,task_id,trigger,run_id,int(time.time()*1000)))
            run = wait_run("legacy", run_id=run_id)
            assert run["status"] == "cancelled", run
            assert run["attempts"] == [], run
        # Interactive assignments use the same durable run model, with no headless fallback.
        api("/v1/projects", {"ident": "interactive"})
        api("/v1/projects/interactive/execution", {"enabled": True, "on_task_received": True, "executor": "cmux"}, "PUT")
        interactive_task = delegated_task("interactive", "Visible interactive execution")
        session = {"workspace_id": "workspace-fixture", "surface_id": "surface-fixture", "session_id": "codex-native-fixture", "project_ident": "interactive", "client": "codex", "model": "fixture-model", "cwd": str(checkout), "state": "idle"}
        registration = {"type": "register", "protocol_version": 1, "instance_id": "cmux-fixture", "sessions": [session]}
        peer = ExecutorSocket()
        try:
            peer.send(registration)
            registered = peer.receive("registered")
            assignment = peer.receive("assignment")
            key, run_id = assignment["session_key"], assignment["run_id"]
            assert assignment["task_id"] == interactive_task["id"]
            assert registered["sessions"][0]["session_key"] == key
            assigned = api("/v1/projects/interactive/execution/runs")[0]
            assert assigned["status"] == "assigned" and not assigned["attempts"]
            peer.send({"type": "accepted", "run_id": run_id, "session_key": key})
            assert peer.receive("accepted")["status"] == "running"
            peer.send({"type": "report", "run_id": run_id, "session_key": key, "state": "waiting_input", "sequence": 1, "message": "Which environment should I use?"})
            assert peer.receive("recorded")["status"] == "waiting_input"
            assert api("/v1/projects/interactive/execution/runs")[0]["progress"] == "Which environment should I use?"
        finally:
            peer.close()
        deadline = time.monotonic()+5
        while api("/v1/projects/interactive/execution/runs")[0]["status"] != "needs_attention":
            assert time.monotonic()<deadline
            time.sleep(0.05)
        peer = ExecutorSocket()
        try:
            peer.send(registration)
            registered = peer.receive("registered")
            assert registered["sessions"][0]["run_id"] == run_id
            # Explicit reconciliation report resumes the acknowledged assignment; no prompt replay.
            peer.send({"type": "report", "run_id": run_id, "session_key": key, "state": "running", "sequence": 2, "message": "User answered; continuing"})
            assert peer.receive("recorded")["status"] == "running"
            for status in ["in_progress", "done"]:
                api(f'/v1/projects/interactive/tasks/{interactive_task["id"]}', {"status": status}, "PATCH")
            peer.send({"type": "report", "run_id": run_id, "session_key": key, "state": "finished", "sequence": 3, "message": "Finished", "summary": "Implemented interactive fixture; validation passed; no blockers."})
            assert peer.receive("recorded")["status"] == "completed"
        finally:
            peer.close()
        interactive_run = api("/v1/projects/interactive/execution/runs")[0]
        assert interactive_run["summary_source"] == "agent_report"
        assert not interactive_run["attempts"]
        # Reopen the same database and verify history/summary persistence.
        process.terminate()
        process.wait(timeout=10)
        process = subprocess.Popen([str(ROOT / "target/debug/gateway")], cwd=FIXTURE, env=env, stdout=log, stderr=log)
        deadline = time.monotonic()+10
        while True:
            try:
                restored = api("/v1/projects/interactive/execution/runs")[0]
                break
            except OSError:
                assert time.monotonic()<deadline
                time.sleep(0.1)
        assert restored["summary"] == interactive_run["summary"]
        assert restored["status"] == "completed"
        assert api("/v1/projects/mapped/execution/runs")[0]["summary_source"] == "client_final"
        print(json.dumps({"result": "passed", "url": URL, "fixture": str(FIXTURE), "parent_task": parent["id"]}), flush=True)
        if "--hold" in sys.argv:
            (ROOT / "target/execution-smoke.json").write_text(json.dumps({"url": URL, "fixture": str(FIXTURE), "parent_task": parent["id"]}))
            while True:
                time.sleep(1)
    finally:
        process.terminate()
        process.wait(timeout=10)
        log.close()


try:
    main()
except KeyboardInterrupt:
    pass
except BaseException:
    print(f"Fixture retained for debugging: {FIXTURE}", file=sys.stderr)
    raise
else:
    if "--hold" not in sys.argv:
        shutil.rmtree(FIXTURE)
