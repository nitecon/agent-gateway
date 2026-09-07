# Gateway UI Redesign Plan

Status: implemented on branch `ui-redesign` (2026-09-07). Companion to
`gateway-features.md`, which owns the artifact substrate. This document owns
the human-facing control panel.

## Implementation Status

Shipped (see the commit series on `ui-redesign`):

- Phase 0: per-user login for pages (`/login`; first visit bootstraps an
  administrator with the API key; argon2 password hashes; sessions bound to
  the user's session epoch; `gateway user add|reset-password|list` for
  recovery; admin user management in Settings; browser actions attributed to
  the signed-in user), API key no longer embedded in pages, "unanswered" derived from agent
  confirmations, Discord author classification (`author_kind`), retention
  that actually purges. API key rotation was declined by the owner.
- Phase 1: Eventic/build removed; `repo_url` on registration derives the
  repository mapping; `kind` (repo/adhoc), `archived_at`, `canonical_remote`;
  adhoc projects create no channel room until first send; archive/restore.
  agent-tools sends `canonical_ident` and re-registers once (marker v2).
- Phase 2: askama shell (Work / Library / Gateway sidebar, breadcrumb,
  project tab strip), Home inbox, `/projects` registry, project overview and
  settings, gateway settings, message resolve/reopen/bulk, project links.
- Phase 3: `/projects/:ident/inbox` with thread list, thread pane, composer
  posting to the channel, alerts filter, bulk resolve; thread list and
  thread APIs; `agent-tools comms list|resolve`.
- Phase 4: `/activity`, cross-project `/tasks` list, canonical task detail
  route with `/task-link` redirect, artifact section anchors.
- Phase 5 (partial): live refresh on Home, Activity, Inbox (page diff +
  reload, opt-in per page), keyboard navigation (`g h/a/p/t/s`, `j/k`,
  Enter, `r`, `e`, `?`), empty states with CLI hints.

Deferred, in priority order:

1. Server-Sent Events instead of polling for live pages.
2. Moving the remaining `format!`-rendered pages (documentation, memories,
   artifacts, patterns, skills/commands/agents, task board) out of
   `routes.rs` into `ui/` templates. They already render inside the new shell.
3. Command palette for project switching.
4. Reclassifying pre-existing Discord rows as bot/webhook (the Discord author
   id was never stored; new rows are classified on ingest).

## Summary

The gateway is a human-to-agent operations console. The current UI is a set of
per-resource inventory pages: it shows counts of everything and lets the user
act on almost nothing. The redesign reorganizes the UI around three questions
a human actually asks:

1. What needs me right now? (Inbox)
2. What are my agents doing, and where are they stuck? (Activity, Tasks)
3. What have they produced or decided? (Artifacts, Memories, Patterns)

Every number on screen must be a link to a list, and every list item must
carry the actions a human can take on it. Data that fails that test is removed.

The plan also fixes four defects the survey surfaced that make the current UI
untrustworthy regardless of styling. They are listed first because they gate
everything else.

## Findings From The Survey

Evidence comes from the live site, the source at HEAD, and a read-only query of
the production database.

### F1. The API key is served to anonymous visitors (critical)

Every HTML page is registered on the unauthenticated router
(`crates/gateway/src/main.rs:646`, comment: "local admin pages, no auth
required"). Every page then embeds `state.api_key` verbatim into an inline
script via `control_panel_close` (`crates/gateway/src/routes.rs:10442`) so the
ndesign runtime can call the bearer-protected `/v1` API. The site is public at
`gateway.nitecon.org`. Anyone who loads `/settings` gets the bearer token and
full read/write access to tasks, memories, messages, skills, and settings.

Action before anything else: rotate `GATEWAY_API_KEY`, and put the HTML pages
behind authentication (Phase 0).

### F2. "Unanswered" can never go to zero

The dashboard counts user messages where `messages.confirmed_at IS NULL`
(`db.rs:7126`). Nothing sets that column for user messages. The live confirm
path `confirm_message_for_agent` (`db.rs:5732`) writes only to
`agent_confirmations`. Production numbers:

| Metric | Value |
| --- | --- |
| user-sourced messages | 2649 |
| shown as unanswered | 2649 (100%) |
| already confirmed by an agent | 2626 (99.1%) |
| sre project shown as unanswered | 2593 |

The agents have done the work. The badge reads the wrong table. Reply and
action (`routes.rs:10565`, `:10678`) also never mark the parent as answered,
so even a correct predicate would need a definition of "answered".

The same column gates retention: `purge_old_messages` (`db.rs:5792`) deletes
only rows with `confirmed_at` set, so user and system messages are never purged.
3990 rows past the 30-day cutoff are stranded and the database is 83 MB.

The sre channel content is Prometheus/Alertmanager output. The Discord ingest
filter `is_gateway_authored` (`channels/discord.rs:61`) excludes only the
gateway's own bot, so every other bot and webhook is ingested as a human.

### F3. Project identity is polluted and mapping is guessed

Projects are created by `POST /v1/projects` from whatever short ident the
client sends. `sanitize_ident` (`projects.rs:16`) takes the last path segment
and discards the provider and namespace it just parsed. Each new ident also
creates a real Discord channel (`channels/discord.rs:216`). Result in
production: 84 projects, 66 unmapped, including `tmp`, `repo`, `documents`,
`src-tauri`, `x-cleanup-preflight-mjwphr`, `ok-we-were-working-on-adding`, and
vendored third-party checkouts like `amazon-eks-ami` and `etcd`.

"Fill unmapped legacy projects" (`db.rs:5582`) assumes `repo_name == ident`
and fabricated mappings such as `nitecon/src-tauri`. The per-project Settings
form silently wipes a mapping to NULL when namespace is left blank
(`db.rs:5552`). The dashboard subtitle renders `repo_full_name` when mapped
and the literal string `discord` otherwise (`routes.rs:6473`), which reads as
two different "sources".

agent-tools already knows the answer. `agent-tools comms whoami` reports
`canonical_ident: github.com/nitecon/agent-gateway.git`, but
`register_project` in `agent-comms/src/gateway.rs:214` sends only the short
ident. The fix is a one-field protocol addition, not a new discovery system.

### F4. Build/Eventic is dead weight and a latency hazard

Eventic state is one JSON blob under the `settings` key `eventic.servers`
pointing at `https://build.nitecon.org`, which no longer resolves. There are no
tables or migrations. `settings_page` and `project_build_page` make blocking
outbound HTTP with no timeout on every render (`routes.rs:3947`). Removal is
about 420 deletable lines plus about 85 edited lines (inventory in Phase 1).

### F5. Structural debt in the rendering layer

- All HTML is `format!` strings inside `routes.rs` (13,015 lines), in four
  islands interleaved with API handlers. About 3,600 lines are markup.
- No template engine, no static assets, no project-owned JS beyond two inline
  blocks. Everything interactive is ndesign `data-nd-*` attributes.
- The dashboard is seven stat cards plus one 84-row table, server-rendered
  once, no filtering, no live refresh.
- `/artifacts` exists but is not in the sidebar. Several T012 deliverables
  (version diff, review round, spec manifest as named views) are folded into
  the detail page; the T012 task file still says `todo` while the manifest says
  `done`.
- The tasks board and Settings send ndesign form-serializer payloads that the
  API handlers special-case (`routes.rs:5232`, `:5464`, `:12628`).

## Design Principles

1. **Needs-you first.** The home screen is an inbox, not a census. Anything
   that does not require a human decision is one click away, not on the home
   screen.
2. **Every count is a door.** No number without a link to the list behind it.
   No list row without its actions.
3. **Agent activity is a first-class view.** Which agents are active, on which
   project, on which task, with what last message. Humans supervise by
   watching this, not by reading memory tables.
4. **Project workspace, not resource silos.** Navigation is project → tabs,
   not resource → project picker. Global resources (patterns, skills, agents,
   commands) live in a Library section.
5. **Dense, operational, predictable.** Same constraints as
   `gateway-features_spec/frontend/T012-artifact-workspace-ui.md`: no hero
   treatments, no nested cards, scannable tables, keyboard reachable.
6. **Server-rendered Rust stays.** The gateway remains a single binary. The
   redesign changes how markup is organized and how the browser authenticates,
   not the deployment model.
7. **Bring your own CI.** The gateway links to external systems. It does not
   integrate with them.

## Target Information Architecture

```
Home (Inbox)                  what needs a human, across all projects
Activity                      agents online, current tasks, recent events
Projects                      registry: active / archived, health, quick links
  └ {project}
      Overview                open questions, in-progress tasks, recent artifacts, links
      Inbox                   conversation thread for this project, reply / resolve
      Tasks                   board (existing, polished) + list view + task detail route
      Artifacts               docs, specs, design reviews (existing T012 surface)
      Memories                existing, with search first
      Settings                identity, repository, channel, archive
Library
      Patterns                existing
      Skills                  existing
      Commands                existing
      Agents                  existing (agent definitions)
Settings                      gateway: auth, channels, retention, appearance
```

Sidebar shows the six top-level entries. When inside a project, a second-level
tab strip shows the project sections. Breadcrumb: `Projects / sre / Inbox`.

## Page Designs

### Home: Inbox

Replaces the seven stat cards and the attention queue.

Content, top to bottom:

1. **Needs you** list. Rows are open questions or blocked items across all
   projects. Sources: user-facing messages awaiting a human reply (agent asked
   a question via comms), tasks in `in_progress` with no activity for N hours,
   delegated tasks awaiting acceptance, artifacts awaiting review decision.
   Each row: project chip, kind icon, one-line summary, age, and inline
   actions (Reply, Resolve, Open).
2. **Agents at work** strip. One card per agent with a heartbeat in the last
   hour: agent id, project, task title, last message time. Click opens the
   task. Derived from `X-Agent-Id` on unread polls, confirmations, and task
   claims. No new tables; a small `agent_presence` view over existing data.
3. **Recent activity** feed, collapsed by default. Task status changes,
   artifact versions, memory pushes, pattern updates. Filterable by project.

No global counts of skills, docs, or memories. Those are one click away in
Library and Projects and are never actionable from Home.

### Project Inbox (the messages UI that does not exist today)

A threaded conversation view for one project. This is the largest new surface.

- **Thread list** on the left: one row per root message, newest activity
  first. Badge for state: `open`, `acknowledged`, `answered`, `resolved`.
  Filters: state, source (human / agent / system / bot), agent, date. Bulk
  select with "Resolve selected" and "Resolve all before date".
- **Thread pane** on the right: the root message, all replies and actions in
  order, with author, author type, agent id, hostname, and timestamps. Replies
  from agents are visually distinct from human replies.
- **Composer**: a human types a reply. The gateway inserts a `source='user'`
  message (or reply) and forwards it to the channel plugin so it also appears
  in Discord. Optional "Nudge agents" checkbox sets `deliver_to_agents`.
- **Source classification** shown per message. Bot and webhook messages are
  labeled and default to a collapsed "Alerts" filter so an Alertmanager firehose
  does not bury human conversation.

Data model changes required (see Phase 0 and Phase 3):

- Define message state once, derived not stored twice:
  - `open`: root message, no agent confirmation, no reply, no action.
  - `acknowledged`: at least one row in `agent_confirmations`.
  - `answered`: has a child `reply` or `action`.
  - `resolved`: `resolved_at` set by a human, by an agent via a new
    `POST .../messages/{id}/resolve`, or by retention.
- Add `messages.resolved_at` and `messages.author_kind`
  (`human | bot | webhook | agent | system`). Populate `author_kind` on ingest
  from the Discord author flags (`bot`, webhook id) rather than treating all
  non-gateway authors as humans.
- Add `GET /v1/projects/{ident}/messages` (list with filters, pagination) and
  `GET .../messages/{id}/thread`. The UI and agent-tools both use these.
- Retention purges by `resolved_at` or by age for `bot`/`webhook` and
  `system` rows, not by `confirmed_at`.

### Activity

A single page answering "what are the agents doing". Columns: agent id,
hostname, project, current task, last seen, last message. Secondary section:
the last 100 events across the gateway. This is where the current "In
progress: 8" stat becomes useful.

### Projects registry (replaces the Settings mapping table)

Table of projects with health indicators that are all links:

| Column | Content | Action |
| --- | --- | --- |
| Project | ident, repository full name or "no repository" | opens Overview |
| Channel | Discord channel name and link | opens Discord |
| Open | open inbox items | opens Inbox filtered |
| Tasks | in progress / todo | opens board |
| Last activity | relative time | none |
| State | active / archived | Archive / Restore |

Filters: state, has repository, has activity in 30 days. Junk projects get
archived, not deleted, so historical tasks and memories remain reachable.

Project Settings (per project) replaces the inline mapping form:

- **Identity**: ident (read-only), canonical remote as reported by clients,
  repository provider / namespace / name (prefilled from the canonical remote,
  editable, with validation that never writes a partial mapping).
- **Channel**: which plugin and room; option to create the Discord channel on
  demand rather than at registration.
- **External links**: free-form labeled URLs (CI dashboard, runbook, staging).
  This is where Woodpecker or GitHub Actions links live for users who want
  them. No integration, just links.
- **Lifecycle**: archive, restore.

Auto-configuration from the upstream repo:

- agent-tools sends `canonical_ident` (already computed) as `repo_url` in
  `RegisterProjectRequest`. The gateway parses provider, namespace, and name
  and stores the mapping at registration. Projects registered from a bare
  filesystem path (no git remote) are created with kind `adhoc`, get no
  Discord channel until first message send, and are flagged in the registry.
- Re-registration of an existing project with a `repo_url` back-fills the
  mapping if it is empty. This heals the 18 guessed mappings and the 66
  unmapped ones the next time an agent runs in each repo.
- The bulk "fill unmapped" tool is removed. A "Suggest mappings" action
  proposes repositories from canonical idents seen and requires per-row
  confirmation.

### Tasks

Keep the board. Add:

- A list view with columns (title, status, assignee agent, age, labels,
  comments) and sorting, because 55 cards in one column is not scannable.
- A dedicated task route `/projects/{ident}/tasks/{id}` (memory `457d97de`
  already records the preference for routes over modals) with the full
  thread, delegation status, and linked artifacts. `/task-link/{ref}` redirects
  there.
- Cross-project task list at `/tasks` with the same columns, replacing the
  project picker table. **Reverted after release:** the owner wants `/tasks`
  to stay a three-column drag-and-drop board, so it now renders the board
  with a project switcher instead of a list.

### Artifacts, Memories, Library

Mostly existing. Changes: add Artifacts to project tabs, give the T012 named
views their own routes (version diff, review round, spec manifest), and move
search to the top of Memories. Library pages get consistent list and detail
layouts but no functional changes in this plan.

### Gateway Settings

Auth (session management, key rotation), channel plugins and their status,
retention settings, theme. The Eventic card is gone. The repository mapping
table moves to the Projects registry.

## Technical Architecture

### Authentication for pages

- Add a login page. Submitting the gateway API key (or a dedicated
  `GATEWAY_UI_PASSWORD`) sets an HttpOnly, Secure, SameSite=Strict session
  cookie signed with a server secret.
- The page router and a new `/ui/api/*` prefix accept the session cookie. The
  existing `/v1` bearer router is unchanged for agents.
- `control_panel_close` stops embedding the API key. ndesign is configured
  with `credentials: same-origin` and a CSRF token that the head already has a
  slot for (`csrf-token` meta, currently empty).
- Localhost-only deployments can set `GATEWAY_UI_AUTH=off` to keep today's
  behavior explicitly, never by default.

### Rendering

- Introduce a `ui` module: `crates/gateway/src/ui/{mod,shell,inbox,activity,
  projects,tasks,artifacts,memories,library,settings}.rs`. Move the four HTML
  islands out of `routes.rs`. `routes.rs` keeps API handlers only.
- Adopt a compile-time template engine (askama) with templates under
  `crates/gateway/templates/`. Templates are checked at build time, escaping is
  automatic, and the `he()` / `format!` pattern goes away. Shell layout, nav,
  and tab strip become base templates.
- Keep ndesign as the design system. It is the user's own system and is
  already used across projects. Pin the version, and add a gateway-specific
  stylesheet served from the binary (`/static/gateway.css`, embedded with
  `include_str!`) for the inbox, activity, and board layouts that ndesign does
  not provide.
- Partial updates: pages that need to refresh (Inbox, Activity, board) expose
  fragment endpoints returning HTML snippets. Use ndesign bindings where they
  fit and a small embedded script (no external dependency) for polling or a
  Server-Sent Events stream at `/ui/events`. This avoids a SPA rewrite while
  giving the Inbox a live feel.
- Remove the lenient form-shape parsing in API handlers once the UI posts JSON
  from its own script.

### Data

New or changed tables and columns, all additive with migrations:

- `messages.author_kind`, `messages.resolved_at`, `messages.resolved_by`.
- `projects.kind` (`repo | adhoc`), `projects.archived_at`,
  `projects.canonical_remote`.
- `project_links (project_ident, label, url, rank)`.
- `ui_sessions (id, created_at, expires_at)`.
- Retention rules keyed on `resolved_at` and `author_kind`.

### agent-tools changes

- `register_project` sends `repo_url: canonical_ident` when the ident came from
  a git remote.
- `comms` gains `list` and `resolve` verbs against the new endpoints.
- The comms `confirm` verb is unchanged. Dashboard "acknowledged" derives from
  the confirmations agents already write.

## Phased Roadmap

Each phase ships independently and leaves the system better than before it.

### Phase 0: Stop the bleeding (1 to 2 days)

1. Rotate `GATEWAY_API_KEY`.
2. Session-cookie auth for all HTML pages; remove the bearer token from
   rendered pages.
3. Change the dashboard "unanswered" predicate to `NOT EXISTS
   agent_confirmations`. sre drops from 2593 to roughly 23 immediately.
4. Classify Discord bot and webhook authors on ingest; back-fill
   `author_kind` for existing rows where the Discord author id is known.
5. Widen retention to purge acknowledged user messages, bot messages, and
   non-deliverable system messages past the cutoff.

Acceptance: anonymous `curl` of `/settings` returns a login page; grepping any
page for the API key returns nothing; the sre unanswered badge matches
`agent-tools comms` unread for a fresh agent; database size drops after the
next purge run.

### Phase 1: Remove Build/Eventic, fix project identity (2 to 3 days)

1. Delete the Eventic handlers, structs, and routes (`routes.rs:3839-4090`
   minus the repo-mapping structs, `routes.rs:4125-4190`, `:7943-8065`;
   `main.rs:474-476`, `:616-626`, `:653`), the Build column and Build buttons,
   the Eventic card in Settings, README section "Eventic build status", and the
   `eventic.servers` settings row.
2. Keep repo-mapping columns, endpoints, and tests.
3. Add `repo_url` to `RegisterProjectRequest`; parse and store the mapping on
   registration and on re-registration when empty. Ship the matching
   agent-tools change.
4. Add `projects.kind`, `projects.archived_at`; archive the junk projects;
   stop creating Discord channels for `adhoc` projects at registration.
5. Fix `update_project_repo_mapping` to reject partial input instead of
   clearing the mapping. Remove the bulk fill tool.

Acceptance: `/projects/{ident}/build` is 404; Settings renders without
outbound HTTP; a fresh `agent-tools` run inside a repo produces a fully mapped
project with no manual step; archived projects are hidden from default lists
but their URLs still work.

### Phase 2: UI foundation (1 week)

1. Extract HTML into the `ui` module and askama templates. No visual change
   yet; route-shape tests keep passing.
2. New shell: six-entry sidebar, project tab strip, breadcrumb, login page.
3. Projects registry page replaces the Settings mapping table. Project
   Settings page with identity, channel, external links, lifecycle.
4. Home becomes the Inbox skeleton: needs-you list fed by open messages and
   stalled tasks, agents-at-work strip, activity feed. Remove stat cards.

Acceptance: every number on Home links somewhere; the old dashboard routes
redirect; Lighthouse or equivalent shows no horizontal scroll at 375 px.

### Phase 3: Project Inbox and message actions (1 week)

1. Message list, thread, and resolve endpoints; `resolved_at` semantics;
   `agent-tools comms list|resolve`.
2. Project Inbox page: thread list, thread pane, composer that posts to the
   channel plugin, bulk resolve, source filters with alerts collapsed.
3. Home needs-you rows for messages use the same components.

Acceptance: a human can read the sre thread, reply from the browser, see the
reply in Discord, and bulk-resolve everything older than a date. The sre
badge can reach zero and stays zero until a human or agent asks a question.

### Phase 4: Activity, tasks, artifacts (1 week)

1. Activity page with agent presence derived from existing tables.
2. Task list view, task detail route, cross-project task list; `/task-link`
   redirects.
3. Artifacts in project tabs; named routes for diff, review round, spec
   manifest. Reconcile the T012 task file with the manifest.
4. Memories search-first layout.

### Phase 5: Live updates and polish (ongoing)

SSE stream for Inbox, Activity, and board; keyboard shortcuts (j/k, r to
reply, e to resolve); command palette for project switching; empty states with
the exact CLI command to populate the page; dark and light parity check
against ndesign tokens.

## Decisions Needed

These change the shape of the work and should be settled before Phase 2.

1. **Bot messages: ingest and classify, or drop at the edge?** The plan
   ingests and labels them so alert history stays searchable but collapsed.
   Dropping them at ingest is simpler and smaller. Recommendation: ingest,
   classify, purge bot rows after 7 days.
2. **Human replies from the browser: post to Discord as the gateway bot?**
   The plan says yes, prefixed with the human's display name. The alternative
   is gateway-only replies that agents see via comms but Discord does not.
   Recommendation: post to Discord, so one thread exists.
3. **Acknowledgement semantics: any agent, or a specific agent?** The plan
   treats "any agent confirmed" as acknowledged and shows which agents. If
   per-agent queues matter to the user, the Inbox filter exposes that.
4. **ndesign stays as the base.** The plan assumes yes. If the answer is no,
   Phase 2 grows by roughly a week to build tokens and components from
   scratch, and the traderx and other ndesign consumers stop sharing fixes.

## Success Criteria

- A human can find every open question from an agent, answer it, and clear it
  without leaving the browser or knowing a message id.
- The Home page shows nothing that cannot be acted on from that page.
- A new repository becomes a correctly mapped project on the first
  `agent-tools` command with no Settings visit.
- The API key never leaves the server. Pages require a session.
- No page makes outbound network calls during render.
- `routes.rs` contains no HTML.
- The database stops growing without bound; retention actually deletes rows.
