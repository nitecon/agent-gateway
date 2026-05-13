# T018 - Extract artifact insert/serialization helpers in db.rs

**Team:** backend
**Phase:** 2
**Depends on:** T005
**Status:** todo

## Scope

**In:** Refactor the artifact substrate code added in T005 to extract repeated patterns BEFORE T006 multiplies them. Touch surface: `crates/gateway/src/db.rs` only.

**Out:** Behaviour change. No new tables, columns, indexes, triggers, routes, or public API surface. All existing T005 tests must still pass unchanged (signatures may change but semantics must not).

## Rationale (from wave-1 DRY check)

Two independent DRY peers (refactor-proposals + semantic-duplication) flagged the same boilerplate:

1. **JSON serialization pattern repeats 5x** — `.map(|v| serde_json::to_string(v).unwrap_or_else(|_| "null".to_string()))` appears in `artifact_actor_upsert`, `artifact_version_insert`, `workflow_run_insert`, `artifact_chunk_insert`, `artifact_contribution_insert`. T006 will add 8-12 more CRUD helpers; if not extracted now, the duplication compounds.
2. **`serialize_labels()` is misnamed** — it is already being used for non-label `Vec<String>` columns (e.g. `participant_actor_ids`, `generated_*_ids` on workflow_runs). T006 will hit this naming friction repeatedly.
3. **Pure-insert helpers share an identical shape** — generate UUIDv7, transform fields, execute INSERT, return id. 6 of the 8 entity inserts follow this pattern. `artifact_actor_upsert` has different (upsert) semantics and is correctly excluded.

## Deliverables

1. **`serialize_json_or_null(&Option<&serde_json::Value>) -> Option<String>`** helper in the artifact substrate section of `db.rs`. Replace all 5 inline occurrences with calls to this helper.
2. **Rename `serialize_labels` → `serialize_string_array`** at all 7 call sites; update doc comment to reflect general `Vec<String>` → JSON-array semantics. Preserve the same JSON output format (do not change wire shape).
3. **Optional but recommended:** introduce a small macro or generic helper (`artifact_insert!` or `insert_with_uuidv7`) for the 6 pure-insert helpers if it can be done without obscuring the SQL. If the abstraction trades clarity for line-count, leave the inserts inline and document why in a one-line code comment so T006 doesn't re-litigate.
4. **Update the traceability matrix doc-comment** (above `apply_artifact_substrate_schema`) only if it referenced specific function names that change.

## Implementation notes

- Keep helpers private to the module unless T006 will need them across module boundaries.
- The new helpers MUST be used by T006's CRUD additions — leave a one-line comment near each helper noting "used by T005 + T006 CRUD".
- Do NOT touch `artifact_actor_upsert`'s upsert path — its ON CONFLICT branch is intentionally distinct.
- Do NOT widen visibility of internal types; this task should not surface anything new in the crate's public API.

## Acceptance criteria

- [ ] Zero remaining inline occurrences of the `serde_json::to_string(v).unwrap_or_else(|_| "null".to_string())` pattern in `db.rs`.
- [ ] `serialize_labels` is gone (renamed to `serialize_string_array`); no call site references the old name.
- [ ] All T005 tests pass without modification: `cargo test -p gateway db::tests` returns the same 20 passing tests as T005.
- [ ] `cargo test -p gateway` (full crate) passes.
- [ ] `cargo clippy -p gateway --all-targets -- -D warnings` is clean.
- [ ] No new public API; `cargo doc -p gateway --no-deps` does not surface helpers as public items unless explicitly justified.
- [ ] If a macro/helper for inserts was introduced, at least 3 of the 6 pure-insert helpers use it; if not introduced, a single inline comment in `db.rs` explains why the abstraction was rejected.

## Validation plan

- `cargo test -p gateway` — must match prior pass count.
- `cargo clippy -p gateway --all-targets -- -D warnings` — clean.
- `git diff --stat crates/gateway/src/db.rs` — should show net negative or near-zero line delta for the substrate section (refactor, not feature work).

## Provides to downstream tasks

- **T006:** clean helpers to call instead of re-implementing serialization boilerplate.
- **T009/T010/T011:** consistent serialization shape for workflow_run generated-id columns.
