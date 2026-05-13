# T012 - Add artifact workspace UI

**Team:** frontend
**Phase:** 4
**Depends on:** T007, T009, T010, T011
**Status:** todo

## Scope

**In:** Add dense operational UI views for artifact lists, artifact detail, version history/diffs, review rounds, spec manifests, documentation search/chunks, comments, and linked tasks/docs/patterns.

**Out:** Decorative landing pages or broad visual redesign of the gateway control panel.

## Source references

- `gateway-features.md` section "UI Experience"
- Existing UI route rendering in `crates/gateway/src/routes.rs`
- Existing control-panel navigation helpers in `routes.rs`

## Deliverables

1. **Artifact list page** with filters.
2. **Artifact detail page** with version, contribution, comment, and link sections.
3. **Version history/diff view**.
4. **Review round view** showing pass 1, pass 2, synthesis, read sets, and decisions.
5. **Spec manifest view** with generated task links.
6. **Documentation browser** with search and chunk inspection.
7. **Navigation update** in the existing control panel.

## Implementation notes

- This is an operational tool, not a marketing surface. Prioritize dense, scannable information and predictable navigation.
- Match existing server-rendered HTML style in `routes.rs`.
- Allow the real server-rendered touch surfaces needed by implementation: route wiring, existing control-panel helpers, templates/assets if introduced, and route shape tests.
- Avoid nested cards and oversized hero treatments; artifact pages should feel like work surfaces.

## Acceptance criteria

- [ ] UI exposes project artifact list filtered by kind, status, label, and actor.
- [ ] Artifact detail page shows current version, accepted version when different, contribution timeline, comments, and linked tasks/docs/patterns.
- [ ] Version history view can show diffs between two artifact versions.
- [ ] Review round view shows pass 1, pass 2, synthesis, read-set/provenance, and decision state.
- [ ] Spec manifest view shows tasks, dependencies, status, acceptance criteria, validation plans, and generated gateway task links.
- [ ] Documentation browser supports search and chunk inspection for current-version and history-aware retrieval modes.
- [ ] UI tests or route shape tests verify dense operational layout and control-panel navigation.

## Validation plan

- **Route shape tests:** Add tests similar to existing `tasks_board_html_shape` and `control_panel_nav_exposes_api_docs`.
- **Manual browser check:** Load artifact list/detail, review round, spec manifest, diff, and documentation browser pages with seeded data and verify filters, links, chunks, and state labels.
- **Responsive sanity:** Check a narrow viewport for text overflow in lists, controls, and manifest rows.

## Dependencies

- **T007:** generic artifact API.
- **T009:** spec workflow.
- **T010:** review workflow.
- **T011:** docs workflow.

## Provides to downstream tasks

- **T014:** rollout validation includes UI smoke checks.
