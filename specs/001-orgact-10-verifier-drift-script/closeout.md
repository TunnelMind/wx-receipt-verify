# Closeout: verifier drift script

## Public-safety redaction (R003)

This repository is public. The files under `specs/001-orgact-10-verifier-drift-script/` were first committed with internal planning text (machine name, local paths, the owner's name, private tm-weather task ids). The redaction is a new commit on the PR branch; there was no force push and no history rewrite. The earlier commit on the PR branch still contains the old text. The owner may ask for the PR to be redone from a clean branch.

## (a) What changed

One new file, `scripts/sync-from-tm-weather.sh` (executable, 90 lines). No crate source changed.

- `sync <tm-weather-commit>`: `scripts/sync-from-tm-weather.sh:58-73`. Reads `wx-receipt-verify` from tm-weather with `git -C "$TM_WEATHER_DIR" archive` only, extracts to a temp dir, copies it over this repo, and writes `TM_WEATHER_SYNC` (commit line, blank line, sha256 of every mirrored file). Every failure point comes before the tree is touched.
- `--check`: `scripts/sync-from-tm-weather.sh:74-88`. Hash mode (file set and sha256 against `TM_WEATHER_SYNC`) always; full mode (tree compare against the recorded tm-weather commit) when that commit is present in `$TM_WEATHER_DIR`, else "hash mode only".
- Owned, never-mirrored paths (`.git`, `target*`, `specs`, the script itself, `TM_WEATHER_SYNC`): the `OWNED` regex at `scripts/sync-from-tm-weather.sh:33`.
- Owned-path guard in `extract()`: `scripts/sync-from-tm-weather.sh:50-52` (dies if the tm-weather archive contains an owned path).

## (b) T001 check-after output (verbatim; scratch clone, removed afterwards)

Local paths in the block below were replaced with placeholders (`<worktree>`, `<tm-weather>`, `~`); everything else is verbatim.

```
+ git clone -q <worktree> <scratch>
+ cp scripts/sync-from-tm-weather.sh <scratch>/scripts/
+ cd <scratch>
+ export TM_WEATHER_DIR=<tm-weather>
+ TM_WEATHER_DIR=<tm-weather>
+ scripts/sync-from-tm-weather.sh sync 8858de0
synced tm-weather 8858de0e2cd4220166e4d8d6d6bcdc32e3ff68cc
+ echo 'exit 0'
exit 0
+ git status --porcelain
 M README.md
 M src/cli.rs
 M src/obs/running_max.rs
 M src/verify.rs
?? TM_WEATHER_SYNC
?? tests/fixtures/errata-2026-11-07/
+ git status -uall --porcelain
+ grep errata
?? tests/fixtures/errata-2026-11-07/01a1b726-c418-7623-a602-0077bf1182e2.json
?? tests/fixtures/errata-2026-11-07/01a1b726-c422-721f-b5a7-07e66eebc511.json
?? tests/fixtures/errata-2026-11-07/01a1b726-c428-70f5-b797-57904533894e.json
?? tests/fixtures/errata-2026-11-07/01a1b726-c42c-713a-ad68-92c0247f28a6.json
+ scripts/sync-from-tm-weather.sh --check
ok: matches tm-weather 8858de0e2cd4220166e4d8d6d6bcdc32e3ff68cc
+ echo 'exit 0'
exit 0
+ TM_WEATHER_DIR=/nonexistent
+ scripts/sync-from-tm-weather.sh --check
ok: hash mode only (tm-weather 8858de0e2cd4220166e4d8d6d6bcdc32e3ff68cc not in /nonexistent)
+ echo 'exit 0'
exit 0
+ echo
+ scripts/sync-from-tm-weather.sh --check
src/lib.rs: FAILED
sha256sum: WARNING: 1 computed checksum did NOT match
sync-from-tm-weather: mirrored files differ from TM_WEATHER_SYNC
+ echo 'exit 1'
exit 1
+ git show HEAD:src/lib.rs
+ touch src/x.rs
+ scripts/sync-from-tm-weather.sh --check
18a19
> src/x.rs
sync-from-tm-weather: mirrored file set differs from TM_WEATHER_SYNC
+ echo 'exit 1'
exit 1
+ rm src/x.rs
+ git status --porcelain
+ TM_WEATHER_DIR=/nonexistent
+ scripts/sync-from-tm-weather.sh sync HEAD
fatal: cannot change to '/nonexistent': No such file or directory
sync-from-tm-weather: cannot resolve HEAD in /nonexistent
+ echo 'exit 1'
exit 1
+ git status --porcelain
+ cmp - <before.txt>
+ echo 'tree unchanged'
tree unchanged
+ cd <worktree>
+ rm -rf <scratch>
+ scripts/sync-from-tm-weather.sh --check
sync-from-tm-weather: TM_WEATHER_SYNC missing: run 'scripts/sync-from-tm-weather.sh sync <commit>'
+ echo 'exit 1'
exit 1
+ git diff --stat main -- . ':!specs'
 scripts/sync-from-tm-weather.sh | 90 +++++++++++++++++++++++++++++++++++++++++
 1 file changed, 90 insertions(+)
```

## (c) Expected state

`scripts/sync-from-tm-weather.sh --check` reports drift (exits non-zero: no sync recorded) in this repo until the first sync runs; this is expected

## (d) Not done, and why

- First sync: held on two checks the owner runs first (see step 2 below).
- CI workflow: would fail until the first sync runs.
- crates.io check: needs network and an owner decision; see owner step 3.
- Publish: not in scope.
- Version bump: no crate source changed.

## (e) Owner steps after merge

1. Merge this draft PR on your word.
2. Run the two held pre-sync checks: the errata key step, and old vs new `wx-receipt verify` on a public-log snapshot (`cmp` stdout + exit). Check: no difference.
3. Check whether `wx-receipt-verify` 0.1.0 is on crates.io (`cargo search wx-receipt-verify`). Check: if listed, the first sync needs a version bump.
4. The first sync as its own crew run: `TM_WEATHER_DIR=<tm-weather> scripts/sync-from-tm-weather.sh sync <tm-weather main sha>` then commit. Check: `scripts/sync-from-tm-weather.sh --check` prints "ok: matches tm-weather <sha>" and `cargo test --locked` passes.
5. Only after 4, optionally a CI step `scripts/sync-from-tm-weather.sh --check` (hash mode).
