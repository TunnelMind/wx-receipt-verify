# wx-receipt-verify

An offline verifier and scorer for TunnelMind's signed weather record, the
log published at <https://tunnelmind.ai/weather/log/v1/>.

## What it verifies

Every entry in the record is a Receipt v1: a JSON payload, canonicalised
with JCS (RFC 8785), hashed and signed with Ed25519. Most receipts also carry
an RFC 3161 timestamp token from an outside time-stamping authority (TSA)
over the receipt's payload hash. Receipts form one hash chain per stream
(`seq` and `prev`). The verifier checks all of this from the archive files,
the pinned Ed25519 public key and the pinned TSA certificate chains alone.
It makes no network calls and does not read the clock: every time it compares
is one the archive states (a receipt `timestamp`, a TSA genTime, a payload
field).

Each problem is printed on its own `FAIL <check> <name>: …` line, and every
check runs even when an earlier one fails. Things that cannot be checked
offline print as `NOTE …`. A failure covered by a valid signed erratum in
`<dir>/errata/` prints as `KNOWN …` and does not fail the run.

## The ten checks

The full rules are in the doc comments on the `check_*` functions in
`src/verify.rs`.

1. **`signature` / `tsa`**: every receipt verifies under Receipt v1 and is signed by the pinned
   key (a valid signature by any other key fails); every RFC 3161 token verifies under one of
   the pinned TSA chains. A receipt with no token is noted as degraded, not failed.
2. **`chain`**: every stream's `seq` is contiguous from 0 and every `prev` link holds.
3. **`schedule` / `station_map`**: every schedule is valid and comes before the slots it
   covers; every commit is for a scheduled station-day, and a TSA-stamped commit was stamped no
   later than its slot's decision_time + 5 minutes; under a founder-signed station map, every mapped station
   has a schedule for every climate day.
4. **`reveal`**: every forecast and bet commit has exactly one matching reveal, whose
   recomputed commitment is the committed one, made after the commit and after the climate
   day's close.
5. **`settlement`**: bet invariants. Every bet commit resolves to an Accepted forecast reveal,
   has a bet reveal on the Accept's side, and that forecast reveal is listed by exactly one
   settlement; every forecast reveal is listed by exactly one settlement and every Accept has
   at least one bet; a settlement lists only reveals of its own station and day; every
   settlement's net = gross − fee (gross and fee recomputed from the fills of every Buy bet),
   and the nets sum to `ledger.json`.
6. **`nws`**: every settlement's source matches the station's configured source (from
   `--stations`, not the receipt's label); NWS settlements have their hit bracket, outcome and
   day-mean scores recomputed from `observed_high_f`.
7. **`twc`**: TWC settlements without an NWS CLI high are our word; they are noted, never
   failed.
8. **`inputs`**: every reveal input is in the manifest by (source, product_id, sha256) and was
   first seen before its slot's scheduled decision_time.
9. **`peak`** ("peak honest"): the running max a reveal's peak block states is recomputed from
   the listed observations in `products/`, and every observation that was available before the
   decision is listed, so a higher reading cannot be left out.
10. **`market_implied`**: the market-implied block is a coherent set of probabilities (each
    `p_bp` ≤ 10000, brackets sum to 10000 ± 1, gap probabilities non-increasing), a bet's
    `market_implied_ref` is the hash of its reveal's block, and `computed_at` falls in the
    window from 5 minutes before to 4 minutes after the slot's scheduled decision_time. The
    block itself is derived from a market book that is not archived, so it is noted as
    derived, never recomputed.

## Running it against the published log

The log has no directory listing. Its `files.txt` lists every published path,
so mirror it like this (about 38,000 small files, most of them `products/`):

```sh
base=https://tunnelmind.ai/weather/log/v1
dir=wx-log
mkdir -p "$dir"
curl -fsS "$base/files.txt" -o "$dir/files.txt"
sed "s#^#$base/#" "$dir/files.txt" | wget -q -x -nH --cut-dirs=3 -P "$dir" -i -
```

The verifier reads `receipts/`, `proofs/`, `manifest.json`, `ledger.json`,
`products/` and, if present, `errata/`. The other published files
(`index.json`, `days/`, `archive/`, …) are ignored. The layout is described
in the module doc of `src/verify.rs`.

Then verify and score:

```sh
wx-receipt-verify verify "$dir" \
  --pubkey <ed25519 public key, hex> \
  --stations "$dir/stations.json" \
  --tsa-chain "$dir/tsa/digicert.pem" "$dir/tsa/freetsa.pem"

wx-receipt-verify score "$dir"
```

The public key is published in the log as well (`key.json`, and
`key.public_key_hex` in `index.json`). A key read from the same mirror only
proves that the log is consistent with itself, so pin the key you got from a
source you trust. The same applies to the TSA chains. The publisher's own run
of the verifier is in `verify.txt`.

Full usage:

```text
wx-receipt-verify verify <dir> --pubkey <ed25519 hex> --stations <json> [--tsa-chain <pem>…] [--founder-pubkey <ed25519 hex>] [--accept-unproven-schedule] [--as-of <RFC 3339 UTC>]
  <dir>/errata/*.json         optional signed errata; a covered FAIL prints as KNOWN
  --stations <json>           {"<station>": {"nws_settled": bool, "lst_offset_hours"?: int}, …}:
                              nws_settled: which settlements check 6 recomputes;
                              lst_offset_hours: the station's standard-time UTC offset
                              check 9 recomputes the running max with
  --founder-pubkey <hex>      the pinned founder key a station_map rule_change's founder_sig
                              must verify under; without it any station_map fails check 3
  --accept-unproven-schedule  a schedule with no TSA token is a NOTE, not a check 3 failure
                              (test / offline dev only, never for the public record)
  --as-of <RFC 3339 UTC>      e.g. 2026-11-04T18:00:00Z, given by you, never read from the archive:
                              a slot with no commit whose decision_time + 300 s is after it is
                              NOTE pending, not a check 3 failure
       wx-receipt-verify score <dir>
```

Exit codes for `verify`:

| Code | Meaning |
|------|---------|
| 0 | No `FAIL` line is left |
| 1 | Any check failed: one `FAIL <check> <name>: …` line per failure |
| 2 | The archive or the arguments cannot be read; usage on stderr |

`score` prints the Brier and log score of every peak block against the
settled NWS high, as a table followed by one `JSON …` summary line. It needs no
key because it only does arithmetic over the files and trusts them, so run
`verify` first. It exits 0, or 2 when the archive cannot be read.

### Parity with `wx-receipt` (maintainers)

Inside the tm-weather workspace, `wx-receipt` keeps the same CLI by calling
this crate. To check that both binaries agree on the live log:

```sh
wx-receipt        verify "$dir" --pubkey <hex> --stations "$dir/stations.json" --tsa-chain "$dir"/tsa/*.pem > a; echo $? > a.code
wx-receipt-verify verify "$dir" --pubkey <hex> --stations "$dir/stations.json" --tsa-chain "$dir"/tsa/*.pem > b; echo $? > b.code
cmp a b && cmp a.code b.code
wx-receipt score "$dir" > c; wx-receipt-verify score "$dir" > d; cmp c d
```

## Out of scope: outcomes and P&L

This tool checks the record's integrity: who signed it, in what order, when it
was timestamped, and whether its numbers agree with each other. It does
**not** check trading outcomes or P&L. Check 5 only tests that the stated
settlement figures are consistent with each other and with `ledger.json`. It
does not check them against an exchange, and it does not report whether the
forecasts made money. `score` reports forecast accuracy (Brier and log score),
not profit.

## Building

```sh
cargo build --locked
cargo test --locked
scripts/standalone-check.sh   # builds, tests and packages a copy outside any workspace, with no git credentials
```

Every runtime dependency is on crates.io. Receipt v1 verification and the JCS
canonicaliser are vendored, verify-only, in `src/receipt.rs` from the public
[`TunnelMind/rune-record`](https://github.com/TunnelMind/rune-record)
(Apache-2.0; rev in that file's header). rune-record stays a git
dev-dependency for `tests/rune_record_parity.rs`, which checks the copy gives
the same bytes and verdicts.

The test fixtures in `tests/fixtures` hold only published material: day
records copied from the public log and real RFC 3161 tokens with their public
TSA chains (see `tests/fixtures/README`).

## Licence

Licensed under the [Apache License, Version 2.0](LICENSE-APACHE).
