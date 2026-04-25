# Agent Tools Pattern Library Requirements

## Purpose

`agent-tools` should expose the gateway global pattern library as a first-class
CLI surface. Patterns are organization-wide markdown documents that describe how
we do things. They are not project-local tasks, and they are not memory entries.

## Gateway API

All endpoints require the same bearer token used by the existing gateway API.

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/v1/patterns?q=<query>` | List or search pattern summaries. |
| `POST` | `/v1/patterns` | Create a pattern. |
| `GET` | `/v1/patterns/:id` | Fetch one pattern by id or slug, without comments. |
| `PATCH` | `/v1/patterns/:id` | Update pattern metadata or markdown body. |
| `DELETE` | `/v1/patterns/:id` | Delete a pattern. |
| `GET` | `/v1/patterns/:id/comments` | Fetch comments for one pattern. |
| `POST` | `/v1/patterns/:id/comments` | Add a comment to one pattern. |

Pattern create body:

```json
{
  "title": "Deploying Eventic Applications",
  "slug": "deploying-eventic-applications",
  "summary": "How we use main and tag deploys for independent sites.",
  "body": "# Deploying Eventic Applications\n\n...",
  "labels": ["eventic", "deploy"],
  "author": "agent-id"
}
```

Pattern response shape:

```json
{
  "id": "uuid-v7",
  "title": "Deploying Eventic Applications",
  "slug": "deploying-eventic-applications",
  "summary": "How we use main and tag deploys for independent sites.",
  "body": "# Deploying Eventic Applications\n\n...",
  "labels": ["eventic", "deploy"],
  "author": "agent-id",
  "created_at": 1777130000000,
  "updated_at": 1777130000000
}
```

List/search response shape is an array of summaries. Summaries omit `body` and
include `comment_count`.

Comments are intentionally not included in `GET /v1/patterns/:id`. Agents should
only fetch comments when the user explicitly asks to address or review comments.

## CLI Surface

Recommended commands:

```bash
agent-tools patterns list
agent-tools patterns search "<query>"
agent-tools patterns get <id-or-slug>
agent-tools patterns create --title "..." [--slug "..."] [--label x] [--summary "..."] --body-file path.md
agent-tools patterns update <id-or-slug> [--title "..."] [--slug "..."] [--label x] [--summary "..."] [--body-file path.md]
agent-tools patterns delete <id-or-slug>
agent-tools patterns comments <id-or-slug>
agent-tools patterns comment <id-or-slug> "<markdown comment>"
```

`get` must print only the pattern document and metadata. It must not fetch or
display comments.

`comments` should call `GET /v1/patterns/:id/comments` and print the thread.

`comment` should call `POST /v1/patterns/:id/comments` with:

```json
{
  "content": "...",
  "author": "<agent id>",
  "author_type": "agent"
}
```

## Agent Behavior

Agents should use patterns as durable global guidance. They should search the
pattern library when the task appears to involve an established organizational
practice, such as deployment, encryption, secrets handling, project setup,
frontend conventions, release workflows, or incident response.

Agents should not treat pattern comments as part of the normal guidance pull.
Comments are review/collaboration material and should be fetched only when the
user says comments exist or asks to address them.

