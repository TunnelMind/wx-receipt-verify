# Closeout: org review item 10, verifier drift script

## (a) What changed

One new file, `scripts/sync-from-tm-weather.sh` (executable, 90 lines). No crate source changed.

- `sync <tm-weather-commit>`: `scripts/sync-from-tm-weather.sh:58-73`. Reads `wx-receipt-verify` from tm-weather with `git -C "$TM_WEATHER_DIR" archive` only, extracts to a temp dir, copies it over this repo, and writes `TM_WEATHER_SYNC` (commit line, blank line, sha256 of every mirrored file). Every failure point comes before the tree is touched.
- `--check`: `scripts/sync-from-tm-weather.sh:74-88`. Hash mode (file set and sha256 against `TM_WEATHER_SYNC`) always; full mode (tree compare against the recorded tm-weather commit) when that commit is present in `$TM_WEATHER_DIR`, else "hash mode only".
- Owned, never-mirrored paths (`.git`, `target*`, `specs`, the script itself, `TM_WEATHER_SYNC`): the `OWNED` regex at `scripts/sync-from-tm-weather.sh:33`.
- Owned-path guard in `extract()`: `scripts/sync-from-tm-weather.sh:50-52` (dies if the tm-weather archive contains an owned path).

## (b) T001 check-after output (verbatim; scratch clone at /tmp/o10, removed afterwards)

Local paths in the block below were replaced with placeholders (`<worktree>`, `<tm-weather>`, `~`); everything else is verbatim.

```
+ git clone -q <worktree> /tmp/o10
+ cp scripts/sync-from-tm-weather.sh /tmp/o10/scripts/
+ cd /tmp/o10
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
+ cmp - /tmp/before.txt
+ echo 'tree unchanged'
tree unchanged
+ cd <worktree>
+ rm -rf /tmp/o10
+ scripts/sync-from-tm-weather.sh --check
sync-from-tm-weather: TM_WEATHER_SYNC missing: run 'scripts/sync-from-tm-weather.sh sync <commit>'
+ echo 'exit 1'
exit 1
+ git diff --stat main -- . ':!specs'
 scripts/sync-from-tm-weather.sh | 90 +++++++++++++++++++++++++++++++++++++++++
 1 file changed, 90 insertions(+)
```

## (c) Expected state

`scripts/sync-from-tm-weather.sh --check` reports drift (exits non-zero: no sync recorded) in this repo until org review item 11 runs the first sync; this is expected

## (d) Not done, and why

- First sync: org review item 11, held on tm-weather spec 030 T005 + T006.
- CI workflow: would fail until item 11 runs the first sync.
- crates.io check: needs network and an owner decision; see owner step 4.
- Publish: not in scope.
- Version bump: no crate source changed.

## (e) Owner steps after merge

1. Merge this draft PR on your word.
2. tm-weather spec 030 T005: mint the errata key (`wx-archive keygen ~/.tm-weather/errata.key`). Check: follow-up run sets `PINNED_ERRATA_PUBKEY`.
3. T006: old vs new `wx-receipt verify` on a public-log snapshot, `cmp` stdout + exit. Check: no difference.
4. Check whether `wx-receipt-verify` 0.1.0 is on crates.io (`cargo search wx-receipt-verify`). Check: if listed, item 11 needs a version bump.
5. Item 11 as its own crew run: `TM_WEATHER_DIR=~/tm-weather scripts/sync-from-tm-weather.sh sync <tm-weather main sha>` then commit. Check: `scripts/sync-from-tm-weather.sh --check` prints "ok: matches tm-weather <sha>" and `cargo test --locked` passes.
6. Only after 5, optionally a CI step `scripts/sync-from-tm-weather.sh --check` (hash mode).
