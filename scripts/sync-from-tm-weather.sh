#!/usr/bin/env bash
# sync-from-tm-weather.sh — everything this repo mirrors is a generated copy of
# the wx-receipt-verify crate in tm-weather. tm-weather is the source of truth;
# never edit mirrored files here (README.md, Cargo.*, src/, tests/, ...).
#
# To change something:
#   1. edit it in tm-weather (wx-receipt-verify/) and commit;
#   2. scripts/sync-from-tm-weather.sh sync <tm-weather-commit>
#   3. commit the result (mirrored files + TM_WEATHER_SYNC).
#
# sync <commit>  reads tm-weather with `git archive` only (never checks out,
#   fetches or writes there), strips the wx-receipt-verify/ prefix, copies every
#   archived file over this tree and deletes previously mirrored files the
#   archive no longer has. It refuses an archive that contains a path this repo
#   owns: .git, target/, specs/, this script, TM_WEATHER_SYNC. TM_WEATHER_SYNC
#   records the tm-weather sha and a sha256 of every mirrored file.
#
# --check  fails if TM_WEATHER_SYNC is missing, any mirrored file was edited, or
#   a file was added/removed. When $TM_WEATHER_DIR holds the recorded commit it
#   also regenerates the mirror and compares it. Without that checkout (CI) it is
#   hash mode only: it proves the tree matches TM_WEATHER_SYNC, NOT that
#   TM_WEATHER_SYNC matches tm-weather.
#   Until the first sync there is no TM_WEATHER_SYNC, so --check fails (org
#   review item 11); run `sync <commit>` once to start.
set -euo pipefail

TM_WEATHER_DIR=${TM_WEATHER_DIR:-$HOME/tm-weather}
cd "$(dirname "$0")/.."

die() { echo "sync-from-tm-weather: $*" >&2; exit 1; }
# Paths this repo owns, never mirrored. target* covers any build dir, e.g. the
# crew loop's target-verify/.
OWNED='^(\.git|target[^/]*|specs)(/|$)|^(scripts/sync-from-tm-weather\.sh|TM_WEATHER_SYNC)$'
# The mirrored set comes from git (tracked + untracked-not-ignored), not from
# walking the tree, so stray build dirs and editor files are never recorded.
# ponytail: assumes plain sha256sum lines (no \ or newline in names) and that this repo's .gitignore matches no crate file.
file_list() {
  git ls-files -z --cached --others --exclude-standard | tr '\0' '\n' |
    LC_ALL=C sort -u | grep -Ev "$OWNED" |
    while IFS= read -r f; do if [ -f "$f" ]; then echo "$f"; fi; done   # drop deleted-but-tracked
}
sums() { xargs -d '\n' -r sha256sum; }   # reads a path list on stdin

# extract <sha> <dir>: tm-weather's crate laid out as this repo's paths.
extract() {
  git -C "$TM_WEATHER_DIR" archive "$1" wx-receipt-verify >"$2.tar"
  # Capture the listing first: `grep -q` in a pipe exits early and, under
  # pipefail, SIGPIPE on tar/sed would silently skip the guard on big archives.
  list=$(tar -tf "$2.tar" | sed 's|^wx-receipt-verify/||')
  if grep -Eq "$OWNED" <<<"$list"; then
    die "archive at $1 contains a path this repo owns"
  fi
  mkdir "$2" && tar -x --strip-components=1 -C "$2" -f "$2.tar"
}

tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT

case "${1:-}" in
  sync)
    [ -n "${2:-}" ] || die "usage: $0 sync <tm-weather-commit>"
    sha=$(git -C "$TM_WEATHER_DIR" rev-parse --verify "$2^{commit}") \
      || die "cannot resolve $2 in $TM_WEATHER_DIR"
    extract "$sha" "$tmp/new"      # every failure point is before we touch the tree
    # ponytail: delete before copy; if cp fails midway the tree is half-updated
    # (git diff shows it) and empty dirs linger. Copy-then-delete if that bites.
    if [ -f TM_WEATHER_SYNC ]; then
      tail -n +3 TM_WEATHER_SYNC | cut -c67- | while IFS= read -r f; do
        # TM_WEATHER_SYNC is tracked in a public repo: never rm outside the
        # tree or a path this repo owns, whatever a merged line says.
        case $f in /*|*..*|'') die "bad path in TM_WEATHER_SYNC: $f";; esac
        if grep -Eq "$OWNED" <<<"$f"; then die "bad path in TM_WEATHER_SYNC: $f"; fi
        [ -e "$tmp/new/$f" ] || rm -f -- "$f"
      done
    fi
    cp -a "$tmp/new/." .
    { echo "tm-weather $sha"; echo; file_list | sums; } > TM_WEATHER_SYNC
    echo "synced tm-weather $sha" ;;
  --check)
    [ -f TM_WEATHER_SYNC ] || die "TM_WEATHER_SYNC missing: run '$0 sync <commit>'"
    sha=$(sed -n '1s/^tm-weather //p' TM_WEATHER_SYNC)
    tail -n +3 TM_WEATHER_SYNC | cut -c67- | diff - <(file_list) >&2 \
      || die "mirrored file set differs from TM_WEATHER_SYNC"
    tail -n +3 TM_WEATHER_SYNC | sha256sum -c --quiet - >&2 \
      || die "mirrored files differ from TM_WEATHER_SYNC"
    if git -C "$TM_WEATHER_DIR" cat-file -e "$sha^{commit}" 2>/dev/null; then
      extract "$sha" "$tmp/new"
      (cd "$tmp/new" && find . -type f | sed 's|^\./||' | LC_ALL=C sort | sums) |
        diff <(tail -n +3 TM_WEATHER_SYNC) - >&2 || die "tree differs from tm-weather $sha"
      echo "ok: matches tm-weather $sha"
    else
      echo "ok: hash mode only (tm-weather $sha not in $TM_WEATHER_DIR)"
    fi ;;
  *) die "usage: $0 sync <tm-weather-commit> | --check" ;;
esac
