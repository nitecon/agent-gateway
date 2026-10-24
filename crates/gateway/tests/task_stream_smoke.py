#!/usr/bin/env python3
"""Isolated task-stream/legacy-migration smoke using a real gateway and RFC6455 peer.

cargo build -p gateway && python3 crates/gateway/tests/task_stream_smoke.py
Pass --hold for browser verification. All fixtures stay under target/; no real agents run.
"""
import base64
import hashlib
import hmac
import http.client
import json
import os
from pathlib import Path
import socket
import sqlite3
import struct
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parents[3]
FIXTURE = Path(tempfile.mkdtemp(prefix="task-stream-smoke-", dir=ROOT / "target"))
TOKEN = "isolated-stream-test-token"
with socket.socket() as sock:
    sock.bind(("127.0.0.1", 0))
    PORT = sock.getsockname()[1]
URL = f"http://127.0.0.1:{PORT}"


def api(path, body=None, method=None, origin=None, headers=None):
    request_headers={"Authorization": "Bearer "+TOKEN, "Content-Type":"application/json", "X-Agent-Id":"fixture-agent"}
    if origin is None and body is not None and (method is None or method == "POST") and path.endswith("/tasks"):
        # Browser-created fixtures have no submitting agent to reserve.
        request_headers.pop("X-Agent-Id", None)
    if origin:
        request_headers.update({"X-Agent-Session-Id":origin["session_id"], "X-Agent-Instance-Id":origin["instance_id"], "X-Agent-Provider":origin["provider"], "X-Agent-OS":origin["os"]})
    request_headers.update(headers or {})
    request_headers={k:v for k,v in request_headers.items() if v is not None}
    request = urllib.request.Request(URL+path, data=json.dumps(body).encode() if body is not None else None,
        method=method or ("POST" if body is not None else "GET"),
        headers=request_headers)
    with urllib.request.urlopen(request,timeout=10) as response:
        return json.load(response)


def rejected(status, path, body, method="PATCH", **kwargs):
    try: api(path, body, method, **kwargs)
    except urllib.error.HTTPError as error: assert error.code==status,(error.code,error.read())
    else: raise AssertionError("mutation unexpectedly accepted")

REQUESTER={"session_id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", "instance_id":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb", "provider":"codex", "os":"linux"}
WORKER={**REQUESTER,"session_id":"cccccccc-cccc-4ccc-8ccc-cccccccccccc", "provider":"claude", "os":"windows"}


def cookie(role):
    with sqlite3.connect(FIXTURE/"gateway.db") as conn:
        uid,epoch=conn.execute("SELECT id,session_epoch FROM users WHERE role=?",(role,)).fetchone()
    expiry=int(time.time()*1000)+300000
    signature=hmac.new(TOKEN.encode(),f"gateway-ui-session:{uid}:{expiry}:{epoch}".encode(),hashlib.sha256).hexdigest()
    return f"gw_session={uid}.{expiry}.{signature}"


def browser_status(path,role):
    req=urllib.request.Request(URL+path,headers={"Cookie":cookie(role),"X-Gateway-UI":"1"})
    try:
        with urllib.request.urlopen(req) as response:
            return response.status,response.read().decode()
    except urllib.error.HTTPError as error:
        return error.code,""


def subscribe(consumer,after=None):
    client=StreamSocket()
    client.send({"type":"subscribe","protocol_version":1,"consumer_id":consumer,"after_event_id":after})
    return client,client.receive("subscribed")


def receive_event(client,kind=None):
    event=client.receive("event")["event"]
    if kind: assert event["kind"]==kind,event
    return event


def ack(client,event,status="received",**fields):
    client.send({"type":"ack","event_id":event["id"],"status":status,**fields})
    return client.receive("recorded")


def wait_disconnected(consumer):
    for _ in range(100):
        if not next(c for c in api("/v1/task-events/consumers") if c["consumer_id"]==consumer)["connected"]:
            return
        time.sleep(.05)
    raise AssertionError("consumer did not disconnect")

class StreamSocket:
    """Minimal RFC6455 test peer using only Python's standard library."""
    def __init__(self):
        self.http = http.client.HTTPConnection("127.0.0.1", PORT, timeout=10)
        key = base64.b64encode(os.urandom(16)).decode()
        self.http.request("GET", "/v1/tasks/stream", headers={"Authorization": f"Bearer {TOKEN}", "Connection": "Upgrade", "Upgrade": "websocket", "Sec-WebSocket-Version": "13", "Sec-WebSocket-Key": key})
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
    binary=str(ROOT/"target/debug/gateway")
    env={"PATH":"/usr/bin:/bin","GATEWAY_API_KEY":TOKEN,
         "GATEWAY_HOST":"127.0.0.1","GATEWAY_PORT":str(PORT),"DATABASE_PATH":str(FIXTURE/"gateway.db"),"GATEWAY_UI_AUTH":"on"}
    for username,flags in [("fixture-admin",["--admin"]),("fixture-member",[])]:
        subprocess.run([binary,"user","add",username,*flags],env={**env,"GATEWAY_USER_PASSWORD":"fixture-password-123"},check=True,capture_output=True)
    with sqlite3.connect(FIXTURE/"gateway.db") as conn:
        conn.execute("INSERT INTO projects(ident,channel_name,room_id,created_at) VALUES ('legacy','discord','',0)")
        conn.execute("INSERT INTO execution_runs(id,project_ident,trigger,dedup_key,status,created_at,summary) VALUES ('legacy-run','legacy','task','legacy-run','running',1,'Preserved old summary')")
        for key in ['execution','execution.templates','execution.project.legacy']:
            conn.execute("INSERT OR REPLACE INTO settings(key,value) VALUES (?,?)",(key,'{"enabled":true,"on_task_received":true}'))
    (FIXTURE/".env").write_text("# Private fixture; no ancestor configuration.\n")
    env.update({"HTTP_PROXY":"http://127.0.0.1:9","HTTPS_PROXY":"http://127.0.0.1:9","NO_PROXY":"127.0.0.1,localhost"})
    log=(FIXTURE/"gateway.log").open("w")
    process=None
    def start():
        proc=subprocess.Popen([binary],cwd=FIXTURE,env=env,stdout=log,stderr=log)
        for _ in range(100):
            if proc.poll() is not None: raise AssertionError((FIXTURE/"gateway.log").read_text())
            try:
                api("/v1/task-events");return proc
            except OSError:time.sleep(.05)
        raise AssertionError("gateway did not start")
    try:
        process=start()
        old=api("/v1/execution/runs")[0]
        assert old["summary"]=="Preserved old summary" and old["status"]=="retired"
        with sqlite3.connect(FIXTURE/"gateway.db") as conn:
            assert conn.execute("SELECT count(*) FROM settings WHERE key LIKE 'execution%' ").fetchone()[0]==0
        for name in ['alpha','beta']:
            api("/v1/projects",{"ident":name})
            api("/v1/projects",{"ident":name,"repo_url":f"git@github.com:fixture/{name}.git"})
        for suffix in ['', '/chunks', '/hierarchy', '/retired-id']:
            rejected(404, '/v1/projects/alpha/api-docs'+suffix, None, 'GET')
        rejected(404, '/v1/projects/alpha/api-docs', {}, 'POST')
        for page in ['/documentation', '/api-docs', '/projects/alpha/documentation', '/projects/alpha/api-docs', '/projects/alpha/documentation/retired-id']:
            assert browser_status(page, 'admin')[0] == 404, page
        status, overview = browser_status('/projects/alpha', 'admin')
        assert status == 200 and '/documentation' not in overview and '/api-docs' not in overview
        rejected(400, '/v1/projects/alpha/artifacts', {'kind':'documentation','title':'Retired docs'}, 'POST',
            headers={'X-Agent-System':'codex','Idempotency-Key':'retired-docs'})
        # Explicit local operator recovery preserves normal API session fencing.
        orphan = api('/v1/projects/alpha/tasks', {'title':'Orphan recovery fixture','specification':'Preserved work'})
        path = '/v1/projects/alpha/tasks/'+orphan['id']
        api(path, {'status':'in_progress'}, 'PATCH', origin=WORKER)
        for status in ['todo','in_progress','done']:
            rejected(409, path, {'status':status}, origin=REQUESTER)
        def claim_info():
            result = subprocess.run([binary,'task','claim-info','alpha',orphan['id']], env=env, check=True, capture_output=True, text=True)
            return json.loads(result.stdout)
        def recover_claim(token):
            return subprocess.run([binary,'task','recover','alpha',orphan['id'],'--expected-claim',token,'--reason','Fixture owner confirmed crashed session'], env=env, capture_output=True, text=True)
        stale = claim_info()['claim_token']
        api(path, {'title':'Owner activity after inspection'}, 'PATCH', origin=WORKER)
        refused = recover_claim(stale)
        assert refused.returncode != 0 and 'task claim changed' in refused.stderr
        recovered = recover_claim(claim_info()['claim_token'])
        assert recovered.returncode == 0, recovered.stderr
        detail = api(path)
        assert detail['status']=='todo' and detail['owner_origin'] is None and detail['owner_agent_id'] is None
        assert detail['specification']=='Preserved work'
        assert any(c['author']=='gateway-admin' and c['author_type']=='system' and c['origin'] is None for c in detail['comments'])
        api(path, {'status':'in_progress'}, 'PATCH', origin=REQUESTER)
        rejected(409, path, {'status':'todo'}, origin=WORKER)
        api(path, {'status':'done'}, 'PATCH', origin=REQUESTER)
        api("/v1/projects/alpha/tasks",{"title":"Preexisting task"})
        client,hello=subscribe("fixture-cmux")
        assert hello["cursor"]==api("/v1/task-events")[0]["id"]
        client.send({"type":"heartbeat"});client.receive("heartbeat_ack")
        duplicate=StreamSocket();duplicate.send({"type":"subscribe","protocol_version":1,"consumer_id":"fixture-cmux"})
        assert 'already connected' in duplicate.receive("error")["message"];duplicate.close()
        # Human task routing through ordinary presence: no machine/session keys.
        presence=api('/v1/projects/alpha/task-targets',{'generation':str(uuid.uuid4()),'canonical_remote':'https://github.com/fixture/alpha.git','display_name':'Grace Hopper','hostname':'FixtureBox'},origin=REQUESTER)
        directory=api('/v1/projects/alpha/task-targets')
        assert directory['targets'][0]['target']==presence['target']
        assert presence['display_name']=='Grace Hopper' and directory['targets'][0]['hostname']=='FixtureBox'
        routed=api('/v1/projects/alpha/tasks',{'title':'Human selected a connected agent','canonical_remote':'github.com/fixture/alpha','routing':{'mode':'session','target':presence['target']}})
        event=receive_event(client,'task_created')
        assert event['task']['id']==routed['id'] and event['origin'] is None
        assert event['task']['routing']==routed['routing'] and event['task']['owner_origin']==REQUESTER
        assert routed['status']=='todo' and routed['started_at'] is None
        ack(client,event)
        worker_presence=api('/v1/projects/alpha/task-targets',{'generation':str(uuid.uuid4()),'canonical_remote':'github.com/fixture/alpha'},origin=WORKER)
        human={'X-Agent-Id':None}
        assigned=api('/v1/projects/alpha/tasks/'+routed['id']+'/assign',{'target':worker_presence['target']},headers=human)
        assert assigned['owner_origin']==WORKER
        event=receive_event(client,'task_commented');assert event['origin'] is None and event['comment']['author_type']=='user';ack(client,event)
        api('/v1/projects/alpha/tasks/'+routed['id'],{'title':'User edited assigned work'},'PATCH',headers=human)
        api('/v1/projects/alpha/tasks/'+routed['id']+'/comments',{'content':'User follow-up'},headers=human)
        event=receive_event(client,'task_commented');assert event['origin'] is None;ack(client,event)
        api('/v1/projects/alpha/tasks/'+routed['id'],{'status':'done'},'PATCH',headers=human)
        event=receive_event(client,'task_completed');assert event['origin'] is None and event['task']['owner_origin']==WORKER;ack(client,event)
        for mode in ['unassigned','broadcast']:
            queued=api('/v1/projects/alpha/tasks',{'title':'Desktop '+mode,'canonical_remote':'github.com/fixture/alpha','routing':{'mode':mode}})
            event=receive_event(client,'task_created')
            assert event['task']['id']==queued['id'] and event['task']['routing']=={'mode':mode}
            assert event['task']['owner_origin'] is None and event['task']['owner_agent_id'] is None
            ack(client,event)
        task=api("/v1/projects/alpha/tasks",{"title":"Ordinary task streams without policy","specification":"Fixture context"})
        event=receive_event(client,"task_created")
        assert event["task"]["id"]==task["id"] and event["canonical_remote"]=="github.com/fixture/alpha" and event["origin"] is None
        assert ack(client,event,"queued")["status"]=="queued"
        assert ack(client,event,"injected",workspace_id="workspace",surface_id="surface",message="Delivered "+TOKEN,summary="Recorded "+TOKEN)["status"]=="injected"
        assert ack(client,event,"received")["status"]=="injected"
        assert api(f"/v1/projects/alpha/tasks/{task['id']}")["status"]=="todo"
        comment=api(f"/v1/projects/alpha/tasks/{task['id']}/comments",{"author":"Human","author_type":"user","content":"Question on an ordinary task"})
        event=receive_event(client,"task_commented");assert event["comment"]["id"]==comment["id"];ack(client,event,"skipped",message="No active agent")
        api(f"/v1/projects/alpha/tasks/{task['id']}/comments",{"content":"Implemented, validation passed"})
        ack(client,receive_event(client,"task_commented"))
        api(f"/v1/projects/alpha/tasks/{task['id']}",{"status":"done"},"PATCH")
        event=receive_event(client,"task_completed");assert event["comment"]["content"]=="Implemented, validation passed";ack(client,event)
        pending=api("/v1/projects/alpha/tasks",{"title":"Reconnect before ack"})
        event=receive_event(client,"task_created");client.close();wait_disconnected("fixture-cmux")
        client,hello=subscribe("fixture-cmux")
        replay=receive_event(client,"task_created");assert replay["id"]==event["id"];ack(client,replay,"injected")
        client.close();wait_disconnected("fixture-cmux")
        process.terminate();process.wait(timeout=10)
        process=start()
        client,hello=subscribe("fixture-cmux");assert hello["cursor"]==replay["id"]
        assert api('/v1/projects/alpha/tasks/'+routed['id'])['routing']==routed['routing']
        delegation=api("/v1/projects/alpha/tasks/delegate",{"target_project_ident":"beta","title":"Incoming delegated task"},origin=REQUESTER)
        received=[receive_event(client)];ack(client,received[-1])
        for _ in range(3):
            received.append(receive_event(client));ack(client,received[-1])
        assert [e["kind"] for e in received]==['task_created','task_created','task_commented','task_commented']
        assert received[0]["task"]["id"]==delegation["target_task"]["id"]
        assert received[0]["delegation"]["source_project_ident"]=="alpha"
        assert received[1]["task"]["kind"]=="delegated"
        assert all(e["origin"]==REQUESTER for e in received)
        target_path=f"/v1/projects/beta/tasks/{delegation['target_task']['id']}"
        source_path=f"/v1/projects/alpha/tasks/{delegation['source_task']['id']}"
        assert api(target_path)["owner_origin"] == REQUESTER
        rejected(409,target_path,{"status":"in_progress"},origin=WORKER)
        machine_key=subprocess.check_output([binary,"task-machine","issue","beta",WORKER["instance_id"],WORKER["os"]],cwd=FIXTURE,env=env,text=True).strip().splitlines()[-1]
        target=api("/v1/projects/beta/task-sessions",{"generation":str(uuid.uuid4())},origin=WORKER,
                   headers={"X-Gateway-Machine-Key":machine_key,"X-Gateway-Session-Key":os.urandom(32).hex()})["target"]
        third={**REQUESTER,"session_id":str(uuid.uuid4())}
        rejected(409,target_path+"/reassign",{"target":target},method="POST",origin=third)
        moved=api(target_path+"/reassign",{"target":target},origin=REQUESTER)
        assert moved["owner_origin"] == WORKER and moved["status"] == "todo" and moved["started_at"] is None
        reassigned=receive_event(client,"task_commented");assert reassigned["origin"]==REQUESTER and reassigned["task"]["owner_origin"]==WORKER;ack(client,reassigned)
        api(source_path,{"status":"in_progress"},"PATCH",origin=REQUESTER)
        claimed=api(target_path,{"status":"in_progress"},"PATCH",origin=WORKER)
        assert claimed["owner_origin"]==WORKER
        for actor in [REQUESTER, {**WORKER,"instance_id":REQUESTER["session_id"]}, None]:
            for status in ["in_progress","todo","done"]:
                rejected(409,target_path,{"status":status},origin=actor)
            rejected(409,target_path,{"owner_agent_id":None},origin=actor)
        rejected(409,"/v1/projects/beta/tasks/reorder?status=done",{"order":[claimed["id"]]},method="POST",origin=REQUESTER)
        rejected(409,"/v1/projects/beta/tasks/reorder?status=todo",{"order":[claimed["id"]]},method="POST")
        rejected(400,target_path,{"status":"done"},headers={"X-Agent-Session-Id":WORKER["session_id"]})
        rejected(400,target_path,{"status":"done"},origin={**WORKER,"provider":"invalid"})
        rejected(400,target_path,{"status":"done"},origin={**WORKER,"session_id":"invalid"})
        released=api(target_path,{"status":"todo"},"PATCH",origin=WORKER)
        assert released["owner_origin"] is None and released["owner_agent_id"] is None
        ack(client,receive_event(client,"task_unclaimed"))
        api(target_path,{"status":"in_progress"},"PATCH",origin=WORKER)
        comment=api(target_path+"/comments",{"content":"A different session commented last"},origin=REQUESTER)
        event=receive_event(client,"task_commented")
        assert event["origin"]==REQUESTER and event["comment"]["origin"]==REQUESTER and comment["origin"]==REQUESTER
        ack(client,event)
        assert api(target_path)["comments"][-1]["origin"]==REQUESTER
        # Restart must preserve both ownership and event/comment provenance.
        client.close();wait_disconnected("fixture-cmux")
        process.terminate();process.wait(timeout=10);process=start()
        client,hello=subscribe("fixture-cmux")
        assert api(target_path)["owner_origin"]==WORKER
        rejected(409,target_path,{"status":"done"},origin=REQUESTER)
        child=api(f"/v1/projects/alpha/tasks/{pending['id']}/subtasks",{"title":"Generated child"},origin=WORKER)
        event=receive_event(client,"task_created");assert event["task"]["id"]==child["id"] and event["origin"]==WORKER;ack(client,event)
        # A failed source mirror must roll back target completion and all events.
        before_tail=api("/v1/task-events")[0]["id"]
        with sqlite3.connect(FIXTURE/"gateway.db") as conn:
            conn.execute(f"CREATE TRIGGER fixture_fail_mirror BEFORE UPDATE OF status ON tasks WHEN NEW.id='{delegation['source_task']['id']}' AND NEW.status='done' BEGIN SELECT RAISE(ABORT,'fixture mirror failure'); END")
        rejected(500,target_path,{"status":"done"},origin=WORKER)
        assert api(target_path)["status"]=="in_progress" and api(source_path)["status"]=="in_progress"
        assert api("/v1/task-events")[0]["id"]==before_tail
        with sqlite3.connect(FIXTURE/"gateway.db") as conn:
            assert conn.execute("SELECT origin_json FROM task_mutation_context").fetchone()[0] is None
            conn.execute("DROP TRIGGER fixture_fail_mirror")
        api(target_path,{"status":"done"},"PATCH",origin=WORKER)
        completed=[]
        for _ in range(3):
            event=receive_event(client);completed.append(event);ack(client,event)
        assert [e["kind"] for e in completed]==['task_completed','task_commented','task_completed']
        assert all(e["origin"]==WORKER for e in completed)
        assert completed[0]["comment"]["origin"]==REQUESTER
        assert completed[1]["comment"]["author"]=="agent-gateway" and completed[1]["comment"]["origin"]==WORKER
        assert api(source_path)["status"]=="done" and api(source_path)["comments"][-1]["origin"]==WORKER
        large=api("/v1/projects/alpha/tasks",{"title":"<script>fixture</script>","specification":("🦀\n"+TOKEN)*10000})
        event=receive_event(client,"task_created");assert event["truncated"] and TOKEN not in json.dumps(event);ack(client,event)
        # Only the maintenance timer can release this assigned TODO: no task reads drive it.
        expiring=api("/v1/projects/alpha/tasks",{"title":"Idle reserved task"},origin=WORKER)
        event=receive_event(client,"task_created");assert event["task"]["owner_origin"]==WORKER;ack(client,event)
        with sqlite3.connect(FIXTURE/"gateway.db") as conn:
            conn.execute("UPDATE tasks SET updated_at=? WHERE id=?",(int(time.time()*1000)-15*60*1000-1,expiring["id"]))
        client.sock.settimeout(40)
        audit=receive_event(client,"task_commented");assert audit["task"]["id"]==expiring["id"] and "15 minutes" in audit["comment"]["content"];ack(client,audit)
        expired=receive_event(client,"task_unclaimed")
        assert expired["task"]["id"]==expiring["id"] and expired["task"]["owner_origin"] is None and expired["task"]["owner_agent_id"] is None
        assert expired["task"]["status"]=="todo" and expired["origin"] is None
        ack(client,expired)
        api(f"/v1/projects/alpha/tasks/{expiring['id']}",{"status":"in_progress"},"PATCH",origin=REQUESTER)
        rows=api("/v1/task-events")
        assert TOKEN not in json.dumps(rows)
        for path in ['/v1/execution/settings','/v1/execution/templates','/v1/execution/clients','/v1/projects/alpha/execution','/v1/execution/connect','/v1/execution/sessions']:
            try:api(path)
            except urllib.error.HTTPError as error:assert error.code==404,(path,error.code)
            else:raise AssertionError('retired route remains: '+path)
        for path in ['/task-stream','/execution','/v1/task-events','/v1/task-events/consumers','/v1/execution/runs']:
            assert browser_status(path,'admin')[0]==200,path
            assert browser_status(path,'member')[0]==403,path
        html=browser_status('/task-stream','admin')[1]
        assert 'execution-form' not in html and 'API_KEY' not in html and TOKEN not in html
        native=http.client.HTTPConnection('127.0.0.1',PORT,timeout=5)
        native.request('GET','/v1/tasks/stream',headers={'Cookie':cookie('admin'),'X-Gateway-UI':'1','Connection':'Upgrade','Upgrade':'websocket','Sec-WebSocket-Version':'13','Sec-WebSocket-Key':base64.b64encode(os.urandom(16)).decode()})
        assert native.getresponse().status==401;native.close()
        assert len(api('/v1/execution/runs'))==1
        client.close()
        print('Task-stream smoke passed:',URL,'fixture:',FIXTURE,flush=True)
        if '--hold' in sys.argv:
            print('Browser fixture login: fixture-admin / fixture-password-123 (member: fixture-member)',flush=True)
            while True:time.sleep(1)
    finally:
        if process and process.poll() is None:
            process.terminate();process.wait(timeout=10)
        log.close()


if __name__=='__main__':
    main()
