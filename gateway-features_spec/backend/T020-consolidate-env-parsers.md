# T020 - Consolidate numeric env-var parsers in db.rs

**Team:** backend
**Phase:** 2
**Depends on:** T016
**Status:** todo
**Promoted from:** wave-2 DRY check (refactor + semantic lenses both flagged)

## Scope

**In:** Replace the three near-identical numeric env parsers introduced by T016 (`parse_positive_usize`, `parse_positive_u64`, `parse_positive_u32`) with a single generic helper. Apply before T006 lands so repository functions consume one shape, not three.

**Out:** Changing parsing semantics, behavior, or default values. Touch beyond `crates/gateway/src/db.rs`.

## Implementation notes

- Use a generic over `T: FromStr + PartialOrd<T> + From<u8>` (or similar bound that captures "positive integer-like") so the rejection of zero is uniform.
- Preserve existing public error variants of `OperationsError` — the API surface does not change.
- Keep the per-type helpers as thin wrappers OR remove them entirely; whichever results in fewer call-site changes.

## Acceptance criteria

- [ ] One generic `parse_positive::<T>(...)` (or macro equivalent) is the single implementation; the three previous helpers either delegate to it or are deleted.
- [ ] All T016 unit tests still pass without modification.
- [ ] `cargo clippy -p gateway --all-targets -- -D warnings` is clean.
- [ ] Zero behavior change: same env values produce same parsed limits, same error variants on invalid input.
- [ ] PR diff is restricted to `crates/gateway/src/db.rs`.

## Validation plan

- `cargo test -p gateway` — all 42+ tests pass.
- `cargo clippy -p gateway --all-targets -- -D warnings`.
- Spot-check `ArtifactOperationsEnvelope::from_env` reads behave identically with shrunken-fixture values from T008.

## Provides to downstream tasks

- **T006:** clearer single shape for future numeric env config additions.
