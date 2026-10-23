# Task cancellation

Cancellation is an explicit, recorded report. It never deletes a task, never
uses the generic `done` status, and never proves that an executor stopped. The
gateway cannot observe or terminate client processes.

## Status

`cancelled` is a terminal task status alongside `todo`, `in_progress` and
`done`. A cancelled task keeps its row, comments, `owner_agent_id`,
`owner_origin` and `started_at` as prior-claim provenance; `done_at` stays
null. PATCH status changes, reorder and delegation status mirroring on a
cancelled task return 409. `GET /v1/projects/{ident}/tasks?status=cancelled`
lists cancelled tasks. Each cancellation emits a `task_cancelled` lifecycle
event whose `origin` is the reporting session; no `task_completed` event fires.

## Ordinary tasks

`POST /v1/projects/{ident}/tasks/{id}/cancellation`

Requires the exact `x-agent-session-id`, `x-agent-instance-id`,
`x-agent-provider` and `x-agent-os` headers. These headers are provenance
supplied by a holder of the shared bearer, not machine authentication.

```json
{"reason": "no longer needed"}
```

`reason` is trimmed and must be 1..=16384 bytes. Unknown fields are rejected.

200 response:

```json
{
  "task": { "...": "existing Task DTO", "status": "cancelled", "done_at": null },
  "cancellation": {
    "task_id": "...", "project_ident": "...", "reason": "no longer needed",
    "prior_status": "in_progress", "prior_owner_agent_id": "...",
    "prior_owner_origin": {"session_id": "...", "instance_id": "...", "provider": "claude", "os": "linux"},
    "prior_started_at": 1760000000000,
    "reporter_origin": {"session_id": "...", "instance_id": "...", "provider": "claude", "os": "linux"},
    "cancelled_at": 1760000000000,
    "executor_stop_confirmed": false
  }
}
```

| Task state | Who may cancel | Result |
| --- | --- | --- |
| `todo`, unowned | Any caller with exact origin headers (same set that may claim) | 200 |
| Owned with bound `owner_origin` | Only the exact owner session + instance | 200; others 409 |
| Owned without bound origin (legacy) | Nobody | 409 |
| `done` | Nobody | 409 |
| Dispatch-managed | Use the dispatch `cancel` action | 409 |
| `cancelled` | Same `reporter_origin` and same reason | 200 replay of the original record; otherwise 409 |

Other errors: 400 missing/invalid origin headers or reason, 422 unknown
field, 404 task not in project.

`GET /v1/projects/{ident}/tasks/{id}/cancellation` returns the `cancellation`
object, or 404 when the task was not cancelled through this endpoint.

## Dispatch-managed tasks

Credential-bound dispatch tasks use `request_cancel` (records a request; task
stays in progress) and `cancel` (terminal) from
`docs/session-task-dispatch.md`. A dispatch `cancel` sets task status
`cancelled`; its reason, requester/destination authentication and prior claim
history live in the dispatch document and `task_dispatch_events`, not in the
`cancellation` record above, so the GET endpoint returns 404 for them.

## Unsupported

- No executor stop proof or forced stop. `executor_stop_confirmed` is always
  false; a dispatch executor's own `cancel` with its execution ID is its report.
- No reopening a cancelled task, and no cancellation of `done` tasks.
- No operator or non-owner forced cancellation of an owned task; use the
  existing local claim recovery first.
- No cascade between a delegated source task and its target task.
- The control-panel UI does not yet show a cancelled column.
- Not deployed by this change; deployment is separate operator work.
