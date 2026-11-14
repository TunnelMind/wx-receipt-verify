# Feature: sync script and drift check for the public weather verifier

**Run:** orgact-10-verifier-drift-script · **Repo:** TunnelMind/wx-receipt-verify (PUBLIC) · **Date:** 2026-11-13
**Source:** the owner's org review plan, drift-script item (approved, reversible).

## Intent

The public verifier is a copy of `tm-weather/wx-receipt-verify/` with nothing tying the two together; four days
after release 4 of 68 files differ. Add one script that makes this repo a generated copy of that crate
(a `sync` mode and a hash-mode `--check`, the pattern of a sibling repo's sync script) — and do **not** run the
first sync.

Key sentences from the owner's prompt:

> "Approved as written. Add the sync and drift script only: do NOT run the first sync (the first sync is on hold: it
> waits on two checks the owner runs)."
>
> "The script's --check is expected to report drift until the first sync runs: say so in the PR text. No crate source
> change, no publish."
>
> "ONE DRAFT PR that comes back to the owner for their merge word. Do not merge."

From the plan item: "it adds one file, a script that copies `tm-weather/wx-receipt-verify/` over this repo, records
what it copied, and has a `--check` mode". "The script must copy the crate directory only, because everything
else in tm-weather is private." "This item adds no workflow."

## Re-verification against this repo (2026-11-13, by the planner)

Every fact the plan item cites, checked on this worktree (HEAD `e837ddb`, = `main`) and on the freshest tm-weather
checkout (`main` = `origin/main` = `8858de0`, 2026-11-13):

| Cited fact | Status |
|---|---|
| Public repo holds `scripts/standalone-check.sh` only, no `.github/` | TRUE (`git ls-files`; no `.github`) |
| No sync script / record exists | TRUE |
| 4 files differ: `README.md`, `src/cli.rs`, `src/obs/running_max.rs`, `src/verify.rs`, plus 4 `tests/fixtures/errata-2026-11-07/*.json` only in tm-weather | TRUE against `8858de0` (`diff -rq`); 68 files here, 72 in tm-weather's crate |
| Pattern script exists in a sibling repo | TRUE (hash mode, its `sync` and `--check` shape) |
| 0 open PRs in this repo | NOT CHECKED by the planner: `gh pr list` was denied in this session → GATE 0 |

Note: an older tm-weather checkout (`main` at `b60c5c6`, 2026-11-10) does not show the `src/obs/running_max.rs`
difference. The acceptance test therefore pins tm-weather commit `8858de0` (the fixture commit).

## User Scenarios

**US1 — maintainer brings the public copy up to date (the first sync, later, on the owner's word).**
Given a tm-weather checkout, when `TM_WEATHER_DIR=<checkout> scripts/sync-from-tm-weather.sh sync <commit>` runs,
then the crate directory at that commit replaces this repo's mirrored files and a record file names the commit and
every mirrored file's sha256.

**US2 — anyone checks the public copy has not been hand-edited.**
Given the record exists, when `scripts/sync-from-tm-weather.sh --check` runs without tm-weather, it exits 0 only if
every mirrored file matches its recorded hash and the file set is unchanged ("hash mode"); with a tm-weather
checkout holding the recorded commit it also regenerates and `diff -r`s.

**US3 — today, before the first sync.** Given no record, `--check` exits non-zero and says no sync is recorded.

### Acceptance scenarios (the plan item's check-after, the run's acceptance test)

1. The run's code commit `git diff --stat main..HEAD -- . ':!specs'` lists `scripts/sync-from-tm-weather.sh` and
   nothing else.
2. In a scratch clone with `TM_WEATHER_DIR=<tm-weather>`, `sync 8858de0` leaves `git status --porcelain`
   showing exactly: ` M README.md`, ` M src/cli.rs`, ` M src/obs/running_max.rs`, ` M src/verify.rs`,
   `?? tests/fixtures/errata-2026-11-07/` (4 json files), `?? TM_WEATHER_SYNC`. Nothing else.
3. In that scratch clone after the sync, `--check` exits 0 (full mode: "matches tm-weather 8858de0…"); with
   `TM_WEATHER_DIR=/nonexistent` it exits 0 in hash mode; after appending a byte to `src/lib.rs` it exits non-zero;
   after adding an untracked `src/x.rs` it exits non-zero.
4. In this repo itself `--check` exits non-zero (no record). Before the change it also fails (script absent) —
   this is the "can fail" evidence.
5. `cargo test --locked` passes.

## Functional Requirements

- **FR-001** Add exactly one file, `scripts/sync-from-tm-weather.sh` (bash, `set -euo pipefail`, executable bit
  committed). No other file outside `specs/` changes.
- **FR-002** `sync <commit>` reads tm-weather only via `git -C "$TM_WEATHER_DIR" archive <sha> wx-receipt-verify`
  (never checks out, fetches or writes there); default `TM_WEATHER_DIR=$HOME/tm-weather` (a tm-weather checkout); resolves `<commit>` with
  `rev-parse --verify "<commit>^{commit}"`.
- **FR-003** Only the `wx-receipt-verify/` directory of tm-weather is read; nothing else from that private repo can
  reach this tree.
- **FR-004** Repo-local paths are never mirrored, overwritten or deleted: `.git`, `target/`, `specs/`,
  `scripts/sync-from-tm-weather.sh`, `TM_WEATHER_SYNC`. A sync aborts if the archive contains any of them.
- **FR-005** `sync` replaces the mirrored set: copies every archived file to the same path, removes any mirrored
  file here that the archive no longer has, then writes `TM_WEATHER_SYNC`: line 1 `tm-weather <sha>`, line 2 empty,
  then `sha256sum` lines for every mirrored file, `LC_ALL=C` sorted.
- **FR-006** `--check` fails (non-zero, message on stderr) if `TM_WEATHER_SYNC` is missing, any recorded hash
  differs, or the mirrored file set differs from the record. If `$TM_WEATHER_DIR` holds the recorded commit it
  regenerates and compares the whole mirrored set; otherwise it prints `ok: hash mode only …`.
- **FR-007** Header comment states: the source of truth is tm-weather's crate; never edit mirrored files here;
  hash mode proves the tree matches the record, not that the record matches tm-weather; `--check` fails until the
  first sync.
- **FR-008** `shellcheck` clean.
- **FR-009** No workflow, no README edit, no crate source change, no version bump, no first sync committed.

## Success Criteria

- **SC-001** Acceptance scenarios 1-5 all hold, with their command output pasted into the closeout.
- **SC-002** `bash -n scripts/sync-from-tm-weather.sh && shellcheck scripts/sync-from-tm-weather.sh` exit 0.
- **SC-003** `CARGO_TARGET_DIR=<dir outside the tree> cargo test --locked` exits 0 (31+ tests pass, as on `e837ddb`).
- **SC-004** `git status --porcelain` in this worktree shows no `TM_WEATHER_SYNC`, no `target/`, no changed crate file.

## Edge Cases

- `$TM_WEATHER_DIR` missing or commit unknown → `sync` dies with a message, tree untouched.
- tm-weather renames or deletes a crate file → `sync` removes the old path here (FR-005).
- No `.gitignore` in this repo: `cargo test` writes `target/`; it is excluded from the mirrored set, and the
  verify command sets `CARGO_TARGET_DIR` outside the worktree so it is never committed.
- The worktree's `.git` is a file pointing at the shared repo: scratch tests use `git clone`, never `cp -r`.
- tm-weather `main` moves after 2026-11-13: acceptance scenario 2 pins `8858de0`, so its expected list is stable.

## Assumptions

- A tm-weather checkout at `8858de0` is a fair stand-in for tm-weather `main` (it equals its `origin/main`).
  Reading it with `git archive` changes nothing there.
- Script name `sync-from-tm-weather.sh` and record name `TM_WEATHER_SYNC` follow the sibling
  repo's sync script and record naming.
- The crew's own `specs/001-…` files ride in the same branch; "nothing else" in the check-after means outside `specs/`.

## Out of Scope (and why)

- Running the first sync / committing `TM_WEATHER_SYNC` — held on two checks the owner runs first.
- A CI workflow running `--check` — would fail until the first sync; this item adds no workflow.
- Any crates.io check or publish; any change in tm-weather or any other repo; any merge or deploy.
