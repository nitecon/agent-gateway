#!/usr/bin/env python3
"""Real HTTP/restart OKF contract smoke, isolated DB, no external services.

Usage: python3 crates/gateway/tests/memory_okf_smoke.py /path/to/gateway
"""
import copy
import json
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[3]
TOKEN = "isolated-okf-test-token"
BINARY = Path(sys.argv[1] if len(sys.argv) > 1 else ROOT / "target/debug/gateway").resolve()
FIXTURE = Path(tempfile.mkdtemp(prefix="memory-okf-smoke-", dir=ROOT / "target"))
with socket.socket() as sock:
    sock.bind(("127.0.0.1", 0))
    PORT = sock.getsockname()[1]
URL = f"http://127.0.0.1:{PORT}"
ENV = {"PATH": os.environ["PATH"], "GATEWAY_API_KEY": TOKEN,
       "GATEWAY_HOST": "127.0.0.1", "GATEWAY_PORT": str(PORT),
       "DATABASE_PATH": str(FIXTURE / "gateway.db")}


def api(path, body=None, capable=False, token=TOKEN, status=200):
    headers = {"Authorization": "Bearer " + token, "Content-Type": "application/json"}
    if capable:
        headers["X-Memory-Capabilities"] = "okf-markdown-v1"
    req = urllib.request.Request(URL + path, headers=headers,
                                 data=None if body is None else json.dumps(body).encode())
    try:
        with urllib.request.urlopen(req, timeout=10) as res:
            assert res.status == status, res.status
            return json.load(res)
    except urllib.error.HTTPError as err:
        assert err.code == status, (err.code, err.read())
        return None


def start(log):
    proc = subprocess.Popen([str(BINARY)], cwd=FIXTURE, env=ENV, stdout=log, stderr=log)
    for _ in range(150):
        if proc.poll() is not None:
            raise AssertionError(f"gateway exited: {proc.returncode}; {FIXTURE}")
        try:
            api("/v1/memories/capabilities")
            return proc
        except (urllib.error.URLError, ConnectionError):
            time.sleep(0.1)
    proc.terminate()
    proc.wait(timeout=10)
    raise AssertionError("gateway did not start")


def stop(proc):
    proc.terminate()
    proc.wait(timeout=15)


def main():
    proc = None
    with (FIXTURE / "gateway.log").open("w") as log:
        try:
            proc = start(log)
            cap = api("/v1/memories/capabilities")
            assert cap["capabilities"][0]["id"] == "okf-markdown-v1"
            api("/v1/memories/capabilities", token="invalid", status=401)
            api("/v1/projects", {"ident": "proj"})
            api("/v1/projects", {"ident": "other"})
            route = "/v1/projects/proj/memories"
            item = json.loads((ROOT / "crates/gateway/tests/fixtures/memory-okf-v1.json").read_text())
            push = api(route + "/push", {"memories": [item]})
            result = push["results"][0]
            assert result["action"] == "created", push
            mid = result["gateway_memory_id"]
            capable = api(route + "/pull", {}, capable=True)
            assert capable["memories"][0]["okf"] == item["okf"]
            assert capable["memories"][0]["concept_hash"] == item["concept_hash"]
            # Legacy wire shape is exactly the capable projection minus optional keys.
            legacy = api(route + "/pull", {})
            expected = copy.deepcopy(capable)
            for memory in expected["memories"]:
                memory.pop("okf", None)
                memory.pop("concept_hash", None)
            assert legacy == expected
            assert "okf" not in api(route)["memories"][0]
            assert api(route, capable=True)["memories"][0]["okf"] == item["okf"]
            api(route + "/pull", {}, capable=True, token="invalid", status=401)
            assert api("/v1/projects/other/memories/pull", {}, capable=True)["memories"] == []
            foreign = {**item, "project_ident": "other", "gateway_memory_id": mid}
            assert api("/v1/projects/other/memories/push", {"memories": [foreign]})["results"][0]["action"] == "rejected"
            # Unknown extension numbers/text survive a server restart and another client.
            stop(proc)
            proc = start(log)
            assert api(route + "/pull", {}, capable=True) == capable
            second = {**item, "client_id": "second-client", "local_memory_id": "second-local"}
            assert api(route + "/push", {"memories": [second]})["results"][0]["action"] == "linked"
            changed = copy.deepcopy(item)
            changed.update(gateway_memory_id=mid, base_gateway_revision=1)
            changed["okf"]["extension_from_second_client"] = {"preserved": True}
            assert api(route + "/push", {"memories": [changed]})["results"][0]["action"] == "updated"
            assert api(route + "/pull", {}, capable=True)["memories"][0]["okf"] == changed["okf"]
            changed["okf"]["revision"] = 2
            assert api(route + "/push", {"memories": [changed]})["results"][0]["action"] == "conflict"
            invalid = copy.deepcopy(item)
            invalid["okf"]["version"] = 2
            assert api(route + "/push", {"memories": [invalid]})["results"][0]["action"] == "rejected"
            plain = {"content": "legacy record", "memory_type": "project"}
            mixed_push = api(route + "/push", {"memories": [invalid, plain]})
            assert [r["action"] for r in mixed_push["results"]] == ["rejected", "created"]
            mixed = api(route + "/pull", {}, capable=True)["memories"]
            assert len(mixed) == 2 and sum("okf" in m for m in mixed) == 1
            tombstone = {"gateway_memory_id": mid, "base_gateway_revision": 2, "tombstone": {"deleted": True}}
            assert api(route + "/push", {"memories": [tombstone]})["results"][0]["action"] == "tombstoned"
            pulled = api(route + "/pull", {}, capable=True)
            assert len(pulled["tombstones"]) == 1 and len(pulled["memories"]) == 1
            with sqlite3.connect(FIXTURE / "gateway.db") as db:
                assert db.execute("PRAGMA integrity_check").fetchone()[0] == "ok"
                assert db.execute("PRAGMA foreign_key_check").fetchall() == []
                assert json.loads(db.execute("SELECT okf_json FROM gateway_memories WHERE id=?", (mid,)).fetchone()[0]) == {**changed["okf"], "revision": 1}
            print(f"PASS: authenticated OKF/legacy HTTP, project isolation, restart, extensions, conflicts, tombstones; {FIXTURE}")
        finally:
            if proc is not None and proc.poll() is None:
                stop(proc)


if __name__ == "__main__":
    main()
