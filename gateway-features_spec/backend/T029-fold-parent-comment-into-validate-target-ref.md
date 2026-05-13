# T029 — Fold parent_comment_id check into validate_target_ref

**Origin**: DRY follow-up from wave T024/T025/T028 (refactor-proposals lens, finding dry-001).

## Problem

`add_artifact_comment` (in `crates/gateway/src/db.rs`) open-codes a SELECT + artifact_id ownership check for `parent_comment_id` (~lines 3716–3724). This duplicates the ownership-check pattern that `validate_target_ref` (T028) already implements for `artifact`, `artifact_version`, and `contribution` kinds. The kind mapping is already present in `read_set_kind_table` (which maps `comment` → `artifact_comments`).

## Change

1. Extend `validate_target_ref` match arms with a `"comment"` case that resolves to `SELECT artifact_id FROM artifact_comments WHERE comment_id = ?1` and verifies the row belongs to the writing artifact.
2. In `add_artifact_comment`, replace the inline parent-comment ownership check with `validate_target_ref(conn, input.artifact_id, "comment", parent_id)`.
3. Keep error message clarity — `validate_target_ref` already uses `"{target_kind} target..."` formatting which is acceptable for parent comments.

## Acceptance criteria

- `validate_target_ref` accepts `"comment"` and validates artifact ownership.
- `add_artifact_comment` parent-comment check delegates to `validate_target_ref`.
- All existing T028 tests pass unchanged.
- A new test covers parent_comment_id ownership rejection (cross-artifact parent) via `add_artifact_comment` to prove the new path is exercised.
- `cargo test -p gateway` passes.
- `cargo clippy -p gateway --all-targets -- -D warnings` is clean.

## Touch surface

- `crates/gateway/src/db.rs` only.

## Out of scope

- Project-scope visibility unification (see iterate-needed memory on T007/T017 link-vs-target visibility split).
- Any change to `read_set_kind_table` or `validate_read_set_refs`.
- Routes work.

## Notes

Estimated net delta: ~−8 LOC, ~10–15 min implementation.
