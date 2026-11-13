# Plan: orgact-10-verifier-drift-script

## Constitution check

No `.specify/memory/constitution.md` and no `.specify/templates/` in this repo: no ADR gate applies. House rules
from the owner's prompt apply instead: one draft PR, no merge, no deploy, no other repo touched, no key read,
Conventional Commits, re-verify before acting (done: see spec.md "Re-verification"), check-after must be able to fail.

## Where the change goes

- **New:** `scripts/sync-from-tm-weather.sh` (the only file outside `specs/`).
- **Read only, as a model:** `~/.crew/coord/org-review/repos/rune-record/scripts/sync-from-rune.sh` (same
  `sync`/`--check` shape, `git archive` read, sha256 record, hash-mode fallback at lines 22-26). Differences from it:
  no patch file (the crate is mirrored byte for byte), and the mirrored set is "everything in the archive" rather
  than fixed paths, minus the repo-local exclusions (spec FR-004).
- **Read only, as the source:** `~/crewbase-tm-weather` at `8858de0` (tm-weather `main` = `origin/main`,
  2026-11-13). `~/tm-weather` is stale (`b60c5c6`); do not fetch or switch it.
- **Untouched:** everything in `src/`, `tests/`, `Cargo.*`, `README.md`, `scripts/standalone-check.sh`.

## Approach

1. Write the script (~70 lines of bash, rune-record's structure). Mirrored file list = `find . -type f` minus
   `./.git`, `./target`, `./specs`, `./scripts/sync-from-tm-weather.sh`, `./TM_WEATHER_SYNC`, `LC_ALL=C sort`.
2. `sync`: `git archive <sha> wx-receipt-verify | tar -x` into a temp dir; refuse if it contains an excluded path;
   delete mirrored files not in the archive; copy the archive over; write the record.
3. `--check`: record present → `sha256sum -c --quiet` → file-set `diff` → if `git -C "$TM_WEATHER_DIR" cat-file -e
   "$sha^{commit}"`, regenerate and `diff -r` per file set, else "ok: hash mode only".
4. Acceptance in a scratch `git clone` of this worktree plus the new script copied in (never `cp -r` the worktree:
   its `.git` is a pointer into the shared repo).

## Risks

- **Hash mode is weaker than it looks:** without tm-weather (public CI, outsiders) `--check` proves only that the
  tree matches the record. Stated in the header and the PR text.
- **`--check` fails here until item 11.** By design; stated in the PR text. No workflow is added, so nothing turns red.
- **Leaking private tm-weather content:** only `wx-receipt-verify/` is archived (FR-003); acceptance scenario 2 shows
  the exact changed-file list.
- **Accidental first sync committed:** the sync is run only in a `/tmp` scratch clone; SC-004 checks the worktree.
- **`target/` committed:** the repo has no `.gitignore`; verify uses `CARGO_TARGET_DIR` outside the tree.
- **Undo:** revert the one commit.

## Verify (every task)

The repo has **no CI** (no `.github/`), so the verify is the repo's own test command, as the check-after names it and
as `scripts/standalone-check.sh` runs it, plus a shell check of the new file once it exists:

```
CARGO_TARGET_DIR=/tmp/orgact10-target cargo test --locked && { [ ! -e scripts/sync-from-tm-weather.sh ] || { bash -n scripts/sync-from-tm-weather.sh && shellcheck scripts/sync-from-tm-weather.sh; }; }
```

Baseline on `e837ddb` (planner, 2026-11-13): `cargo test --locked` all suites `ok` (31 lib tests + integration).
