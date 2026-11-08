#!/usr/bin/env bash
# Standalone build check for wx-receipt-verify (spec 027 wx-receipt-oss, O004).
#
# Proves the crate builds and tests on its own, as an outsider would after
# cloning the public repo: copied OUT of the tm-weather workspace (so no
# workspace inheritance or sibling `path` deps can leak in), `--locked` (the
# committed Cargo.lock is complete), and with no git credentials at all, so
# the only git dependency that can resolve is a public one. Its lock-drift
# check is also a test (tests/lock_drift.rs), run by `cargo test --locked`.
#
# Why the env vars:
#   GIT_TERMINAL_PROMPT=0, GIT_ASKPASS=/bin/false  - git can never ask for a login
#   GH_TOKEN= GITHUB_TOKEN=                         - no token for gh-based helpers
#   GIT_CONFIG_GLOBAL/SYSTEM=/dev/null              - ignore ~/.gitconfig (helpers, insteadOf rewrites)
#   GIT_CONFIG_*: credential.helper=                - clear any remaining helper
#   CARGO_NET_GIT_FETCH_WITH_CLI=true               - cargo fetches via git, so the above applies
#                                                     (libgit2 would ignore them)
# CARGO_HOME is left alone: the crates.io cache is public and fine to reuse.
set -euo pipefail

crate="$(cd "$(dirname "$0")/.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
dst="$tmp/wx-receipt-verify"
mkdir -p "$dst"
tar --exclude=./target -C "$crate" -cf - . | tar -C "$dst" -xf -

fail() { echo "FAIL: $*" >&2; exit 1; }

# No local-path or workspace-inherited dependencies. ([lib]/[[bin]] `path =`
# is a source file, not a dep, so only dependency sections are checked.)
awk '
  /^[ \t]*#/ { next }
  /^\[/ { dep = ($0 ~ /dependencies/) }
  dep && /(^|[{ ,])path[ \t]*=/ { print; bad = 1 }
  /workspace[ \t]*=[ \t]*true/ { print; bad = 1 }
  END { exit bad }
' "$dst/Cargo.toml" || fail "Cargo.toml has a path or workspace dependency"

# The only git source allowed in the lockfile is the public rune-record (a
# dev-dependency since R007, for the vendored receipt.rs parity test).
others="$(grep '^source = "git+' "$dst/Cargo.lock" |
  grep -v '^source = "git+https://github.com/TunnelMind/rune-record[?#]' || true)"
[ -z "$others" ] || fail "Cargo.lock has a git source other than TunnelMind/rune-record:
$others"

# The crate's own Cargo.lock is what outsiders build with (cargo --locked in
# the public repo); the workspace Cargo.lock is what tm-weather CI builds and
# tests with. If a package shared by both drifts to a different version, CI's
# green says nothing about the crate as published, so fail on any mismatch.
# Packages in only one lock are fine (the workspace has more crates; the crate
# lock has its dev-deps). Checked on $crate, not the copy: the copy has no
# parent lock. In a copy of the public repo there is no workspace lock at all.
# The same check also runs under `cargo test --locked` (tests/lock_drift.rs),
# so plain fmt/clippy/test CI covers it; this copy is belt-and-braces, run
# before any cargo step and against $crate.
pairs() { # "name version" per [[package]], sorted
  awk '{ gsub(/"/, "") } /^\[\[package\]\]/ { n = "" } /^name = / { n = $3 }
       /^version = / && n != "" { print n, $3; n = "" }' "$1" | sort -u
}
export LC_ALL=C # sort, comm and join must collate the same way
ws_lock="$crate/../Cargo.lock"
if [ -f "$ws_lock" ]; then
  # Drift = a crate-lock pair the workspace lock lacks, for a name the
  # workspace does lock. The workspace may also hold other versions of a name
  # (e.g. syn 2 next to syn 3 for other members); that alone is not drift.
  # Output "name crate_ver->workspace_ver[,ver...]".
  drift="$(join <(comm -23 <(pairs "$crate/Cargo.lock") <(pairs "$ws_lock")) \
                <(pairs "$ws_lock" | awk '{ v[$1] = v[$1] (v[$1] ? "," : "") $2 }
                                          END { for (n in v) print n, v[n] }' | sort) |
    awk '{ print "  " $1 " " $2 "->" $3 }')"
  [ -z "$drift" ] || fail "wx-receipt-verify/Cargo.lock drifted from the workspace Cargo.lock (name crate->workspace):
$drift"
else
  echo "NOTE: no workspace Cargo.lock at $ws_lock; lock-drift check skipped"
fi

export GIT_TERMINAL_PROMPT=0 GIT_ASKPASS=/bin/false GH_TOKEN='' GITHUB_TOKEN=''
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null
export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=credential.helper GIT_CONFIG_VALUE_0=''
export CARGO_NET_GIT_FETCH_WITH_CLI=true CARGO_TARGET_DIR="$tmp/target"

run() {
  local log="$tmp/$1.log"
  echo "== cargo $* (in $dst)"
  (cd "$dst" && cargo "$@" --locked) >"$log" 2>&1 || { cat "$log" >&2; fail "cargo $* --locked"; }
  # A credential-less fetch of a private repo can surface as a git error even
  # when cargo recovers from a cache; any such line on a git/fetch line fails.
  if grep -Ei 'authentication|could not read Username|terminal prompts disabled|Permission denied|private' "$log" |
     grep -Ei 'git|fetch|clone|repositor'; then
    fail "private fetch attempted during cargo $*"
  fi
}
run build
run test
# `cargo publish --dry-run` packages and verifies but never uploads (no
# registry token needed): the packaged manifest has no git [dependencies]
# (crates.io refuses them; the git-only rune-record dev-dependency is
# stripped) and the tarball builds alone.
run publish --dry-run

echo "PASS: wx-receipt-verify builds, tests and passes cargo publish --dry-run standalone (--locked, no git credentials)"
