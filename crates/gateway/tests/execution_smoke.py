#!/usr/bin/env python3
"""Isolated execution smoke: cargo build -p gateway && python3 crates/gateway/tests/execution_smoke.py.

Only fake headless clients run. Fixtures stay under this repository's target/.
Pass --hold to keep the fixture gateway available for browser verification.
"""
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request

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


def wait_run(project, task=None):
    deadline = time.monotonic() + 25
    while time.monotonic() < deadline:
        for run in api(f"/v1/projects/{project}/execution/runs"):
            if (task is None or run["task_id"] == task) and run["status"] not in ("queued", "running"):
                return run
        time.sleep(0.2)
    raise AssertionError(f"execution did not finish: {project}")


def git(*args, cwd):
    subprocess.run(["git", *args], cwd=cwd, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)


def main():
    checkout = FIXTURE / "checkout"
    checkout.mkdir()
    git("init", cwd=checkout)
    git("-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "--allow-empty", "-m", "fixture", cwd=checkout)
    binaries = FIXTURE / "bin"
    binaries.mkdir()
    client = r'''#!/usr/bin/python3
import json, os, re, sys, urllib.request
from pathlib import Path
prompt = sys.stdin.read()
print(json.dumps({"cwd": os.getcwd(), "argv": sys.argv[1:]}))
if Path(sys.argv[0]).name == "claude":
    print("simulated provider outage", file=sys.stderr)
    sys.exit(1)
match = re.search(r"A new task ([0-9a-f-]+) was allocated to project ([a-z0-9-]+)", prompt)
if match:
    task, project = match.groups()
    if project == "failure":
        sys.exit(3)
    if project == "unfinished":
        sys.exit(0)
    url = os.environ["GATEWAY_EXECUTION_URL"] + f"/v1/projects/{project}/tasks/{task}"
    for status in ["in_progress", "done"]:
        req = urllib.request.Request(url, data=json.dumps({"status": status}).encode(), method="PATCH", headers={"Authorization": "Bearer " + os.environ["GATEWAY_EXECUTION_API_KEY"], "Content-Type": "application/json", "X-Agent-Id": "fixture-agent"})
        urllib.request.urlopen(req).close()
'''
    for name in ["claude", "codex"]:
        path = binaries / name
        path.write_text(client)
        path.chmod(0o700)
    # Environment is explicitly isolated: no real provider credentials or clients.
    env = {
        "PATH": f"{binaries}:/usr/bin:/bin", "HOME": str(FIXTURE),
        "GATEWAY_API_KEY": TOKEN, "GATEWAY_HOST": "127.0.0.1", "GATEWAY_PORT": str(PORT),
        "DATABASE_PATH": str(FIXTURE / "gateway.db"), "GATEWAY_UI_AUTH": "off",
        # Exercise a real clone without network access or another project checkout.
        "GIT_CONFIG_COUNT": "1", "GIT_CONFIG_KEY_0": f"url.{checkout.as_uri()}.insteadOf",
        "GIT_CONFIG_VALUE_0": "https://fixture.invalid/repository.git",
    }
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
        for project in ["mapped", "cloned", "source", "disabled", "cadence", "failure", "unfinished"]:
            api("/v1/projects", {"ident": project})
        api("/v1/projects/mapped/execution", {"local_path": str(checkout), "enabled": True, "on_task_received": True}, "PUT")
        task = api("/v1/projects/mapped/tasks", {"title": "Mapped execution"})
        run = wait_run("mapped", task["id"])
        assert run["status"] == "completed", run
        assert [a["candidate"] for a in run["attempts"]] == candidates
        assert [a["success"] for a in run["attempts"]] == [False, False, True]
        assert str(checkout) in run["attempts"][-1]["output"]
        api("/v1/projects/cloned/execution", {"allow_checkout": True, "clone_url": "https://fixture.invalid/repository.git", "enabled": True, "on_task_received": True}, "PUT")
        cloned_task = api("/v1/projects/cloned/tasks", {"title": "Checkout execution"})
        assert wait_run("cloned", cloned_task["id"])["status"] == "completed"
        assert len(list((FIXTURE / "managed").iterdir())) == 1
        second = api("/v1/projects/cloned/tasks", {"title": "Reuse managed checkout"})
        assert wait_run("cloned", second["id"])["status"] == "completed"
        assert len(list((FIXTURE / "managed").iterdir())) == 1
        delegated = api("/v1/projects/source/tasks/delegate", {"target_project_ident": "mapped", "title": "Delegated incident"})
        assert wait_run("mapped", delegated["target_task"]["id"])["status"] == "completed"
        parent = api("/v1/projects/source/tasks", {"title": "Parent work"})
        child = api(f'/v1/projects/source/tasks/{parent["id"]}/subtasks', {"title": "Security review", "target_project_ident": "mapped"})
        assert wait_run("mapped", child["id"])["status"] == "completed"
        children = api(f'/v1/projects/source/tasks/{parent["id"]}/subtasks')
        assert children[0]["status"] == "done"
        api("/v1/projects/cadence/execution", {"local_path": str(checkout), "enabled": True, "cadence_seconds": 60}, "PUT")
        assert wait_run("cadence")["status"] == "completed"
        api("/v1/projects/disabled/tasks", {"title": "Do not execute"})
        for project, status in [("failure", "failed"), ("unfinished", "needs_attention")]:
            api(f"/v1/projects/{project}/execution", {"local_path": str(checkout), "enabled": True, "on_task_received": True}, "PUT")
            task = api(f"/v1/projects/{project}/tasks", {"title": "Leave open on unsuccessful work"})
            run = wait_run(project, task["id"])
            assert run["status"] == status, run
            assert api(f'/v1/projects/{project}/tasks/{task["id"]}')["status"] == "todo"
        assert api("/v1/projects/disabled/execution/runs") == []
        assert len(api("/v1/projects/mapped/execution/runs")) == 3
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
