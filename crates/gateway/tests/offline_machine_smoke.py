#!/usr/bin/env python3
"""Real loopback HTTP + SQLite restart, with synthetic actors and no executors.

cargo build -p gateway --no-default-features
python3 crates/gateway/tests/offline_machine_smoke.py [--binary /path/to/gateway]
All artifacts and private credentials stay under this repository's target/.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import secrets
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parents[3]
parser = argparse.ArgumentParser()
parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/gateway")
binary = parser.parse_args().binary.resolve()
os.umask(0o077)
(ROOT / "target").mkdir(exist_ok=True)
fixture = Path(tempfile.mkdtemp(prefix="offline-machine-smoke-", dir=ROOT / "target"))
(fixture / ".env").write_text("# Isolated fixture; no ancestor config.\n")
with socket.socket() as sock:
    sock.bind(("127.0.0.1", 0))
    port = sock.getsockname()[1]
base = f"http://127.0.0.1:{port}"
token = secrets.token_hex(32)
env = {
    "PATH": os.environ["PATH"], "DATABASE_BACKEND": "sqlite",
    "DATABASE_PATH": str(fixture / "private.sqlite3"), "GATEWAY_API_KEY": token,
    "GATEWAY_HOST": "127.0.0.1", "GATEWAY_PORT": str(port),
    "GATEWAY_UI_AUTH": "on", "DEFAULT_CHANNEL": "fixture-disabled",
    "HTTP_PROXY": "http://127.0.0.1:9", "HTTPS_PROXY": "http://127.0.0.1:9",
    "ALL_PROXY": "http://127.0.0.1:9", "NO_PROXY": "127.0.0.1,localhost",
}
http = urllib.request.build_opener(urllib.request.ProxyHandler({}))
server = None
log = (fixture / "server.log").open("wb")


def uid():
    return str(uuid.uuid4())


def api(path, body=None, actor=None, status=200):
    headers = {"Authorization": "Bearer " + token, "Content-Type": "application/json"}
    if actor:
        headers.update({"X-Agent-Session-Id": actor["origin"]["session_id"],
                        "X-Agent-Instance-Id": actor["origin"]["instance_id"],
                        "X-Agent-Provider": actor["origin"]["provider"],
                        "X-Agent-OS": actor["origin"]["os"],
                        "X-Gateway-Machine-Key": actor["machine_key"],
                        "X-Gateway-Session-Key": actor["session_key"]})
    request = urllib.request.Request(base + path, headers=headers,
                                    data=json.dumps(body).encode() if body is not None else None)
    try:
        with http.open(request, timeout=5) as response:
            code, data = response.status, json.load(response)
    except urllib.error.HTTPError as error:
        code = error.code
        raw = error.read()
        try:
            data = json.loads(raw)
        except ValueError:
            data = {"error": raw.decode(errors="replace")}
    assert code == status, (path, code, data, status)
    return data


def boot():
    global server
    server = subprocess.Popen([str(binary)], cwd=fixture, env=env, stdout=log, stderr=log)
    for _ in range(100):
        assert server.poll() is None, "fixture server exited"
        try:
            api("/v1/projects")
            return
        except OSError:
            time.sleep(0.1)
    raise AssertionError("fixture readiness timed out")


def stop():
    if server and server.poll() is None:
        server.terminate()
        try:
            server.wait(timeout=10)
        except subprocess.TimeoutExpired:
            server.kill()
            server.wait(timeout=10)


def machine(project, os_name):
    instance = uid()
    issued = subprocess.check_output([str(binary), "task-machine", "issue", project, instance, os_name],
                                     cwd=fixture, env=env, text=True).strip().splitlines()[-1]
    assert len(issued) == 64
    return {"instance_id": instance, "os": os_name, "key": issued}


def actor(machine):
    return {"origin": {"session_id": uid(), "instance_id": machine["instance_id"],
                       "os": machine["os"], "provider": "codex"},
            "generation": uid(), "machine_key": machine["key"], "session_key": secrets.token_hex(32)}


def register(project, peer):
    api(f"/v1/projects/{project}/task-sessions", {"generation": peer["generation"]}, peer)


def action(task, peer, operation, status=200, **fields):
    return api(f"/v1/projects/destination/task-dispatches/{task}/actions",
               {"request_id": uid(), "generation": peer["generation"], "operation": operation, **fields}, peer, status)


def recovery(peer):
    return api(f"/v1/projects/destination/task-dispatches/recovery?generation={peer['generation']}", actor=peer)


try:
    boot()
    for project in ["source", "destination"]:
        api("/v1/projects", {"ident": project, "channel": "fixture-disabled"})
    sender = actor(machine("source", "linux"))
    destination = machine("destination", "windows")  # No destination registration yet.
    register("source", sender)
    body = {"request_id": uid(), "generation": sender["generation"],
            "target_project_ident": "destination", "machine_target": {"instance_id": destination["instance_id"]},
            "title": "Offline machine fixture", "specification": "Synthetic reports; no provider or executor runs.",
            "required_os": "windows", "required_provider": "codex"}
    path = "/v1/projects/source/task-dispatches"
    accepted = api(path, body, sender)
    task = accepted["task"]["id"]
    assert accepted["dispatch"]["state"] == "pending_machine"
    assert accepted["dispatch"]["target"] is None
    assert accepted["dispatch"]["machine_target"]["os"] == "windows"
    assert accepted["task"]["status"] == "todo" and accepted["task"]["started_at"] is None
    stop()
    boot()  # Same private SQLite file, new native gateway process.
    assert api(path, body, sender) == accepted
    peers = [actor(destination) for _ in range(3)]
    for peer in peers:
        register("destination", peer)
        recovered = recovery(peer)
        assert len(recovered["machine_pending"]) == 1 and not recovered["pending"]

    def receive(peer):
        try:
            return action(task, peer, "receive")
        except AssertionError as error:
            assert error.args[0][1] == 409, error
            return None

    with ThreadPoolExecutor(max_workers=3) as pool:
        responses = list(pool.map(receive, peers))
    assert sum(response is not None for response in responses) == 1
    index = next(index for index, response in enumerate(responses) if response)
    owner = peers[index]
    received = responses[index]
    assert received["task"]["status"] == "todo" and received["task"]["owner_origin"] is None
    assert received["dispatch"]["claim_id"] is None and received["task"]["started_at"] is None
    for peer in peers:
        if peer is not owner:
            action(task, peer, "claim", status=409)
    claimed = action(task, owner, "claim")
    claim = claimed["dispatch"]["claim_id"]
    assert claimed["task"]["started_at"] is None
    execution = uid()
    action(task, owner, "start", claim_id=claim, execution_id=execution)
    restarted = {**owner, "generation": uid(), "session_key": secrets.token_hex(32)}
    register("destination", restarted)
    recovered = recovery(restarted)
    assert not recovered["machine_pending"] and not recovered["pending"]
    assert any(event["operation"] == "start" and event["dispatch"]["claim_id"] == claim
               for event in recovered["prior_claims"])
    action(task, restarted, "claim", status=409, claim_id=claim)
    offline_next = machine("destination", "windows")
    queued = action(task, owner, "handoff", claim_id=claim,
                    machine_target={"instance_id": offline_next["instance_id"]},
                    result="Synthetic executor stopped; explicit offline handoff")
    assert queued["dispatch"]["state"] == "pending_machine" and queued["task"]["status"] == "todo"
    assert any(event["operation"] == "handoff_prior_claim" and event["dispatch"]["claim_id"] == claim
               and event["dispatch"]["execution_id"] == execution for event in recovery(restarted)["prior_claims"])
    next_owner = actor(offline_next)
    register("destination", next_owner)
    before = api(f"/v1/projects/destination/tasks/{task}", actor=next_owner)
    lineage = recovery(next_owner)
    assert lineage["machine_pending"][0]["task_id"] == task
    prior = next(event for event in lineage["prior_claims"] if event["operation"] == "handoff_prior_claim")
    assert prior["dispatch"]["claim_id"] == claim and prior["dispatch"]["execution_id"] == execution
    assert prior["authentication"]["session"] == owner["origin"]
    assert prior["authentication"]["machine"]["instance_id"] == destination["instance_id"]
    assert prior["dispatch"]["finished_at"] is None and prior["dispatch"]["result"] is None
    assert api(f"/v1/projects/destination/tasks/{task}", actor=next_owner) == before
    assert before["status"] == "todo" and before["owner_origin"] is None and before["started_at"] is None
    action(task, next_owner, "receive")
    next_claim = action(task, next_owner, "claim")["dispatch"]["claim_id"]
    assert next_claim != claim
    action(task, next_owner, "start", status=409, claim_id=claim, execution_id=uid())
    outcome = {"status": "passed", "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
               "checks": ["offline acceptance", "SQLite/process restart", "durable dedup replay",
                          "three-session receipt race", "no implicit claim/start", "exact owner conflicts",
                          "read-only prior-generation recovery", "explicit offline handoff preserves prior claim", "new destination reads old authenticated lineage before claim"],
               "fixture_path": str(fixture)}
    (fixture / "result.json").write_text(json.dumps(outcome, indent=2) + "\n")
    print(json.dumps(outcome))
finally:
    stop()
    log.close()
