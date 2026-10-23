# Session-targeted task dispatch

This is the opt-in server contract for amux M2. It supports multiple native
sessions, including three Codex sessions on one machine in one project. It does
not deploy the server, provision live credentials, or change existing clients.

## Identity and authentication

All endpoints below require the normal `Authorization: Bearer <gateway-key>`
plus these headers:

| Header | Meaning |
| --- | --- |
| `X-Agent-Session-Id` | Native actor session UUID, normalized by the gateway |
| `X-Agent-Instance-Id` | Provisioned machine instance UUID |
| `X-Agent-Provider` | Declared provider: `codex` or `claude` |
| `X-Agent-OS` | Provisioned OS: `linux`, `windows`, or `macos` |
| `X-Gateway-Machine-Key` | Project-scoped machine secret |
| `X-Gateway-Session-Key` | Independent session/generation secret; required for registration, mutations, and recovery |

Session keys must be 32 cryptographically random bytes encoded as 64 hex
characters. Each executor generation generates its own key and UUID, persists
both before registration, and supplies the UUID in `generation`. A target is:

```json
{
  "origin": {
    "session_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
    "instance_id": "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
    "provider": "codex",
    "os": "linux"
  },
  "generation": "cccccccc-cccc-4ccc-8ccc-cccccccccccc"
}
```

Provider, host labels, project labels, and `X-Agent-Id` do not establish an
owner. Exact target matching uses all origin fields and the generation. A
registration's origin and secret are immutable within its generation. A new
process may register a new generation for the same native conversation; that
does not transfer any existing claims. A peer with the same machine credential
cannot act as an already registered generation without that generation's secret.

The server validates the machine secret against a provisioned
`(project_ident, instance_id, os)` record and checks the separate session secret.
It stores SHA-256 hashes of these high-entropy secrets. Mutations return
`authentication.machine` from that server record, including `credential_id`
(the machine key's hash), and the authenticated session and generation. Durable
history records the same authentication snapshot. This is credential-based
machine identity, not hardware attestation. Provider and native conversation
identity remain declarations made by an authorized machine; the gateway cannot
independently attest that a process is Codex or Claude.

The local operator commands are:

```text
gateway task-machine issue <project> <instance-uuid> <linux|windows|macos>
gateway task-machine revoke <project> <instance-uuid>
```

`issue` prints a newly generated secret once. Reissuing invalidates the previous
machine key and expires the machine's leases while retaining session-secret
fences. Revocation rejects further requests and removes the machine from the
eligible directory. These commands require local database access; HTTP clients
cannot self-provision machines or authorize themselves for another project.
No live provisioning was performed for this implementation.

## HTTP operations

Paths below have prefix `/v1/projects/{ident}`. JSON bodies reject unknown fields.
Successful operations return HTTP 200. Missing machine/session secrets return
401, invalid credential bindings return 403, and ownership, eligibility,
generation, state, or request-ID conflicts return 409. Malformed header UUIDs
and registration generation UUIDs return 400; malformed JSON uses Axum's 4xx
rejection. Semantic mutation validation errors, including invalid request IDs,
return 409. Internal database failures return 500 and roll back the mutation.

| Method and path | Body / response |
| --- | --- |
| `POST /task-sessions` | `{ "generation": "<uuid>" }`; register or heartbeat the exact generation using its secret. Returns `{target, seen_at, expires_at}`. |
| `GET /task-sessions` | Machine credential required. Returns `{sessions:[{target,seen_at,expires_at}]}` for eligible sessions in this project. |
| `POST /task-dispatches` | Delegate body below; returns `{dispatch, task, authentication}`. |
| `POST /task-dispatches/{task_id}/actions` | Action body below; returns `{dispatch, task, authentication}`. |
| `GET /task-dispatches/recovery?generation=<uuid>` | Authenticated session required. Returns `{history_complete, pending, machine_pending, machine_dispatches, prior_claims, requested}`. |

Registrations have a 120-second availability lease. Heartbeat the same
registration before expiry. Delegation, receipt, new claims, start, handoff,
outcome, and cancellation require a live caller. Exact-session delegation and
handoff require a live eligible destination. Machine-addressed delegation and
handoff require a provisioned eligible machine but no destination session or
availability lease. Expiring a lease never releases a task or
proves that an executor has stopped. A successful request can be replayed after
lease expiry, provided its credentials and immutable registration remain valid.

The delegate body is:

```json
{
  "request_id": "11111111-1111-4111-8111-111111111111",
  "generation": "22222222-2222-4222-8222-222222222222",
  "target_project_ident": "agent-gateway",
  "target": { "origin": { "session_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", "instance_id": "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb", "provider": "codex", "os": "linux" }, "generation": "cccccccc-cccc-4ccc-8ccc-cccccccccccc" },
  "title": "Implement the assigned server contract",
  "specification": "Concrete authorized work and acceptance criteria",
  "required_os": "linux",
  "required_provider": "codex"
}
```

`generation` identifies the requester. `required_os` and `required_provider` are
optional, nullable exact constraints checked against the registered target.
The destination can be the same project or another project with a separately
provisioned registered target. Acceptance creates one TODO task and one pending
dispatch atomically. It does not claim the task, acknowledge receipt, start an
executor, or send a project notification. Titles are limited to 512 UTF-8 bytes;
specifications to 65,536 bytes.

The action body has required `request_id`, `generation`, and `operation`, plus
optional nullable `claim_id`, `execution_id`, `result`, `target`, and
`machine_target` (the latter two are alternatives for handoff). The returned
dispatch contains:

```text
task_id, project_ident, source_project_ident, requester: Target, target: Target|null,
machine_target?: {instance_id, os}, required_os?, required_provider?, state, claim_id?, received_at?, claimed_at?,
started_at?, finished_at?, execution_id?, cancel_requested_at?, cancel_reason?, result?
```

Timestamps are server Unix milliseconds. `task` uses the existing flat Task DTO;
use `dispatch.state` to distinguish failure from success. A confirmed `cancel`
sets task status `cancelled` (never `done`); see `docs/task-cancellation.md`.
For example, after receipt, claim with
`{"request_id":"<new uuid>","generation":"<target generation>","operation":"claim"}`.
Persist the returned `dispatch.claim_id` before taking further action.

## Offline machine delivery

A sender can queue work while the destination machine is off. In the delegate
body, replace `target` with:

```json
{
  "machine_target": {"instance_id": "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"},
  "required_os": "windows",
  "required_provider": "codex"
}
```

Supply exactly one non-null `target` or `machine_target`. Machine eligibility
comes from the destination project's non-revoked provisioned machine row;
the server returns its OS in `dispatch.machine_target`. Unknown machines,
revoked credentials, OS mismatches, and unknown provider constraints return 409.
Neither client headers nor a prior session registration grant eligibility to
a different destination machine. No new provisioning is exposed through HTTP.

Acceptance durably stores the canonical TODO task and a dispatch with
`state: "pending_machine"`, `target: null`, and null receipt/claim/start fields.
It creates no session, owner, execution, reminder, or project notification.
No destination process UUID or generation is needed. The same request-ID and
SQLite transaction guarantees as exact-session dispatch apply across restarts.

On reconnect, an authenticated registered session reads `machine_pending` from
the existing recovery endpoint. This read changes no state. To select an item,
it explicitly sends `operation: "receive"` to the existing action endpoint.
Only a live session authenticated for that destination machine and provisioned
OS, matching any provider constraint, can receive it. Receipt atomically binds
`dispatch.target` to that exact session/generation and changes state to
`received`. Competing sessions receive 409; the successful receiver's identical
request-ID retry replays its original response. Receipt still leaves the task
TODO and unowned. Claim and start require their separate existing operations.
Once bound, all exact-session/generation/claim fences apply. `machine_target`
remains as delivery-address metadata, not permission for another session to
adopt the bound work.

The exact current owner may also use `operation: "handoff"` with
`machine_target` instead of `target`, its current claim ID when active, and an
explicit reason in `result`. As with existing handoff, the executor must already
be stopped. The old claim/execution snapshot is recorded before returning the
same task to `pending_machine`/TODO and clearing current ownership. A future
receiver receives a fresh claim ID only after explicitly claiming. A restarted
generation can inspect prior claims but cannot silently transfer or restart
them. A pending machine item may be cancelled by its exact requester; a
destination session first receives/binds it before acting as its owner.

Machine keys remain revocable. Receipt revalidates current credentials and the
stored destination OS; changing a machine's provisioned OS does not silently
retarget existing work. If a receiver disappears after binding, another session
cannot receive the item again. An explicit owner handoff or separately authorized
intervention remains necessary. This queue adds durable addressing, not dead-owner
takeover or automatic execution.

## State transitions and ownership

| Operation | Required state / authority | Result |
| --- | --- | --- |
| `receive` | Pending exact destination, or pending-machine eligible receiver | Records explicit receipt; leaves task TODO and unowned. |
| `claim` | Received, exact eligible destination | Generates a fresh `claim_id`; task becomes in progress with exact `owner_origin`. `started_at` remains null. |
| `start` | Claimed, exact destination and claim; no cancellation requested | Requires executor's `execution_id` UUID. Records running and `started_at`. |
| `succeeded` / `failed` | Running, exact destination, claim, and execution | Requires a nonempty result. Records the explicit terminal outcome and marks the task done. |
| `request_cancel` | Claimed/running, exact requester or destination, current claim | Requires a reason in `result`; records the request without finishing the task or asserting that the executor stopped. |
| `cancel` | Pending/received: requester or destination. Claimed/running: exact destination and current claim. | Requires a reason. Running cancellation also requires the exact execution ID as the executor's explicit confirmation. Records dispatch `cancelled` and task status `cancelled` with `done_at` null; owner, `owner_origin`, `started_at`, any `cancel_requested_at`/`cancel_reason` and prior event history are kept. Emits `task_cancelled`, not `task_completed`. |
| `handoff` | Exact destination, nonterminal, no cancellation requested; current claim if claimed/running | Requires a different live eligible `target` or a provisioned `machine_target`, plus a reason. Preserves the prior claim/execution event, returns the task to TODO and the new destination to pending. |

Every mutation, task transition, authentication snapshot, history event, and
request replay record commits in one SQLite transaction under the gateway DB
mutex. Concurrent distinct claims yield one success and one 409. Concurrent
identical requests replay the same accepted response and claim ID. All active
mutations require the current claim ID, including owner handoff, so a previous
claim cannot affect a replacement claim.

A running executor must stop before its owner reports a handoff or confirmed
cancellation. The server records the authenticated report; it cannot observe
or terminate a client process. `start` must be reported by the executor after
confirming launch. A claim, notification, terminal input, or websocket delivery
acknowledgement is never an executor-start report.

## Request deduplication and recovery

Clients persist a fresh UUID request ID and the complete body before sending
any delegation/action. Reuse that ID with the identical request after a lost
response. Uniqueness is scoped to authenticated project, instance, and native
session, across operations and tasks. The fingerprint includes the generation,
complete typed input, actor, operation and task where applicable. Reusing an ID
with different input returns 409. Accepted responses are durable snapshots;
retrying `claim` after completion returns its original claim response, not the
current task state and not a new claim. No external execution is rerun by replay.
Rejected transactions leave no task, event, or accepted-request record.

Recovery is read-only. A fully successful response includes
`"history_complete": true`: its `prior_claims` includes all durable matching
claim/start/outcome/cancel and prior-handoff rows for this authenticated
machine's own history plus the relevant currently queued/assigned canonical
tasks in the same project and provisioned OS, as scoped below. Original
claimant/authentication/execution and unknown outcomes remain unchanged. The
marker is emitted only after all response queries and decoding succeed;
authentication, query, and other errors provide no completeness proof. An older
response with an absent marker, or a response with `false`, must not be treated
as complete, even if its history array is empty. This is a server history scope
guarantee, not proof of native execution, permission to transfer ownership, or
consent to input, claim, or start work.


- `pending` contains pending/received dispatches for the exact caller and
  generation, including work accepted while it was disconnected.
- `machine_pending` contains unbound `pending_machine` items for this
  authenticated machine and provisioned OS. Reading the array does not bind,
  receive, claim, or start them.
- `machine_dispatches` includes current dispatch documents targeted at this
  authenticated machine across its sessions and generations, including unbound
  machine-addressed items.
- `prior_claims` contains durable claim/start/outcome/cancel and prior-handoff
  snapshots from this destination project for this authenticated machine's own
  history, plus earlier machines' lineage for canonical tasks currently queued
  or assigned to this machine and provisioned OS. It does not expose other
  machines' unrelated tasks or other projects. Each entry includes monotonic
  `id`, `operation`, `dispatch`, `created_at`, and the original stored
  `authentication` snapshot. A new receiver can inspect the old claimant,
  machine, claim ID, execution ID and unknown outcome before receiving or
  claiming. Reading lineage does not bind work or transfer prior ownership.
- `requested` contains current dispatches created by this exact requester and
  generation through the source project, including cross-project work. It lets
  the requester inspect the current claim before requesting cancellation.

A new generation can inspect prior machine claims but cannot claim, start,
complete, cancel, or hand off the previous generation's work. It must not infer
that an accepted request was launched or rerun it automatically. Resume requires
the original generation's persisted credentials, or deliberate handoff by the
exact current owner. A dead owner with lost generation credentials remains
fenced: this protocol intentionally provides no automatic takeover or operator
override of a running dispatch. Choosing a broader orphan-transfer policy is
separate work requiring an explicit executor-stop/reconciliation policy.

## Legacy contract and integration boundaries

Existing `/tasks`, `/tasks/delegate`, task streaming, and shared-bearer clients
retain their response shapes. Legacy `/tasks/delegate` remains cross-project
and has no exact-session target; clients needing these guarantees must use
`/task-dispatches`. Ordinary task claim/status/owner and goal mutations and
status-changing reorders now fail closed on legacy/unbound owned claims, even
when the machine label matches. Existing local operator recovery can release
an inspected legacy in-progress claim with a reason. Legacy claim timestamps
still retain their historical claim-time meaning; explicit executor-start
semantics apply to dispatch-managed tasks.

Dispatch-managed tasks reject ordinary PATCH, reorder, DELETE, and legacy
operator release; automatic one-hour stale reclaim excludes them. Comments and
ordinary reads do not constitute receipt or execution. Legacy APIs are not
machine-authenticated by the new protocol, and client-supplied origin headers
alone do not prove a machine. The gateway's existing public bearer-authenticated
event/history read surfaces remain available; session secrets protect mutation
authority, not confidentiality between authorized project agents.

This server implementation does not deliver prompts, schedule models, launch
processes, prove exactly-once external execution, or attest native provider
identity. amux still needs to consume this API, persist its request IDs and
per-generation secrets, reconcile recovered state, and report explicit executor
events. Authorized operators must deploy a reviewed revision and provision
project-scoped machine credentials separately before cross-machine acceptance.
Local fixtures are evidence about this source revision, not a deployed server.

## Validation

Repository-local HTTP/SQLite fixtures cover three sessions sharing one
provider/host/project, exact credentials and targets, same/cross-project routing,
platform eligibility, lease expiry, revocation, request replay/conflict,
transaction rollback, concurrent claims, receipt versus start, explicit outcomes,
cancellation request versus confirmation, handoff, restart fencing, and durable
accepted-request/prior-claim recovery. They do not launch executors, send external
notifications, provision live secrets, or rerun real tasks.

Use the repository checks: `cargo fmt --all -- --check`,
`cargo clippy --workspace --all-targets -- -D warnings`, and
`cargo test --workspace`.

The offline machine extension is covered by native router/SQLite tests, including
file-database reopen, three-session receipt races, exact-owner conflicts,
request replay, provider/OS eligibility, cancellation, and prior-claim history.
A real loopback HTTP/process-restart smoke runs without executors:

```bash
cargo build -p gateway --no-default-features
python3 crates/gateway/tests/offline_machine_smoke.py
```

The smoke provisions only its private temporary SQLite fixture under `target/`,
starts and stops only its own server processes, and never connects a provider.
Use `--binary` to select a separately built source-pinned executable.
