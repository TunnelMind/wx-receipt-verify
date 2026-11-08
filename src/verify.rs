//! The offline verifier (C002): Fable §5's eight checks, plus check 9 (C021,
//! the peak block) and check 10 (C061, the market-implied block), over a published
//! archive directory, from the files plus the pinned Ed25519 key and TSA
//! chains alone. No network, no clock: every time compared is one the archive
//! states (a receipt `timestamp`, a TSA genTime, a payload field).
//!
//! Archive layout (docs/receipts.md copies this):
//!
//! ```text
//! <dir>/receipts/*.json    one signed Receipt v1 per file; any names, any order
//! <dir>/proofs/<hex>.json  optional tsa::Proof for the receipt whose
//!                          payload_hash is 0x<hex> (lowercase, no 0x in the name)
//! <dir>/manifest.json      the public archive manifest:
//!                          [{"source", "product_id", "sha256", "first_seen"}, …]
//! <dir>/ledger.json        {"net_cents": <i64>}, our reported P&L total
//! <dir>/products/<sha256>.raw  the archived product body whose SHA-256 is
//!                          <sha256> (lowercase hex, as in the manifest); read
//!                          only by check 9. For each peak reveal: every body
//!                          in peak.obs, and every metar body the manifest
//!                          lists as first seen in [climate-day UTC midnight
//!                          - (lst_offset_hours + 1) h, decision_time), other
//!                          stations' included; and every cli / cli_prelim body
//!                          the manifest lists as first seen before a peak
//!                          decision (no window, all stations and days). A
//!                          missing one is FAIL 9
//! <dir>/errata/*.json      optional signed errata, one Receipt v1 per file,
//!                          payload {"type": "wx.erratum", "covers": [<exact
//!                          FAIL line>, …], "fact", "cause"}; never mixed
//!                          into receipts/ (see [`apply_errata`])
//! ```
//!
//! Streams: a receipt whose payload has a `station` is in that station's
//! stream; a bet commit or reveal is in the stream of the forecast commit it
//! acts on; everything else (refill, rule_change, model_version) is in the
//! [`GLOBAL_STREAM`]. Each stream is ordered by `seq` before [`check_chain`].
//!
//! [`verify`] runs every check and collects every failure, one line each,
//! `FAIL <check> <name>: …`, plus `NOTE …` lines for what cannot be checked.
//! Last, valid errata move the failure lines they name verbatim to `known`.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::receipt as receipt_v1;
use anyhow::{anyhow, ensure, Context, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use serde::Deserialize;
use serde_json::Value;

use crate::obs::cli::{CliKind, ClimateDay};
use crate::tsa::{self, Proof};
use crate::*;

/// Check 3's stated bound: a commit's TSA genTime may be at most this long
/// after its slot's decision_time. Five minutes covers the TSA round trip and
/// a retry on the second TSA; "within seconds" is the target, not the bound.
pub const COMMIT_BOUND_SECS: i64 = 300;

/// Check 10's window on `market_implied.computed_at`: the book is read at
/// decision time, so the block may be computed at most this long before the
/// slot's scheduled decision_time (a fetch started ahead of the slot)...
pub const MARKET_IMPLIED_LEAD_SECS: i64 = 5 * 60;
/// ...and at most this long after it: four minutes, leaving room under
/// [`COMMIT_BOUND_SECS`] for the commit's TSA round trip.
pub const MARKET_IMPLIED_LAG_SECS: i64 = 240;

/// The stream of receipts that belong to no station.
pub const GLOBAL_STREAM: &str = "_global";

/// One signed receipt from `receipts/`.
pub struct Rec {
    pub file: String,
    pub raw: Value,
    pub payload: Payload,
    /// `payload_hash` as signed, "0x"-prefixed.
    pub hash: String,
    /// The receipt's own `timestamp`, unix ms.
    pub ts_ms: i64,
}

/// A loaded archive directory (layout in the module doc).
pub struct Archive {
    pub receipts: Vec<Rec>,
    /// Keyed by payload_hash without "0x".
    pub proofs: BTreeMap<String, Proof>,
    pub manifest: Vec<Input>,
    pub ledger_net_cents: i64,
    /// `errata/`, unverified as loaded; [`apply_errata`] checks them.
    pub errata: Vec<Erratum>,
    /// payload_hash -> index into `receipts`.
    index: BTreeMap<String, usize>,
    /// The archive directory, for check 9's lazy `products/` reads.
    dir: PathBuf,
    /// [`Archive::product`] results by sha256, so each body is read and
    /// hashed once per verify: check 9 meets the same body in peak.obs, the
    /// (c) scan and every reveal of a station-day. An error is kept as its
    /// `{:#}` text, which is how every caller reports it, so a bad body still
    /// fails at each use with the same message.
    products: RefCell<BTreeMap<String, std::result::Result<String, String>>>,
    /// `products/` files actually read from disk (see [`Archive::product_reads`]).
    reads: Cell<usize>,
}

/// One file from `errata/`, as read. Its signature and body are checked in
/// [`apply_errata`], not at load, so a bad erratum is a FAIL line rather than
/// an archive that will not load. That includes a file that is not JSON at
/// all (say a truncated upload): `raw` is then `Null` and `load_err` holds
/// the parse error, reported by [`apply_errata`] like any other bad erratum.
pub struct Erratum {
    pub file: String,
    pub raw: Value,
    pub load_err: Option<String>,
}

/// An erratum's payload. Unknown fields are refused, so nothing can ride
/// along unseen under the signature.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ErratumBody {
    #[serde(rename = "type")]
    ty: String,
    covers: Vec<String>,
    #[allow(dead_code)] // signed and published; the verifier only needs `covers`
    fact: String,
    #[allow(dead_code)]
    cause: String,
}

#[derive(Debug, Default)]
pub struct Report {
    pub failures: Vec<String>,
    pub notes: Vec<String>,
    /// Failures a valid erratum covers: `KNOWN <check> <name>: <msg> (erratum
    /// <receipt_id>)`. They do not count as failures.
    pub known: Vec<String>,
}

impl Report {
    fn fail(&mut self, check: u8, name: &str, msg: impl std::fmt::Display) {
        self.failures.push(format!("FAIL {check} {name}: {msg}"));
    }
}

fn read_json(p: &Path) -> Result<Value> {
    let bytes = std::fs::read(p).with_context(|| format!("reading {}", p.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", p.display()))
}

fn json_files(dir: &Path) -> Result<Vec<std::path::PathBuf>> {
    let mut v = Vec::new();
    for e in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let p = e?.path();
        if p.extension().is_some_and(|x| x == "json") {
            v.push(p);
        }
    }
    // Stable output order; the checks themselves do not depend on it.
    v.sort();
    Ok(v)
}

impl Archive {
    /// Read `dir`. A file that is missing or not what the layout says is an
    /// error: there is nothing to verify until the archive itself parses.
    pub fn load(dir: &Path) -> Result<Archive> {
        let mut receipts = Vec::new();
        for path in json_files(&dir.join("receipts"))? {
            let raw = read_json(&path)?;
            let file = path.file_name().unwrap_or_default().to_string_lossy();
            let payload = serde_json::from_value(raw["payload"].clone())
                .with_context(|| format!("{file}: payload is not a wx receipt payload"))?;
            let hash = raw["payload_hash"]
                .as_str()
                .with_context(|| format!("{file}: no payload_hash"))?
                .to_string();
            let ts_ms = raw["timestamp"]
                .as_str()
                .and_then(unix_ms)
                .with_context(|| format!("{file}: timestamp missing or not RFC 3339 UTC"))?;
            receipts.push(Rec {
                file: file.into_owned(),
                raw,
                payload,
                hash,
                ts_ms,
            });
        }
        let mut proofs = BTreeMap::new();
        let proof_dir = dir.join("proofs");
        if proof_dir.exists() {
            for path in json_files(&proof_dir)? {
                let stem = path.file_stem().unwrap_or_default().to_string_lossy();
                let proof = serde_json::from_value(read_json(&path)?)
                    .with_context(|| format!("{}: not a tsa proof", path.display()))?;
                proofs.insert(stem.to_ascii_lowercase(), proof);
            }
        }
        let mut errata = Vec::new();
        let errata_dir = dir.join("errata");
        if errata_dir.exists() {
            for path in json_files(&errata_dir)? {
                let file = path.file_name().unwrap_or_default().to_string_lossy();
                let (raw, load_err) = match read_json(&path) {
                    Ok(v) => (v, None),
                    Err(err) => (Value::Null, Some(format!("{err:#}"))),
                };
                errata.push(Erratum {
                    file: file.into_owned(),
                    raw,
                    load_err,
                });
            }
        }
        let manifest = serde_json::from_value(read_json(&dir.join("manifest.json"))?)
            .context("manifest.json is not [{source, product_id, sha256, first_seen}]")?;
        #[derive(Deserialize)]
        struct Ledger {
            net_cents: i64,
        }
        let ledger: Ledger = serde_json::from_value(read_json(&dir.join("ledger.json"))?)
            .context("ledger.json is not {net_cents}")?;
        let index = receipts
            .iter()
            .enumerate()
            .map(|(i, r)| (r.hash.clone(), i))
            .collect();
        Ok(Archive {
            receipts,
            proofs,
            manifest,
            ledger_net_cents: ledger.net_cents,
            errata,
            index,
            dir: dir.to_path_buf(),
            products: RefCell::default(),
            reads: Cell::new(0),
        })
    }

    /// The archived body `products/<sha256>.raw`, as text. Err names what is
    /// wrong: a name that is not 64 lowercase hex (never joined into a path,
    /// so a receipt cannot point outside the archive), a missing file, bytes
    /// whose SHA-256 is not their name, or bytes that are not UTF-8.
    /// Each body is read and hashed once per Archive; later calls clone it.
    pub fn product(&self, sha256: &str) -> Result<String> {
        let cached = self.products.borrow().get(sha256).cloned();
        let got = cached.unwrap_or_else(|| {
            let got = self.read_product(sha256).map_err(|e| format!("{e:#}"));
            self.products
                .borrow_mut()
                .insert(sha256.to_owned(), got.clone());
            got
        });
        got.map_err(|e| anyhow!(e))
    }

    /// The uncached read behind [`Archive::product`].
    fn read_product(&self, sha256: &str) -> Result<String> {
        ensure!(
            sha256.len() == 64
                && sha256
                    .bytes()
                    .all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')),
            "product {sha256:?}: sha256 is not 64 lowercase hex"
        );
        let p = self.dir.join("products").join(format!("{sha256}.raw"));
        self.reads.set(self.reads.get() + 1);
        let bytes = std::fs::read(&p)
            .with_context(|| format!("product {sha256}: no products/{sha256}.raw"))?;
        let got = sha256_hex(&bytes);
        ensure!(
            got == sha256,
            "product {sha256}: products/{sha256}.raw has SHA-256 {got}"
        );
        String::from_utf8(bytes).with_context(|| format!("product {sha256}: not UTF-8"))
    }

    /// How many `products/` files [`Archive::product`] has tried to read from
    /// disk (failed reads included), for tests of what check 9 reads.
    pub fn product_reads(&self) -> usize {
        self.reads.get()
    }

    fn get(&self, hash: &str) -> Option<&Rec> {
        self.index.get(hash).map(|&i| &self.receipts[i])
    }
}

/// The verifier's station config, keyed by settlement `station`. It comes
/// from the person verifying, never from the archive, so the agent's own
/// `settlement_source` label cannot pick which stations check 6 recomputes,
/// and the agent cannot pick the climate-day window check 9 recomputes over.
pub type Stations = BTreeMap<String, StationConfig>;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StationConfig {
    pub nws_settled: bool,
    /// Standard-time UTC offset, e.g. -6 for CST (crate::obs::running_max). Absent:
    /// check 9 notes the station and does not recompute its running max.
    #[serde(default)]
    pub lst_offset_hours: Option<i32>,
}

/// Parse the CLI's `--stations` file: a JSON object keyed by station,
/// `{"KMDW": {"nws_settled": true, "lst_offset_hours": -6}, "KDFW": {"nws_settled": false}}`.
/// An unknown field is an error, not ignored.
///
/// ponytail: a JSON subset of wx-archive's stations.toml until that file lands.
pub fn parse_stations(json: &str) -> Result<Stations> {
    serde_json::from_str(json).context(
        "stations file is not {\"<station>\": {\"nws_settled\": bool, \"lst_offset_hours\"?: int}, …}",
    )
}

/// Load `dir` and run all ten checks. `accept_unproven_schedule` is the
/// CLI's `--accept-unproven-schedule` (see [`check_schedules`]);
/// `founder_pubkey_hex` its `--founder-pubkey` (see [`check_station_maps`]).
pub fn verify_dir(
    dir: &Path,
    pubkey_hex: &str,
    chains_pem: &[String],
    stations: &Stations,
    accept_unproven_schedule: bool,
    founder_pubkey_hex: Option<&str>,
) -> Result<Report> {
    Ok(verify(
        &Archive::load(dir)?,
        pubkey_hex,
        chains_pem,
        stations,
        accept_unproven_schedule,
        founder_pubkey_hex,
    ))
}

/// All ten checks; every failure is collected, none stops the others. Then
/// [`apply_errata`] moves erratum-covered failures to `known`.
pub fn verify(
    a: &Archive,
    pubkey_hex: &str,
    chains_pem: &[String],
    stations: &Stations,
    accept_unproven_schedule: bool,
    founder_pubkey_hex: Option<&str>,
) -> Report {
    verify_as_of(
        a,
        pubkey_hex,
        chains_pem,
        stations,
        accept_unproven_schedule,
        founder_pubkey_hex,
        None,
    )
}

/// [`verify`] with the CLI's `--as-of` (unix ms, see [`check_schedules`]).
pub fn verify_as_of(
    a: &Archive,
    pubkey_hex: &str,
    chains_pem: &[String],
    stations: &Stations,
    accept_unproven_schedule: bool,
    founder_pubkey_hex: Option<&str>,
    as_of_ms: Option<i64>,
) -> Report {
    let mut r = Report::default();
    let gen = check_signatures(a, pubkey_hex, chains_pem, &mut r);
    check_chains(a, &mut r);
    check_schedules(a, &gen, accept_unproven_schedule, as_of_ms, &mut r);
    check_station_maps(a, founder_pubkey_hex, &mut r);
    check_reveals(a, &gen, &mut r);
    check_settlements(a, &mut r);
    check_nws(a, stations, &mut r);
    check_twc(a, &mut r);
    check_inputs(a, &mut r);
    check_peaks(a, stations, &mut r);
    check_market_implied(a, &mut r);
    apply_errata(a, pubkey_hex, &mut r);
    r
}

/// An erratum's body if its receipt verifies, is signed by the pinned key
/// (`want`, lowercase hex) and its payload is a `wx.erratum`.
fn erratum_body(e: &Erratum, want: &str) -> Result<ErratumBody> {
    if let Some(err) = &e.load_err {
        return Err(anyhow!("{err}"));
    }
    let v = receipt_v1::verify(&e.raw)?;
    let got = B64
        .decode(&v.public_key)
        .map(hex::encode)
        .unwrap_or_default();
    ensure!(got == want, "signed by key {got}, not the pinned key");
    let body: ErratumBody = serde_json::from_value(e.raw["payload"].clone())
        .context("payload is not {type, covers, fact, cause}")?;
    ensure!(
        body.ty == "wx.erratum",
        "payload type {:?}, not wx.erratum",
        body.ty
    );
    Ok(body)
}

/// Errata are a view, not a rewrite: run after every check, they never touch
/// `receipts/`. A failure line moves to `known` only when it EXACTLY equals a
/// `covers` entry of an erratum that verifies under the pinned key, so a
/// reworded check or another row cannot be masked (fails closed). An erratum
/// that does not verify is `FAIL 1 erratum` and covers nothing; those lines
/// are added after matching, so no erratum can cover them.
pub fn apply_errata(a: &Archive, pubkey_hex: &str, r: &mut Report) {
    let want = pubkey_hex.trim().to_ascii_lowercase();
    let mut bad = Vec::new();
    for e in &a.errata {
        let body = match erratum_body(e, &want) {
            Ok(b) => b,
            Err(err) => {
                bad.push(format!("{}: {err:#}", e.file));
                continue;
            }
        };
        let id = e.raw["receipt_id"].as_str().unwrap_or(&e.file);
        for line in &body.covers {
            match r.failures.iter().position(|f| f == line) {
                Some(i) => {
                    r.failures.remove(i);
                    let rest = line.strip_prefix("FAIL ").unwrap_or(line);
                    r.known.push(format!("KNOWN {rest} (erratum {id})"));
                }
                None => r
                    .notes
                    .push(format!("NOTE erratum {id}: covers no failure: {line}")),
            }
        }
    }
    for msg in bad {
        r.fail(1, "erratum", msg);
    }
}

fn receipt_id(rec: &Rec) -> &str {
    rec.raw["receipt_id"].as_str().unwrap_or(&rec.file)
}

/// Check 1: every receipt verifies under receipt_v1 AND is signed by the
/// pinned key (a valid signature by any other key fails); every rfc3161 proof
/// verifies under one of the pinned chains. Returns the verified genTimes
/// (unix s) by payload_hash; a receipt without one is degraded, not failed.
pub fn check_signatures(
    a: &Archive,
    pubkey_hex: &str,
    chains_pem: &[String],
    r: &mut Report,
) -> BTreeMap<String, i64> {
    let want = pubkey_hex.trim().to_ascii_lowercase();
    let mut gen = BTreeMap::new();
    for rec in &a.receipts {
        match receipt_v1::verify(&rec.raw) {
            Err(e) => r.fail(1, "signature", format!("{}: {e:#}", rec.file)),
            Ok(v) => {
                let got = B64
                    .decode(&v.public_key)
                    .map(hex::encode)
                    .unwrap_or_default();
                if got != want {
                    r.fail(
                        1,
                        "signature",
                        format!("{}: signed by key {got}, not the pinned key", rec.file),
                    );
                }
            }
        }
        let id = receipt_id(rec);
        match a.proofs.get(rec.hash.trim_start_matches("0x")) {
            Some(p) if p.method == "rfc3161" => match proof_time(&rec.raw, p, chains_pem) {
                Ok(t) => {
                    gen.insert(rec.hash.clone(), t);
                }
                Err(e) => r.fail(1, "tsa", format!("{id}: {e:#}")),
            },
            Some(p) if p.method != "none" => r.fail(
                1,
                "tsa",
                format!("{id}: unknown proof method {:?}", p.method),
            ),
            _ => r.notes.push(format!("NOTE degraded: {id}")),
        }
    }
    gen
}

/// The genTime (unix s) of an rfc3161 `proof` for `receipt`: its imprint must
/// be the receipt's payload_hash and its token must verify under any one of
/// `chains_pem`.
pub fn proof_time(receipt: &Value, proof: &Proof, chains_pem: &[String]) -> Result<i64> {
    let imprint = tsa::imprint_of(receipt)?;
    ensure!(
        proof.imprint.as_deref() == Some(hex::encode(imprint).as_str()),
        "proof imprint {:?} is not this receipt's payload_hash",
        proof.imprint
    );
    let tsr = B64
        .decode(
            proof
                .token
                .as_deref()
                .context("rfc3161 proof has no token")?,
        )
        .context("proof token is not base64")?;
    // Every chain's reason, in --tsa-chain order: the one that names the real
    // problem (wrong imprint under the right chain) is rarely the last.
    let mut why = Vec::new();
    for (i, chain) in chains_pem.iter().enumerate() {
        match tsa::verify_token(&tsr, &imprint, chain) {
            Ok(info) => return Ok(info.gen_time),
            Err(e) => why.push(format!("chain {}: {e:#}", i + 1)),
        }
    }
    if why.is_empty() {
        why.push("no --tsa-chain given".into());
    }
    Err(anyhow!(
        "token verifies under no pinned TSA chain ({})",
        why.join("; ")
    ))
}

/// The stream `rec` belongs to (module doc).
pub fn stream_key(a: &Archive, rec: &Rec) -> String {
    let mut rec = rec;
    // ponytail: bet reveal -> bet commit -> forecast commit is the longest
    // link chain the payload types allow; three hops, then give up.
    for _ in 0..3 {
        if let Some(s) = rec.raw["payload"]["station"].as_str() {
            return s.to_string();
        }
        let next = match &rec.payload.body {
            Body::BetCommit(b) => &b.reveal_commit_ref,
            Body::BetReveal(b) => &b.commit_ref,
            _ => break,
        };
        match a.get(next) {
            Some(n) => rec = n,
            None => break,
        }
    }
    GLOBAL_STREAM.to_string()
}

/// Check 2: every stream's seq is contiguous from 0 and its prev links hold.
pub fn check_chains(a: &Archive, r: &mut Report) {
    let mut streams: BTreeMap<String, Vec<&Rec>> = BTreeMap::new();
    for rec in &a.receipts {
        streams.entry(stream_key(a, rec)).or_default().push(rec);
    }
    for (key, mut recs) in streams {
        recs.sort_by_key(|x| x.payload.seq);
        let raws: Vec<Value> = recs.iter().map(|x| x.raw.clone()).collect();
        if let Err(e) = check_chain(&raws) {
            r.fail(2, "chain", format!("stream {key}: {e:#}"));
        }
    }
}

/// (station, climate_day, slot_id) of a receipt that counts as a slot's commit.
fn slot_of(b: &Body) -> Option<(&str, &str, &str)> {
    match b {
        Body::ForecastCommit(c) => Some((&c.station, &c.climate_day, &c.slot_id)),
        Body::BetRefused(c) => Some((&c.station, &c.climate_day, &c.slot_id)),
        _ => None,
    }
}

/// Check 3: every schedule passes [`check_schedule`] and has a plausible
/// `close_utc` ([`close_plausible`]); every commit is for a
/// scheduled station-day; a commit with a verified genTime was stamped no
/// later than its slot's decision_time + [`COMMIT_BOUND_SECS`]. Degraded
/// commits skip the time bound (their own `timestamp` is our claim, not proof).
///
/// The schedule must also come before every slot it covers, because
/// `published_at` is only a payload field we wrote:
/// (a) its seq is below every commit / refusal for the station-day in the same
///     stream (structural; holds for degraded receipts too);
/// (b) its verified genTime is before the earliest decision_time (the only
///     part independent of our clock);
/// (c) its own `timestamp` is before each commit's (self-attested: a
///     consistency check, not proof).
///
/// With no genTime for the schedule its time is unproven and the station-day
/// fails; `accept_unproven` (test / offline dev only) makes that a NOTE.
///
/// `as_of_ms` (the CLI's `--as-of`, unix ms, given by the verifier and never
/// read from the archive or the clock): a slot with no commit whose
/// decision_time + [`COMMIT_BOUND_SECS`] is after it is not due yet and prints
/// `NOTE pending: slot <id>`; past that bound it still FAILs. `None` changes
/// nothing.
pub fn check_schedules(
    a: &Archive,
    gen: &BTreeMap<String, i64>,
    accept_unproven: bool,
    as_of_ms: Option<i64>,
    r: &mut Report,
) {
    let raws: Vec<Value> = a.receipts.iter().map(|x| x.raw.clone()).collect();
    let mut scheduled = BTreeSet::new();
    for rec in &a.receipts {
        let Body::Schedule(s) = &rec.payload.body else {
            continue;
        };
        scheduled.insert((s.station.as_str(), s.climate_day.as_str()));
        if !close_plausible(&s.climate_day, &s.close_utc) {
            r.fail(
                3,
                "schedule",
                format!(
                    "{} {}: close_utc {:?} is not 23:59 local at any US zone (UTC-4 to UTC-10)",
                    s.station, s.climate_day, s.close_utc
                ),
            );
        }
        match check_schedule_as_of(s, &raws, as_of_ms) {
            Ok(pending) => r
                .notes
                .extend(pending.iter().map(|id| format!("NOTE pending: slot {id}"))),
            Err(e) => r.fail(
                3,
                "schedule",
                format!("{} {}: {e:#}", s.station, s.climate_day),
            ),
        }
        let stream = stream_key(a, rec);
        for c in &a.receipts {
            let Some((st, day, slot)) = slot_of(&c.payload.body) else {
                continue;
            };
            if (st, day) != (s.station.as_str(), s.climate_day.as_str()) {
                continue;
            }
            if stream_key(a, c) == stream && rec.payload.seq >= c.payload.seq {
                r.fail(
                    3,
                    "schedule",
                    format!(
                        "{st} {day}: schedule seq {} not before slot {slot} commit seq {}",
                        rec.payload.seq, c.payload.seq
                    ),
                );
            }
            if rec.ts_ms >= c.ts_ms {
                r.fail(
                    3,
                    "schedule",
                    format!(
                        "{st} {day}: schedule timestamp {} not before slot {slot} commit timestamp {}",
                        rec.ts_ms, c.ts_ms
                    ),
                );
            }
        }
        let first = s
            .slots
            .iter()
            .filter_map(|x| unix_ms(&x.decision_time))
            .min();
        match (gen.get(&rec.hash), first) {
            (Some(&g), Some(first)) if g * 1000 >= first => r.fail(
                3,
                "schedule",
                format!(
                    "{} {}: schedule genTime {g} not before first decision_time (unix ms {first})",
                    s.station, s.climate_day
                ),
            ),
            (Some(_), _) => {}
            // Not `NOTE degraded: `, which check 1 already printed for this receipt.
            (None, _) if accept_unproven => r.notes.push(format!(
                "NOTE schedule: {} {}: schedule time unproven",
                s.station, s.climate_day
            )),
            (None, _) => r.fail(
                3,
                "schedule",
                format!("{} {}: schedule time unproven", s.station, s.climate_day),
            ),
        }
        for slot in &s.slots {
            let Some(due) = unix_ms(&slot.decision_time) else {
                r.fail(
                    3,
                    "schedule",
                    format!("slot {}: bad decision_time", slot.slot_id),
                );
                continue;
            };
            let key = (
                s.station.as_str(),
                s.climate_day.as_str(),
                slot.slot_id.as_str(),
            );
            for c in &a.receipts {
                // Commits, refusals and reveals each sign a decision_time;
                // it must be the scheduled one, or check 8 could be measured
                // against a later instant that admits a late input.
                if let Some((k, t)) = slot_decision(&c.payload.body) {
                    if k == key && unix_ms(t) != Some(due) {
                        r.fail(
                            3,
                            "schedule",
                            format!(
                                "slot {}: decision time not the scheduled slot ({t} vs scheduled {})",
                                slot.slot_id, slot.decision_time
                            ),
                        );
                    }
                }
                if slot_of(&c.payload.body) != Some(key) {
                    continue;
                }
                if let Some(&g) = gen.get(&c.hash) {
                    if g * 1000 > due + COMMIT_BOUND_SECS * 1000 {
                        r.fail(
                            3,
                            "schedule",
                            format!(
                                "slot {}: commit genTime {g} is more than {COMMIT_BOUND_SECS} s after decision_time {}",
                                slot.slot_id, slot.decision_time
                            ),
                        );
                    }
                }
            }
        }
    }
    for c in &a.receipts {
        if let Some((st, day, slot)) = slot_of(&c.payload.body) {
            if !scheduled.contains(&(st, day)) {
                r.fail(
                    3,
                    "schedule",
                    format!("slot {slot}: no schedule for {st} {day}"),
                );
            }
        }
    }
}

/// Check 3, station days (C040): a `wx.rule_change{kind: station_map}` is
/// the published commitment to which station-days exist, so a whole day with
/// no schedule (and so no commits) is no longer invisible.
///
/// Every station map must be founder-signed ([`station_map`]); one that is
/// not FAILs and sets no coverage. Under the signed map with the latest
/// `effective_from`, every mapped station needs a `wx.schedule` for every
/// climate day from the UTC date of `effective_from` through the newest
/// schedule's climate day in the archive. No station map: nothing to check.
///
/// ponytail: the latest map governs from its effective_from and earlier maps
/// are ignored, so days between an older map and a newer one are not
/// covered; walk each map's [effective_from, next map) when maps change.
pub fn check_station_maps(a: &Archive, founder_pubkey_hex: Option<&str>, r: &mut Report) {
    let mut maps = Vec::new();
    for rec in &a.receipts {
        let Body::RuleChange(rc) = &rec.payload.body else {
            continue;
        };
        if rc.kind != RuleKind::StationMap {
            continue;
        }
        match station_map(rc, founder_pubkey_hex) {
            Ok(m) => maps.push(m),
            Err(e) => r.fail(3, "station_map", format!("{}: {e:#}", receipt_id(rec))),
        }
    }
    let Some((from, map)) = maps.into_iter().max_by_key(|m| m.0) else {
        return;
    };
    let mut scheduled = BTreeSet::new();
    let mut newest = None;
    for rec in &a.receipts {
        if let Body::Schedule(s) = &rec.payload.body {
            let day = (s.climate_day.len() == 10)
                .then(|| unix_ms(&s.climate_day))
                .flatten();
            if let Some(d) = day {
                let d = d.div_euclid(DAY_MS);
                scheduled.insert((s.station.as_str(), d));
                newest = newest.max(Some(d));
            }
        }
    }
    let Some(newest) = newest else {
        return;
    };
    for day in from.div_euclid(DAY_MS)..=newest {
        for st in &map.stations {
            if !scheduled.contains(&(st.station.as_str(), day)) {
                r.fail(
                    3,
                    "schedule",
                    format!(
                        "{} {}: no schedule (station map)",
                        st.station,
                        date_of_day(day)
                    ),
                );
            }
        }
    }
}

const DAY_MS: i64 = 86_400_000;

/// A station map's `effective_from` (unix ms) and stations, if it is
/// founder-signed: `founder_sig` is the lowercase hex of an Ed25519 signature
/// by `founder_pubkey_hex` over the 64 lowercase-hex ASCII bytes of
/// `after_hash` (wx-exec's `verify_rule_change` convention), and `after_hash`
/// is the SHA-256 of the `diff_json` string. `diff_json`'s own
/// `effective_from` must equal the receipt's, since only it is under the
/// founder signature; otherwise the agent could re-sign a founder-signed map
/// with a later `effective_from` and skip coverage. No founder key fails
/// closed.
fn station_map(rc: &RuleChange, founder_pubkey_hex: Option<&str>) -> Result<(i64, StationMap)> {
    let founder = founder_pubkey_hex.context("no --founder-pubkey given to verify founder_sig")?;
    let founder = hex::decode(founder.trim()).context("--founder-pubkey is not hex")?;
    ensure!(
        rc.after_hash.len() == 64
            && rc
                .after_hash
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        "after_hash is not 64 lowercase hex chars"
    );
    ensure!(
        rc.after_hash == sha256_hex(rc.diff_json.as_bytes()),
        "after_hash is not the SHA-256 of diff_json"
    );
    ensure!(!rc.founder_sig.is_empty(), "founder_sig is empty");
    ensure!(
        rc.founder_sig.len() == 128 && rc.founder_sig.bytes().all(|b| b.is_ascii_hexdigit()),
        "founder_sig is not 64 bytes of hex"
    );
    let sig = hex::decode(&rc.founder_sig).context("founder_sig is not hex")?;
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &founder)
        .verify(rc.after_hash.as_bytes(), &sig)
        .map_err(|_| anyhow!("founder_sig does not verify under the founder key"))?;
    let from = unix_ms(&rc.effective_from)
        .with_context(|| format!("bad effective_from {:?}", rc.effective_from))?;
    let map: StationMap = serde_json::from_str(&rc.diff_json).context(
        "diff_json is not {\"effective_from\", \"stations\": [{\"station\", \"slots_rule\", \"close_rule\"}, …]}",
    )?;
    ensure!(
        map.effective_from == rc.effective_from,
        "diff_json effective_from {:?} is not the receipt's effective_from {:?}",
        map.effective_from,
        rc.effective_from
    );
    Ok((from, map))
}

/// `YYYY-MM-DD` of `days` since 1970-01-01: civil_from_days (H. Hinnant),
/// the inverse of the date arithmetic in [`unix_ms`].
fn date_of_day(days: i64) -> String {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// (station, climate_day, slot_id) and signed decision_time of a receipt that
/// carries one: forecast commit, forecast reveal, refusal.
fn slot_decision(b: &Body) -> Option<((&str, &str, &str), &str)> {
    match b {
        Body::ForecastReveal(v) => {
            Some(((&v.station, &v.climate_day, &v.slot_id), &v.decision_time))
        }
        Body::ForecastCommit(v) => {
            Some(((&v.station, &v.climate_day, &v.slot_id), &v.decision_time))
        }
        Body::BetRefused(v) => Some(((&v.station, &v.climate_day, &v.slot_id), &v.decision_time)),
        _ => None,
    }
}

fn reveal_ref(b: &Body) -> Option<&str> {
    match b {
        Body::ForecastReveal(v) => Some(&v.commit_ref),
        Body::BetReveal(v) => Some(&v.commit_ref),
        _ => None,
    }
}

/// Earliest and latest plausible close after UTC midnight ending the climate
/// day: 23:59 local clock time at UTC-4 (EDT) and UTC-10 (Hawaii). Kalshi
/// closes at 23:59 local per series (settlement.md §2.3); the agent signs that
/// instant as `close_utc` in the schedule.
const CLOSE_WINDOW_MS: [i64; 2] = [(3 * 60 + 59) * 60_000, (9 * 60 + 59) * 60_000];

/// Whether `close_utc` is 23:59 local at some US zone on the night `climate_day`
/// ends. The verifier cannot check the agent's time-zone conversion without a
/// tz database, only that the close is no earlier than any real US close.
fn close_plausible(climate_day: &str, close_utc: &str) -> bool {
    let (Some(day), Some(close)) = (
        (climate_day.len() == 10)
            .then(|| unix_ms(climate_day))
            .flatten(),
        unix_ms(close_utc),
    ) else {
        return false;
    };
    let midnight = day + 86_400_000;
    (midnight + CLOSE_WINDOW_MS[0]..=midnight + CLOSE_WINDOW_MS[1]).contains(&close)
}

/// (station, climate_day) -> the close signed in its wx.schedule, unix ms, or
/// None if a schedule's `close_utc` does not parse. Two schedules for one
/// station-day: the later close wins, so check 4 errs towards "too early".
fn closes(a: &Archive) -> BTreeMap<(&str, &str), Option<i64>> {
    let mut m: BTreeMap<(&str, &str), Option<i64>> = BTreeMap::new();
    for rec in &a.receipts {
        let Body::Schedule(s) = &rec.payload.body else {
            continue;
        };
        let close = unix_ms(&s.close_utc);
        m.entry((s.station.as_str(), s.climate_day.as_str()))
            .and_modify(|c| *c = c.zip(close).map(|(x, y)| x.max(y)))
            .or_insert(close);
    }
    m
}

/// Check 4: every forecast and bet commit has exactly one reveal of its kind
/// whose recomputed [`commitment`] is the committed one, revealed after the
/// commit and after the climate day's close (receipt `timestamp`, and TSA
/// genTime where verified). A reveal naming no receipt fails too.
///
/// The close is the `close_utc` signed in that station-day's wx.schedule. No
/// schedule, or a close that does not parse, fails (closed): with no close
/// there is no proof the reveal came after it.
pub fn check_reveals(a: &Archive, gen: &BTreeMap<String, i64>, r: &mut Report) {
    let closes = closes(a);
    for rv in &a.receipts {
        if let Some(cr) = reveal_ref(&rv.payload.body) {
            if a.get(cr).is_none() {
                r.fail(
                    4,
                    "reveal",
                    format!("{}: commit_ref {cr} names no receipt", rv.file),
                );
            }
        }
    }
    for c in &a.receipts {
        // `day` is (station, climate_day): a bet takes its forecast commit's.
        let (want, day, label) = match &c.payload.body {
            Body::ForecastCommit(fc) => (
                &fc.commitment,
                Some((fc.station.as_str(), fc.climate_day.as_str())),
                fc.slot_id.clone(),
            ),
            Body::BetCommit(bc) => {
                let day = a
                    .get(&bc.reveal_commit_ref)
                    .and_then(|f| match &f.payload.body {
                        Body::ForecastCommit(fc) => {
                            Some((fc.station.as_str(), fc.climate_day.as_str()))
                        }
                        _ => None,
                    });
                (&bc.commitment, day, format!("bet {}", bc.client_order_id))
            }
            _ => continue,
        };
        let reveals: Vec<&Rec> = a
            .receipts
            .iter()
            .filter(|x| reveal_ref(&x.payload.body) == Some(c.hash.as_str()))
            .collect();
        let [rv] = reveals[..] else {
            r.fail(
                4,
                "reveal",
                format!("{label}: commit has {} reveals, want 1", reveals.len()),
            );
            continue;
        };
        match commitment(&rv.payload) {
            Ok(h) if &h == want => {}
            Ok(h) => r.fail(
                4,
                "reveal",
                format!("{label}: unmatched commitment, reveal hashes to {h}, commit says {want}"),
            ),
            Err(e) => r.fail(4, "reveal", format!("{label}: {e:#}")),
        }
        let agrees = match (&c.payload.body, &rv.payload.body) {
            (Body::ForecastCommit(fc), Body::ForecastReveal(fr)) => {
                (&fc.station, &fc.climate_day, &fc.slot_id) == (&fr.station, &fr.climate_day, &fr.slot_id)
                    // Same instant as unix ms; an unparseable one never agrees.
                    && unix_ms(&fc.decision_time).is_some()
                    && unix_ms(&fc.decision_time) == unix_ms(&fr.decision_time)
            }
            (Body::BetCommit(bc), Body::BetReveal(br)) => bc.client_order_id == br.client_order_id,
            _ => false,
        };
        if !agrees {
            r.fail(
                4,
                "reveal",
                format!("{label}: reveal is not for this commit's slot or order"),
            );
        }
        let ts = rv.raw["timestamp"].as_str().unwrap_or_default();
        if rv.ts_ms <= c.ts_ms {
            r.fail(
                4,
                "reveal",
                format!("{label}: revealed at {ts}, not after its commit"),
            );
        }
        let Some((st, d)) = day else {
            r.fail(
                4,
                "reveal",
                format!("{label}: no climate day to close (bet names no forecast commit)"),
            );
            continue;
        };
        let close = match closes.get(&(st, d)) {
            Some(Some(close)) => *close,
            Some(None) => {
                r.fail(
                    4,
                    "reveal",
                    format!("{label}: close_utc of the {st} {d} schedule does not parse"),
                );
                continue;
            }
            None => {
                r.fail(
                    4,
                    "reveal",
                    format!("{label}: no wx.schedule for {st} {d} to take the close from"),
                );
                continue;
            }
        };
        if rv.ts_ms <= close {
            r.fail(
                4,
                "reveal",
                format!("{label}: early reveal at {ts}, before the close of {d}"),
            );
        }
        if let Some(&g) = gen.get(&rv.hash) {
            if g * 1000 <= close {
                r.fail(
                    4,
                    "reveal",
                    format!("{label}: reveal genTime {g} is before the close"),
                );
            }
            if gen.get(&c.hash).is_some_and(|&cg| g <= cg) {
                r.fail(
                    4,
                    "reveal",
                    format!("{label}: reveal genTime {g} is not after the commit's"),
                );
            }
        }
    }
}

/// Check 5, an invariant over every bet rather than a walk from the Accepts
/// (each path-based walk left some bet outside it):
/// - every wx.bet.commit, whatever its slot's decision, resolves to its
///   forecast reveal, which must be an Accept; it has a wx.bet.reveal on the
///   Accept's side; and that forecast reveal is listed by exactly one
///   settlement (none: unsettled bet; two or more: settled twice);
/// - every forecast reveal is listed by exactly one settlement, and every
///   Accept has at least one bet commit;
/// - a settlement lists only forecast reveals of its own station and day;
/// - every settlement's net = gross - fee, and the sum of net_cents equals
///   ledger.json. P&L is tied to the bets: a settlement with no Accept (or
///   outcome no_bet) must be all zero, and for Buy bets gross and fee are
///   recomputed from the fills of EVERY bet on its Accepts (see [`fill_pnl`]),
///   so the ledger total is the sum of recomputed values.
pub fn check_settlements(a: &Archive, r: &mut Report) {
    let bodies = || a.receipts.iter().map(|x| (x, &x.payload.body));
    let fail = |r: &mut Report, msg: String| r.fail(5, "settlement", msg);
    // How many settlements list each reveal: an Accept settled twice would
    // count its P&L twice in the sum below.
    let mut listed: BTreeMap<&str, usize> = BTreeMap::new();
    for (_, b) in bodies() {
        if let Body::Settlement(s) = b {
            for rf in &s.reveal_refs {
                *listed.entry(rf.as_str()).or_default() += 1;
            }
        }
    }
    let what = |fr: &ForecastReveal| match fr.decision {
        Decision::Accept { .. } => format!("Accept {}", fr.slot_id),
        _ => format!("slot {}", fr.slot_id),
    };
    // Forecast reveal hash -> the bet reveal of each bet commit on it (None
    // where the bet was never revealed; that is failed here, once).
    let mut bets: BTreeMap<&str, Vec<Option<&BetReveal>>> = BTreeMap::new();
    for (bc_rec, b) in bodies() {
        let Body::BetCommit(bc) = b else { continue };
        let coid = &bc.client_order_id;
        let Some((fr_rec, fr)) = bodies().find_map(|(x, b)| match b {
            Body::ForecastReveal(fr) if fr.commit_ref == bc.reveal_commit_ref => Some((x, fr)),
            _ => None,
        }) else {
            fail(
                r,
                format!("bet {coid}: no wx.forecast.reveal for its forecast commit"),
            );
            continue;
        };
        let what = what(fr);
        let br = bodies().find_map(|(_, b)| match b {
            Body::BetReveal(br) if br.commit_ref == bc_rec.hash => Some(br),
            _ => None,
        });
        match (&fr.decision, br) {
            (Decision::Accept { .. }, None) => fail(r, format!("{what}: no wx.bet.reveal")),
            (Decision::Accept { side, .. }, Some(br)) if br.side != *side => fail(
                r,
                format!(
                    "{what}: bet {coid} side {:?}, Accept side {side:?}",
                    br.side
                ),
            ),
            (Decision::Accept { .. }, _) => {}
            _ => fail(r, format!("{what}: bet {coid} on a non-Accept slot")),
        }
        bets.entry(fr_rec.hash.as_str()).or_default().push(br);
        // Kept in the old wording too, so a reader grepping either finds it.
        match listed.get(fr_rec.hash.as_str()).copied().unwrap_or(0) {
            0 => fail(
                r,
                format!("{what}: missing settlement: unsettled bet {coid}"),
            ),
            1 => {}
            n => fail(
                r,
                format!("{what}: listed {n} times across settlements: bet {coid} settled twice"),
            ),
        }
    }
    // Reveals with a bet were counted per bet above; these are the rest.
    for (rf, n) in &listed {
        if *n < 2 || bets.contains_key(rf) {
            continue;
        }
        let what = match a.get(rf).map(|x| &x.payload.body) {
            Some(Body::ForecastReveal(fr)) => what(fr),
            _ => format!("reveal {rf}"),
        };
        fail(r, format!("{what}: listed {n} times across settlements"));
    }
    for (rec, b) in bodies() {
        let Body::ForecastReveal(fr) = b else {
            continue;
        };
        if bets.contains_key(rec.hash.as_str()) {
            continue;
        }
        if matches!(fr.decision, Decision::Accept { .. }) {
            fail(r, format!("Accept {}: no wx.bet.commit", fr.slot_id));
        }
        if !listed.contains_key(rec.hash.as_str()) {
            fail(r, format!("{}: missing settlement", what(fr)));
        }
    }
    let mut sum: i128 = 0;
    for (_, b) in bodies() {
        let Body::Settlement(s) = b else { continue };
        let p = &s.pnl;
        if p.gross_cents.checked_sub(p.fee_cents) != Some(p.net_cents) {
            fail(
                r,
                format!(
                    "{} {}: net_cents {} != gross {} - fee {}",
                    s.station, s.climate_day, p.net_cents, p.gross_cents, p.fee_cents
                ),
            );
        }
        let label = format!("{} {}", s.station, s.climate_day);
        let mut accepts = Vec::new();
        for rf in &s.reveal_refs {
            let Some(Body::ForecastReveal(fr)) = a.get(rf).map(|x| &x.payload.body) else {
                fail(
                    r,
                    format!("{label}: reveal_ref {rf} is not a forecast reveal"),
                );
                continue;
            };
            if (&fr.station, &fr.climate_day) != (&s.station, &s.climate_day) {
                fail(
                    r,
                    format!(
                        "{label}: reveal_ref {} is for {} {}, not this settlement's station-day",
                        fr.slot_id, fr.station, fr.climate_day
                    ),
                );
            }
            if matches!(fr.decision, Decision::Accept { .. }) {
                accepts.push((rf.as_str(), fr));
            }
        }
        let zero = p.gross_cents == 0 && p.fee_cents == 0 && p.net_cents == 0;
        if !zero && s.outcome == Outcome::NoBet {
            fail(r, format!("{label}: outcome no_bet with non-zero P&L"));
        } else if !zero && accepts.is_empty() {
            fail(r, format!("{label}: non-zero P&L with no Accept"));
        } else if accepts.is_empty()
            && matches!(s.outcome, Outcome::Won | Outcome::Lost | Outcome::Mixed)
        {
            // Check 6 says the same for NWS; this covers TWC, where nothing
            // else re-derives the outcome.
            fail(
                r,
                format!("{label}: outcome {:?} with no Accept", s.outcome),
            );
        } else if !accepts.is_empty() && s.outcome == Outcome::NoBet {
            // A settled Accept is a bet: "no bet, 0¢" would hide a loss where
            // check 6 does not re-derive the outcome (TWC).
            fail(
                r,
                format!("{label}: outcome no_bet but an Accept is settled"),
            );
        } else if s.outcome == Outcome::Mixed && s.observed_high_f.is_none() {
            // Mixed P&L needs each slot's own result, which only an observed
            // high re-derives; a TWC settlement states none.
            fail(
                r,
                format!("{label}: outcome mixed cannot be recomputed without observed_high_f"),
            );
        } else if !accepts.is_empty() {
            // Every bet reveal on every listed Accept, two orders on one Accept
            // included. An Accept with no bet, or a bet with no reveal, is
            // already failed above; recomputing without it would only add a
            // second, misleading line.
            // With an observed high, a won/lost/mixed settlement is priced per
            // Accept from that slot's own result (check 6 checks the stated
            // outcome); void, and TWC won/lost, apply the stated outcome.
            let per_slot = s
                .observed_high_f
                .filter(|_| matches!(s.outcome, Outcome::Won | Outcome::Lost | Outcome::Mixed));
            let fills: Option<Vec<(Outcome, Vec<&BetReveal>)>> = accepts
                .iter()
                .map(|(rf, fr)| {
                    let o = per_slot
                        .and_then(|t| accept_outcome(fr, t))
                        .unwrap_or(s.outcome);
                    bets.get(rf)
                        .and_then(|v| v.iter().copied().collect::<Option<Vec<_>>>())
                        .map(|v| (o, v))
                })
                .collect();
            // A Sell's P&L depends on the position it closes, which no receipt
            // states, so it fails closed until Sell P&L is specified. The Buy
            // recompute is skipped then: one clear line, not a second one.
            let sells: Vec<&str> = fills
                .iter()
                .flatten()
                .flat_map(|(_, v)| v)
                .filter(|b| b.action == Action::Sell)
                .map(|b| b.client_order_id.as_str())
                .collect();
            for coid in &sells {
                fail(
                    r,
                    format!("{label}: bet {coid} is a Sell; P&L cannot be recomputed"),
                );
            }
            if let Some(slots) = fills.filter(|_| sells.is_empty()) {
                let pnl = slots.iter().try_fold((0i64, 0i64), |(g, f), (o, v)| {
                    let (dg, df) = fill_pnl(v, o)?;
                    Some((g.checked_add(dg)?, f.checked_add(df)?))
                });
                match pnl {
                    Some((g, f)) if (g, f) == (p.gross_cents, p.fee_cents) => {}
                    Some((g, f)) => fail(
                        r,
                        format!(
                            "{label}: gross {} fee {}, recomputed from fills gross {g} fee {f}",
                            p.gross_cents, p.fee_cents
                        ),
                    ),
                    None => fail(r, format!("{label}: P&L from fills overflows")),
                }
            }
        }
        sum += i128::from(p.net_cents);
    }
    if sum != i128::from(a.ledger_net_cents) {
        fail(
            r,
            format!(
                "P&L sum of net_cents {sum} != ledger.json net_cents {}",
                a.ledger_net_cents
            ),
        );
    }
}

/// (gross, fee) in cents of Buy bets from their fills: won pays
/// count × (100 − price) per fill, lost loses count × price, void is 0 gross;
/// fee is the fills' fees either way. None on overflow.
fn fill_pnl(bets: &[&BetReveal], outcome: &Outcome) -> Option<(i64, i64)> {
    let (mut gross, mut fee) = (0i64, 0i64);
    for f in bets.iter().flat_map(|b| &b.fills) {
        let (n, px) = (i64::from(f.count), i64::from(f.price_cents));
        let g = match outcome {
            Outcome::Won => n.checked_mul(100 - px)?,
            Outcome::Lost => n.checked_mul(px)?.checked_neg()?,
            // Mixed never gets here: check_settlements resolves it per slot.
            Outcome::Void | Outcome::NoBet | Outcome::Mixed => 0,
        };
        gross = gross.checked_add(g)?;
        fee = fee.checked_add(i64::from(f.fee_cents))?;
    }
    Some((gross, fee))
}

/// Index of the bracket holding `t`°F: `lo` and `hi` inclusive, None open.
fn bracket_hit(fr: &ForecastReveal, t: i32) -> Option<u32> {
    fr.brackets
        .iter()
        .position(|b| b.lo.is_none_or(|lo| lo <= t) && b.hi.is_none_or(|hi| t <= hi))
        .and_then(|i| u32::try_from(i).ok())
}

/// Won or Lost for an Accept reveal at observed high `t`°F: Yes wins iff the
/// hit bracket is the bet's, No wins iff it is not. None for a non-Accept.
fn accept_outcome(fr: &ForecastReveal, t: i32) -> Option<Outcome> {
    let Decision::Accept {
        side, bracket_idx, ..
    } = fr.decision
    else {
        return None;
    };
    let won = (bracket_hit(fr, t) == Some(bracket_idx)) == (side == Side::Yes);
    Some(if won { Outcome::Won } else { Outcome::Lost })
}

/// Check 6: every settlement's source is its station's configured one; for
/// NWS settlements, recompute bracket_hit_idx from observed_high_f and each
/// referenced reveal's brackets (lo and hi inclusive, None open), and the
/// outcome from each Accept's side: Yes wins iff the hit bracket is the bet's,
/// No wins iff it is not. `void` is accepted as is (the venue voided the
/// market); a void settlement over an Accept prints `NOTE void: <station>
/// <day>`, NWS or TWC, because its bet's gross is dropped (fill_pnl counts
/// void as 0 gross). With no Accept the outcome is no_bet.
///
/// Not recomputed here: fetching the CLI itself (no network offline; the
/// verifier takes observed_high_f as stated and the `cli` sha256 names the
/// bytes to check it against).
///
/// `scores` are recomputed (D8) as the day mean: over every forecast reveal
/// in the settlement's `reveal_refs` (in order), each reveal's bracket Brier
/// (bp, over `fr.brackets`) and log score (milli-nats) at observed_high_f,
/// each mean rounded half away from zero. The one function that defines it,
/// [`crate::score::day_scores`], is the one the recorder (wx-run settle)
/// calls too, so the two cannot drift apart.
///
/// Which settlements are NWS comes from `stations` (the verifier's config),
/// not from the agent's `settlement_source` label: a label that disagrees,
/// or a station the config does not name, fails, and an NWS-settled station
/// is recomputed whatever it is labelled. The config names the market's
/// source, so the label compared is `market_settlement_source` when set
/// (`settlement_source` is then how we settled paper). Any other settlement
/// carrying both `observed_high_f` and `cli` is recomputed too: a TWC
/// market settled on the NWS CLI is checked like an NWS one.
pub fn check_nws(a: &Archive, stations: &Stations, r: &mut Report) {
    for rec in &a.receipts {
        let Body::Settlement(s) = &rec.payload.body else {
            continue;
        };
        let label = format!("{} {}", s.station, s.climate_day);
        let Some(nws) = stations.get(&s.station).map(|c| c.nws_settled) else {
            r.fail(
                6,
                "nws",
                format!("{label}: source mismatch: station not in the --stations file"),
            );
            continue;
        };
        let want = if nws {
            SettlementSource::NwsCli
        } else {
            SettlementSource::Twc
        };
        let labelled = s.market_settlement_source.unwrap_or(s.settlement_source);
        if labelled != want {
            r.fail(
                6,
                "nws",
                format!(
                    "{label}: source mismatch: labelled {labelled:?}, the station config says {want:?}"
                ),
            );
        }
        // `void` is taken as stated for NWS and TWC alike, and fill_pnl counts
        // it as 0 gross, so a void over an Accept drops that bet's gross. Say
        // so for each one rather than fail it: the venue can really void.
        if s.outcome == Outcome::Void
            && s.reveal_refs.iter().any(|rf| {
                matches!(
                    a.get(rf).map(|x| &x.payload.body),
                    Some(Body::ForecastReveal(fr)) if matches!(fr.decision, Decision::Accept { .. })
                )
            })
        {
            r.notes
                .push(format!("NOTE void: {} {}", s.station, s.climate_day));
        }
        // A TWC settlement with no CLI high is our word (check 7 notes it).
        if !nws && (s.observed_high_f.is_none() || s.cli.is_none()) {
            continue;
        }
        let Some(t) = s.observed_high_f else {
            // A day voided at the backstop with no final CLI and no METAR
            // reading has no high to state, so nothing here to recompute.
            // Anything else claiming an outcome or a hit must state one.
            let bare_void =
                s.outcome == Outcome::Void && s.cli.is_none() && s.bracket_hit_idx.is_none();
            if !bare_void {
                r.fail(
                    6,
                    "nws",
                    format!("{label}: NWS settlement without observed_high_f"),
                );
            }
            continue;
        };
        let mut results = Vec::new();
        let mut reveals = Vec::new();
        for rf in &s.reveal_refs {
            let Some(Body::ForecastReveal(fr)) = a.get(rf).map(|x| &x.payload.body) else {
                r.fail(
                    6,
                    "nws",
                    format!("{label}: reveal_ref {rf} is not a forecast reveal"),
                );
                continue;
            };
            let hit = bracket_hit(fr, t);
            if hit != s.bracket_hit_idx {
                r.fail(
                    6,
                    "nws",
                    format!(
                        "{label}: bracket_hit_idx {:?}, recomputed {hit:?} from {t}°F",
                        s.bracket_hit_idx
                    ),
                );
            }
            results.extend(accept_outcome(fr, t));
            reveals.push(fr);
        }
        let got = crate::score::day_scores(reveals, Some(t));
        let (b, l) = (i64::from(got.brier_bp), got.log_score_milli);
        if (i64::from(s.scores.brier_bp), s.scores.log_score_milli) != (b, l) {
            r.fail(
                6,
                "nws",
                format!(
                    "{label}: scores brier_bp {} log_score_milli {}, recomputed {b} {l}",
                    s.scores.brier_bp, s.scores.log_score_milli
                ),
            );
        }
        // Each Accept has its own bracket and side, so the station-day's
        // outcome is won or lost only when every slot agrees, else mixed.
        let want = match (
            results.contains(&Outcome::Won),
            results.contains(&Outcome::Lost),
        ) {
            (true, true) => Some(Outcome::Mixed),
            (true, false) => Some(Outcome::Won),
            (false, true) => Some(Outcome::Lost),
            (false, false) => None,
        };
        if let Some(want) = want.filter(|w| s.outcome != *w && s.outcome != Outcome::Void) {
            r.fail(
                6,
                "nws",
                format!("{label}: outcome {:?}, recomputed {want:?}", s.outcome),
            );
        }
        if want.is_none() && !matches!(s.outcome, Outcome::NoBet | Outcome::Void) {
            r.fail(
                6,
                "nws",
                format!("{label}: outcome {:?} with no Accept", s.outcome),
            );
        }
    }
}

/// Check 7: TWC settlements without a CLI high are our word; noted, never
/// failed. A TWC market that paper settled on the NWS CLI is noted as such
/// (check 6 recomputed it).
pub fn check_twc(a: &Archive, r: &mut Report) {
    for rec in &a.receipts {
        let Body::Settlement(s) = &rec.payload.body else {
            continue;
        };
        let (st, day) = (&s.station, &s.climate_day);
        if s.market_settlement_source == Some(SettlementSource::Twc)
            && s.settlement_source == SettlementSource::NwsCli
        {
            r.notes.push(format!(
                "NOTE market source twc, paper settled on the NWS CLI ({st} {day})"
            ));
        } else if s.settlement_source == SettlementSource::Twc
            && (s.observed_high_f.is_none() || s.cli.is_none())
        {
            // With both, check 6 did recompute it: "not recomputed" would lie.
            r.notes.push(format!(
                "NOTE twc: attested by us, not recomputed ({st} {day})"
            ));
        }
    }
}

/// Check 8: every reveal input is in the manifest by (source, product_id,
/// sha256), first seen before its slot's scheduled decision_time (not the
/// reveal's own field, which the agent could move later). A reveal with no
/// scheduled slot is skipped: check 3 already fails it.
///
/// A refusal that lists `evidence_inputs` (the health gate's) must have
/// `evidence_hash` = sha256 of them joined by "\n"; one with none (every
/// other rule's) hashes evidence this archive does not hold, so is skipped.
pub fn check_inputs(a: &Archive, r: &mut Report) {
    for rec in &a.receipts {
        if let Body::BetRefused(b) = &rec.payload.body {
            let got = sha256_hex(b.evidence_inputs.join("\n").as_bytes());
            if !b.evidence_inputs.is_empty() && got != b.evidence_hash {
                r.fail(
                    8,
                    "inputs",
                    format!(
                        "{}: evidence_hash {} is not sha256 of its evidence_inputs ({got})",
                        b.slot_id, b.evidence_hash
                    ),
                );
            }
            continue;
        }
        let Body::ForecastReveal(fr) = &rec.payload.body else {
            continue;
        };
        let Some(due) = a.receipts.iter().find_map(|x| match &x.payload.body {
            Body::Schedule(s) if (&s.station, &s.climate_day) == (&fr.station, &fr.climate_day) => {
                s.slots
                    .iter()
                    .find(|sl| sl.slot_id == fr.slot_id)
                    .map(|sl| &sl.decision_time)
            }
            _ => None,
        }) else {
            continue;
        };
        // Times compare as unix ms, never as strings: a non-canonical one is
        // refused by name, not sorted.
        let Some(due_ms) = unix_ms(due) else {
            r.fail(
                8,
                "inputs",
                format!(
                    "{}: decision_time {due} is not a canonical RFC 3339 UTC timestamp",
                    fr.slot_id
                ),
            );
            continue;
        };
        for i in &fr.inputs {
            let m = a.manifest.iter().find(|m| {
                (&m.source, &m.product_id, &m.sha256) == (&i.source, &i.product_id, &i.sha256)
            });
            let what = format!(
                "{}: input {}/{} {}",
                fr.slot_id, i.source, i.product_id, i.sha256
            );
            match m {
                None => r.fail(8, "inputs", format!("{what} not in manifest")),
                Some(m) => match unix_ms(&m.first_seen) {
                    None => r.fail(
                        8,
                        "inputs",
                        format!(
                            "{what}: first_seen {} is not a canonical RFC 3339 UTC timestamp",
                            m.first_seen
                        ),
                    ),
                    Some(seen) if seen >= due_ms => r.fail(
                        8,
                        "inputs",
                        format!(
                            "{what} first seen {}, not before decision_time {due}",
                            m.first_seen
                        ),
                    ),
                    Some(_) => {}
                },
            }
        }
    }
}

/// Lowercase hex SHA-256, the form the manifest and the peak block use.
fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(ring::digest::digest(&ring::digest::SHA256, bytes))
}

/// `YYYY-MM-DD` as wx-obs's [`ClimateDay`]; None unless canonical.
fn climate_day(s: &str) -> Option<ClimateDay> {
    unix_ms(s).filter(|_| s.len() == 10)?;
    Some(ClimateDay {
        year: s[0..4].parse().ok()?,
        month: s[5..7].parse().ok()?,
        day: s[8..10].parse().ok()?,
    })
}

/// Check 9 ("peak honest"): a reveal's peak block states what was knowable at
/// its slot's scheduled decision_time, from bytes anyone can re-read in
/// `products/`. Per reveal with a peak (one without is skipped, so receipts
/// from before C020 stay valid):
///
/// - (b) every listed observation is a manifest `metar` row stating the same
///   first_seen, and it and `latest_cli_seen` were first seen strictly
///   before decision_time per the manifest (unix ms, non-canonical refused
///   by name);
/// - each listed observation's bytes hash to its sha256 and hold a report
///   line hashing to its `line_sha256`;
/// - (a) wx-obs's own `running_max` over the listed bodies, as of the
///   decision, equals `running_max_f`, and its max tenths °C converted by
///   wx-obs's `tenths_c_to_tenths_f` equals `running_max_tenths_f` (no
///   conversion is reimplemented here);
/// - (c) every manifest `metar` body first seen before the decision that
///   carries a reading for this station-day is listed, so a higher reading
///   cannot be left out;
/// - (d) a `latest_cli_seen` that is the day's final CLI is a NOTE, `outcome
///   known`: the decision may be honest but is not a forecast, and is never
///   hidden. The manifest's `cli` and `cli_prelim` rows are scanned too, not
///   just the field: a final CLI for the day first seen before the decision
///   is the same NOTE whatever `latest_cli_seen` says, and one for the day
///   first seen after `latest_cli_seen` (or any, when it is null and none is
///   final) fails, since "latest" is then false.
///
/// (a) and (c) need the station's standard-time offset, which comes from the
/// verifier's `--stations` file, never the archive; without it they are
/// noted and skipped, not guessed.
///
/// Every bet reveal is also tied to the peak block it acted on, see
/// [`check_bet_peaks`].
pub fn check_peaks(a: &Archive, stations: &Stations, r: &mut Report) {
    check_bet_peaks(a, r);
    for rec in &a.receipts {
        let Body::ForecastReveal(fr) = &rec.payload.body else {
            continue;
        };
        let Some(peak) = fr.peak.as_deref() else {
            continue;
        };
        let slot = &fr.slot_id;
        // The scheduled time, found as check 8 does; no slot fails check 3.
        let Some(due) = a.receipts.iter().find_map(|x| match &x.payload.body {
            Body::Schedule(s) if (&s.station, &s.climate_day) == (&fr.station, &fr.climate_day) => {
                s.slots
                    .iter()
                    .find(|sl| &sl.slot_id == slot)
                    .map(|sl| &sl.decision_time)
            }
            _ => None,
        }) else {
            continue;
        };
        let Some(due_ms) = unix_ms(due) else {
            r.fail(
                9,
                "peak",
                format!("{slot}: decision_time {due} is not a canonical RFC 3339 UTC timestamp"),
            );
            continue;
        };
        let before = |r: &mut Report, what: &str, seen: &str| -> Option<i64> {
            match unix_ms(seen) {
                None => r.fail(
                    9,
                    "peak",
                    format!("{slot}: {what}: first_seen {seen} is not a canonical RFC 3339 UTC timestamp"),
                ),
                Some(t) if t >= due_ms => r.fail(
                    9,
                    "peak",
                    format!("{slot}: {what} first seen {seen}, not before decision_time {due}"),
                ),
                Some(t) => return Some(t),
            }
            None
        };

        // (b), the line hashes, and the bodies (a) recomputes from.
        let mut bodies = Vec::new();
        for o in &peak.obs {
            let what = format!("obs {} {}", o.product_id, o.sha256);
            // The manifest, not the peak block, is the record of first_seen
            // (as in check 8): the agent could move its own field earlier.
            let Some(m) = a.manifest.iter().find(|m| {
                m.source == "metar" && (&m.product_id, &m.sha256) == (&o.product_id, &o.sha256)
            }) else {
                r.fail(9, "peak", format!("{slot}: {what} not in manifest"));
                continue;
            };
            if unix_ms(&o.first_seen).is_none() || unix_ms(&o.first_seen) != unix_ms(&m.first_seen)
            {
                r.fail(
                    9,
                    "peak",
                    format!(
                        "{slot}: {what}: first_seen {} in the peak block, {} in the manifest",
                        o.first_seen, m.first_seen
                    ),
                );
            }
            let seen = before(r, &what, &m.first_seen);
            let body = match a.product(&o.sha256) {
                Ok(b) => b,
                Err(e) => {
                    r.fail(9, "peak", format!("{slot}: {what}: {e:#}"));
                    continue;
                }
            };
            if !crate::obs::metar::report_lines(&body)
                .any(|l| sha256_hex(l.as_bytes()) == o.line_sha256)
            {
                r.fail(
                    9,
                    "peak",
                    format!(
                        "{slot}: {what}: no report line hashes to line_sha256 {}",
                        o.line_sha256
                    ),
                );
            }
            if let Some(t) = seen {
                bodies.push((t.div_euclid(1000), body));
            }
        }
        if let Some(c) = &peak.latest_cli_seen {
            let what = format!("latest_cli_seen {}", c.product_id);
            if before(r, &what, &c.first_seen).is_some() {
                check_cli_final(a, fr, c, r);
            }
        }
        // No CLI for climate day D exists before D began locally, which is
        // after D-1 00:00 UTC at any UTC offset; known before the lst offset
        // is looked up. An unparsable day keeps no bound (it fails below).
        let cli_from_ms = unix_ms(&fr.climate_day).map_or(i64::MIN, |t| t - 86_400_000);
        check_cli_manifest(a, fr, peak, cli_from_ms..due_ms, r);

        let Some(day) = climate_day(&fr.climate_day) else {
            r.fail(
                9,
                "peak",
                format!("{slot}: climate_day {} is not YYYY-MM-DD", fr.climate_day),
            );
            continue;
        };
        let Some(off) = stations.get(&fr.station).and_then(|c| c.lst_offset_hours) else {
            r.notes.push(format!(
                "NOTE peak: {slot}: no lst_offset_hours for {} in --stations, running max and completeness not recomputed",
                fr.station
            ));
            continue;
        };
        // running_max admits first-seen <= as_of in whole seconds; the last
        // second before the decision admits exactly the reports first seen
        // strictly before it, at ms resolution.
        let as_of = (due_ms - 1).div_euclid(1000);
        let rm = |bodies: &[(i64, &str)]| {
            crate::obs::running_max::running_max(&fr.station, day, as_of, bodies, off)
        };

        // (a) Only when every listed body was readable and in time: a partial
        // recompute would fail for a reason already reported above.
        if bodies.len() == peak.obs.len() {
            let refs: Vec<(i64, &str)> = bodies.iter().map(|(t, b)| (*t, b.as_str())).collect();
            let rec = rm(&refs);
            let got = rec.max_f;
            if got != peak.running_max_f {
                r.fail(
                    9,
                    "peak",
                    format!(
                        "{slot}: running_max_f {:?}, recomputed {got:?} from the listed obs",
                        peak.running_max_f
                    ),
                );
            }
            // Checked on its own: a right whole °F does not vouch for the
            // tenths, which the writer must round as tenths_c_to_tenths_f does.
            let got = rec
                .max_tenths_c
                .map(crate::obs::convert::tenths_c_to_tenths_f);
            if got != peak.running_max_tenths_f {
                r.fail(
                    9,
                    "peak",
                    format!(
                        "{slot}: running_max_tenths_f {:?}, recomputed {got:?} from the listed obs",
                        peak.running_max_tenths_f
                    ),
                );
            }
        }

        // (c) A body "carries a reading" when running_max finds one in it for
        // this station-day by itself: wx-obs's own day window, gate and max
        // groups, not a second parser here.
        //
        // A body first seen before the climate day began cannot hold one of
        // its readings: running_max places a report no later than its fetch
        // plus 10 minutes of skew. The hour of margin covers that skew and
        // more, so those rows are skipped unread.
        // ponytail: still a scan of every manifest row per reveal, and every
        // metar body first seen in the window is read, other stations'
        // included: product_id is not bound to the body's station, so it
        // cannot be trusted to skip one. Each body is read once per verify
        // (Archive::product caches); index the manifest by first_seen if the
        // row scan itself gets slow.
        // `day` parsed, so unix_ms (its UTC midnight) is Some.
        let window_ms = unix_ms(&fr.climate_day)
            .unwrap_or(i64::MIN)
            .saturating_sub((i64::from(off) + 1) * 3_600_000);
        for m in &a.manifest {
            if m.source != "metar" || peak.obs.iter().any(|o| o.sha256 == m.sha256) {
                continue;
            }
            let Some(seen) = unix_ms(&m.first_seen).filter(|&t| (window_ms..due_ms).contains(&t))
            else {
                continue;
            };
            let what = format!("manifest metar {} {}", m.product_id, m.sha256);
            match a.product(&m.sha256) {
                Err(e) => r.fail(9, "peak", format!("{slot}: {what}: {e:#}")),
                Ok(body) => {
                    if rm(&[(seen.div_euclid(1000), &body)]).max_f.is_some() {
                        r.fail(
                            9,
                            "peak",
                            format!(
                                "{slot}: {what} first seen {}, before decision_time {due}, \
                                 holds a {} reading for {} and is not in peak.obs",
                                m.first_seen, fr.station, fr.climate_day
                            ),
                        );
                    }
                }
            }
        }
    }
}

/// Part of check 9: a bet reveal against the forecast reveal it acted on
/// (bet reveal -> its bet commit -> that commit's forecast commit -> the
/// forecast reveal of that commit, as check 5 resolves it):
///
/// - a `peak_ref` must be [`peak_hash`] of that reveal's peak block, and must
///   not name a block when the reveal has none;
/// - when the reveal has a peak block, the bet's `limit_cents` / `count` must
///   equal its `limit_price_cents` / `count`. A null one is not compared: the
///   block may predate the order (docs/receipts.md: "or null").
///
/// A `peak_ref` of None is not a failure (bets from before C020 have none).
/// A bet whose commit or forecast reveal cannot be found is skipped here;
/// checks 4 and 5 fail it.
fn check_bet_peaks(a: &Archive, r: &mut Report) {
    for rec in &a.receipts {
        let Body::BetReveal(br) = &rec.payload.body else {
            continue;
        };
        let Some(Body::BetCommit(bc)) = a.get(&br.commit_ref).map(|x| &x.payload.body) else {
            continue;
        };
        let Some(fr) = a.receipts.iter().find_map(|x| match &x.payload.body {
            Body::ForecastReveal(fr) if fr.commit_ref == bc.reveal_commit_ref => Some(fr),
            _ => None,
        }) else {
            continue;
        };
        let what = format!("{}: bet {}", fr.slot_id, br.client_order_id);
        let Some(peak) = fr.peak.as_deref() else {
            if let Some(pr) = &br.peak_ref {
                r.fail(
                    9,
                    "peak",
                    format!("{what}: peak_ref {pr} names a peak block, but the forecast reveal has none"),
                );
            }
            continue;
        };
        if let Some(pr) = &br.peak_ref {
            match peak_hash(peak) {
                Ok(h) if &h == pr => {}
                Ok(h) => r.fail(
                    9,
                    "peak",
                    format!("{what}: peak_ref {pr}, but the forecast reveal's peak hashes to {h}"),
                ),
                Err(e) => r.fail(9, "peak", format!("{what}: peak_hash: {e:#}")),
            }
        }
        for (field, peak_v, bet_v) in [
            ("limit_cents", peak.limit_price_cents, br.limit_cents),
            ("count", peak.count, br.count),
        ] {
            if let Some(pv) = peak_v.filter(|&pv| pv != bet_v) {
                r.fail(
                    9,
                    "peak",
                    format!("{what}: {field} {bet_v}, peak block says {pv}"),
                );
            }
        }
    }
}

/// Check 10 (C061): the market-implied block (C060). It is derived from the
/// Kalshi book at decision time, and the book is never archived, so nothing
/// here can recompute it: each reveal carrying one gets a `NOTE derived`
/// saying so, never a failure for that alone. What can be checked is that
/// the block is a coherent set of probabilities:
///
/// - (a) every `p_bp` is at most 10000;
/// - (b) `bracket_p_bp` is a partition: its p_bp sum to 10000 ± 1 (the
///   rounding of each bracket to whole bp), and an empty one fails;
/// - (c) `p_gap_bp` has k_f strictly increasing and p_bp non-increasing,
///   since P(exceed by >= k) cannot rise with k;
/// - (d) a bet's `market_implied_ref` is the [`market_implied_hash`] of its
///   forecast reveal's block, found as [`check_bet_peaks`] finds the peak;
/// - (e) `computed_at` is a canonical RFC 3339 UTC timestamp within
///   [`MARKET_IMPLIED_LEAD_SECS`] before to [`MARKET_IMPLIED_LAG_SECS`] after
///   (inclusive) the slot's scheduled decision_time, taken from the Schedule,
///   not the reveal, whose decision_time the agent writes itself.
///
/// A reveal without the block is skipped (receipts from before C060), as is
/// a bet with no `market_implied_ref`.
pub fn check_market_implied(a: &Archive, r: &mut Report) {
    for rec in &a.receipts {
        let Body::ForecastReveal(fr) = &rec.payload.body else {
            continue;
        };
        let Some(m) = fr.market_implied.as_deref() else {
            continue;
        };
        let slot = &fr.slot_id;
        r.notes.push(format!(
            "NOTE derived: {slot}: market_implied is derived from the Kalshi book, which is not archived; checked for internal consistency only, not against Kalshi"
        ));
        let ps = m
            .bracket_p_bp
            .iter()
            .map(|b| (format!("bracket {}", b.ticker), b.p_bp))
            .chain(
                m.p_gap_bp
                    .iter()
                    .map(|g| (format!("gap k={}", g.k_f), g.p_bp)),
            );
        for (what, p) in ps {
            if p > 10000 {
                r.fail(
                    10,
                    "market_implied",
                    format!("{slot}: {what} p_bp {p} > 10000"),
                );
            }
        }
        // u64: a forged block of many large u32s must not wrap into range.
        let sum: u64 = m.bracket_p_bp.iter().map(|b| u64::from(b.p_bp)).sum();
        if !(9999..=10001).contains(&sum) {
            r.fail(
                10,
                "market_implied",
                format!(
                    "{slot}: bracket_p_bp sums to {sum} over {} brackets, not 10000 ± 1",
                    m.bracket_p_bp.len()
                ),
            );
        }
        for w in m.p_gap_bp.windows(2) {
            let (g0, g1) = (&w[0], &w[1]);
            if g1.k_f <= g0.k_f {
                r.fail(
                    10,
                    "market_implied",
                    format!(
                        "{slot}: p_gap_bp k={} after k={}, not strictly increasing",
                        g1.k_f, g0.k_f
                    ),
                );
            } else if g1.p_bp > g0.p_bp {
                r.fail(
                    10,
                    "market_implied",
                    format!(
                        "{slot}: p_gap_bp k={} p_bp {} > k={} p_bp {}, P(exceed by >= k) rises with k",
                        g1.k_f, g1.p_bp, g0.k_f, g0.p_bp
                    ),
                );
            }
        }
        // (e). No scheduled slot fails check 3 and an unparseable
        // decision_time fails check 9, so both just skip this one; `if let`,
        // not `continue`, as (a)-(c) above have already run.
        let due = a.receipts.iter().find_map(|x| match &x.payload.body {
            Body::Schedule(s) if (&s.station, &s.climate_day) == (&fr.station, &fr.climate_day) => {
                s.slots
                    .iter()
                    .find(|sl| &sl.slot_id == slot)
                    .map(|sl| &sl.decision_time)
            }
            _ => None,
        });
        if let Some((due, due_ms)) = due.and_then(|d| Some((d, unix_ms(d)?))) {
            let at = &m.computed_at;
            match unix_ms(at) {
                None => r.fail(
                    10,
                    "market_implied",
                    format!("{slot}: computed_at {at} is not a canonical RFC 3339 UTC timestamp"),
                ),
                Some(t) if t < due_ms - MARKET_IMPLIED_LEAD_SECS * 1000 => r.fail(
                    10,
                    "market_implied",
                    format!(
                        "{slot}: computed_at {at} is early, more than {MARKET_IMPLIED_LEAD_SECS} s before decision_time {due}"
                    ),
                ),
                Some(t) if t > due_ms + MARKET_IMPLIED_LAG_SECS * 1000 => r.fail(
                    10,
                    "market_implied",
                    format!(
                        "{slot}: computed_at {at} is late, more than {MARKET_IMPLIED_LAG_SECS} s after decision_time {due}"
                    ),
                ),
                Some(_) => {}
            }
        }
    }
    // (d), the lookup as in check_bet_peaks; a missing commit or reveal is
    // checks 4 and 5's to fail.
    for rec in &a.receipts {
        let Body::BetReveal(br) = &rec.payload.body else {
            continue;
        };
        let Some(mr) = &br.market_implied_ref else {
            continue;
        };
        let Some(Body::BetCommit(bc)) = a.get(&br.commit_ref).map(|x| &x.payload.body) else {
            continue;
        };
        let Some(fr) = a.receipts.iter().find_map(|x| match &x.payload.body {
            Body::ForecastReveal(fr) if fr.commit_ref == bc.reveal_commit_ref => Some(fr),
            _ => None,
        }) else {
            continue;
        };
        let what = format!("{}: bet {}", fr.slot_id, br.client_order_id);
        match fr.market_implied.as_deref().map(market_implied_hash) {
            None => r.fail(
                10,
                "market_implied",
                format!("{what}: market_implied_ref {mr} names a block, but the forecast reveal has none"),
            ),
            Some(Ok(h)) if &h == mr => {}
            Some(Ok(h)) => r.fail(
                10,
                "market_implied",
                format!("{what}: market_implied_ref {mr}, but the forecast reveal's block hashes to {h}"),
            ),
            Some(Err(e)) => r.fail(10, "market_implied", format!("{what}: market_implied_hash: {e:#}")),
        }
    }
}

/// (d) of [`check_peaks`]: NOTE `outcome known` when `c` is the final CLI for
/// `fr`'s climate day. Its bytes are found through the manifest row with the
/// same product_id and first_seen (the peak block names no sha256). A CLI the
/// manifest does not list fails, as check 8 fails an unlisted input; one
/// wx-obs cannot read is noted, since its layout is unverified beyond HGX.
fn check_cli_final(a: &Archive, fr: &ForecastReveal, c: &PeakCli, r: &mut Report) {
    let slot = &fr.slot_id;
    let Some(m) = a
        .manifest
        .iter()
        .find(|m| (&m.product_id, &m.first_seen) == (&c.product_id, &c.first_seen))
    else {
        r.fail(
            9,
            "peak",
            format!(
                "{slot}: latest_cli_seen {} first seen {} not in manifest",
                c.product_id, c.first_seen
            ),
        );
        return;
    };
    let body = match a.product(&m.sha256) {
        Ok(b) => b,
        Err(e) => {
            r.fail(
                9,
                "peak",
                format!("{slot}: latest_cli_seen {}: {e:#}", c.product_id),
            );
            return;
        }
    };
    match parse_cli(&body) {
        Err(e) => r.notes.push(format!(
            "NOTE peak: {slot}: latest_cli_seen {} unreadable ({e}), not checked for final",
            c.product_id
        )),
        Ok(cli) => {
            if cli.kind == CliKind::Final && climate_day(&fr.climate_day) == Some(cli.climate_day) {
                outcome_known(slot, r);
            }
        }
    }
}

/// (d) of [`check_peaks`] from the manifest: every `cli` or `cli_prelim` row
/// for `fr`'s station and climate day first seen in `seen_ms` (from before
/// the climate day could have begun to the decision). A final one is `outcome known` whatever the agent's `latest_cli_seen` says; one
/// first seen after `latest_cli_seen` fails, and so does any when the field
/// is null and none of them is final, because then "latest" is false.
fn check_cli_manifest(
    a: &Archive,
    fr: &ForecastReveal,
    peak: &Peak,
    seen_ms: std::ops::Range<i64>,
    r: &mut Report,
) {
    let slot = &fr.slot_id;
    let day = climate_day(&fr.climate_day);
    let latest = peak.latest_cli_seen.as_ref();
    let mut final_seen = false;
    let mut unclaimed = Vec::new();
    for m in &a.manifest {
        // wx-archive stores finals as `cli` and "VALID TODAY AS OF" prelims as
        // `cli_prelim`; the body parse below tells them apart.
        if !matches!(m.source.as_str(), "cli" | "cli_prelim") {
            continue;
        }
        // Rows outside the range are skipped unread: a CLI seen before the
        // day began cannot be that day's, and product_id does not reliably
        // name the station (`CLINYC.prelim1`), so time is the filter.
        let Some(seen) = unix_ms(&m.first_seen).filter(|t| seen_ms.contains(t)) else {
            continue;
        };
        let what = format!("manifest cli {} {}", m.product_id, m.sha256);
        let body = match a.product(&m.sha256) {
            Ok(b) => b,
            Err(e) => {
                r.fail(9, "peak", format!("{slot}: {what}: {e:#}"));
                continue;
            }
        };
        if !cli_names_station(&body, &fr.station) {
            continue;
        }
        let cli = match parse_cli(&body) {
            Ok(c) => c,
            Err(e) => {
                r.notes.push(format!(
                    "NOTE peak: {slot}: {what} unreadable ({e}), not checked for final"
                ));
                continue;
            }
        };
        if day != Some(cli.climate_day) {
            continue;
        }
        final_seen |= cli.kind == CliKind::Final;
        match latest {
            // A non-canonical latest first_seen already failed in check_peaks.
            Some(c) if unix_ms(&c.first_seen).is_some_and(|t| seen > t) => r.fail(
                9,
                "peak",
                format!(
                    "{slot}: latest_cli_seen {} first seen {}, but {what} for {} was first seen \
                     later, {}, before decision_time",
                    c.product_id, c.first_seen, fr.climate_day, m.first_seen
                ),
            ),
            Some(_) => {}
            None => unclaimed.push((what, &m.first_seen)),
        }
    }
    if final_seen {
        outcome_known(slot, r);
    } else {
        for (what, seen) in unclaimed {
            r.fail(
                9,
                "peak",
                format!(
                    "{slot}: latest_cli_seen is null, but {what} for {} was first seen {seen}, \
                     before decision_time",
                    fr.climate_day
                ),
            );
        }
    }
}

/// The `outcome known` NOTE, once per slot: check_cli_final and the manifest
/// scan can both find the same final CLI.
fn outcome_known(slot: &str, r: &mut Report) {
    let note = format!("NOTE outcome known: {slot}");
    if !r.notes.contains(&note) {
        r.notes.push(note);
    }
}

/// A CLI body as the archive stores it: the api.weather.gov product JSON or
/// its text.
fn parse_cli(body: &str) -> Result<crate::obs::cli::Cli, crate::obs::cli::CliError> {
    crate::obs::cli::parse_product_json(body)
        .map(|p| p.report)
        .or_else(|_| crate::obs::cli::parse_text(body))
}

/// Whether a CLI body (JSON or text) is `station`'s: wx-obs's Cli names no
/// station, so this looks for the AWIPS line `CLI` + the ICAO id without its
/// leading K (`CLINYC` for KNYC), which every cli_product in stations.toml
/// follows. Without it a multi-station archive would hold one station's
/// reveal to another station's CLI.
///
/// ponytail: naming convention; take cli_product from --stations when that
/// file carries it.
fn cli_names_station(body: &str, station: &str) -> bool {
    let pil = format!("CLI{}", station.get(1..).unwrap_or_default());
    let json = serde_json::from_str::<Value>(body).ok();
    let text = json
        .as_ref()
        .and_then(|v| v["productText"].as_str())
        .unwrap_or(body);
    text.lines().take(8).any(|l| l.trim() == pil)
}

/// Unix ms for `YYYY-MM-DD` (its midnight) or `YYYY-MM-DDTHH:MM:SS[.mmm]Z`,
/// UTC: the formats rune-record and this crate write. Anything else is None.
pub fn unix_ms(s: &str) -> Option<i64> {
    let n = |a: usize, b: usize| -> Option<i64> {
        s.get(a..b)
            .filter(|t| t.bytes().all(|c| c.is_ascii_digit()))?
            .parse()
            .ok()
    };
    let at = |i: usize, c: u8| s.as_bytes().get(i) == Some(&c);
    if !(at(4, b'-') && at(7, b'-')) {
        return None;
    }
    let (y, m, d) = (n(0, 4)?, n(5, 7)?, n(8, 10)?);
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let mdays = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    // A day past its month's end (02-30) would roll over, not be refused.
    if !(1..=12).contains(&m) || !(1..=mdays[m as usize - 1]).contains(&d) {
        return None;
    }
    let (h, mi, sec, ms) = match s.len() {
        10 => (0, 0, 0, 0),
        20 | 24 if at(10, b'T') && at(13, b':') && at(16, b':') && s.ends_with('Z') => {
            let ms = if s.len() == 24 && at(19, b'.') {
                n(20, 23)?
            } else if s.len() == 20 {
                0
            } else {
                return None;
            };
            let (h, mi, sec) = (n(11, 13)?, n(14, 16)?, n(17, 19)?);
            // 24:00 or :60 is non-canonical: refused, not rolled over.
            if h > 23 || mi > 59 || sec > 59 {
                return None;
            }
            (h, mi, sec, ms)
        }
        _ => return None,
    };
    // days_from_civil (H. Hinnant): proleptic Gregorian date -> days since 1970-01-01.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    Some((((days * 24 + h) * 60 + mi) * 60 + sec) * 1000 + ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_window_is_2359_local_utc4_to_utc10() {
        // CDT (UTC-5) and CST (UTC-6) closes, and the window's edges.
        assert!(close_plausible("2026-07-15", "2026-07-16T04:59:00Z"));
        assert!(close_plausible("2026-01-15", "2026-01-16T05:59:00.000Z"));
        assert!(close_plausible("2026-07-15", "2026-07-16T03:59:00Z"));
        assert!(close_plausible("2026-07-15", "2026-07-16T09:59:00Z"));
        assert!(!close_plausible("2026-07-15", "2026-07-16T03:58:59Z"));
        assert!(!close_plausible("2026-07-15", "2026-07-16T10:00:00Z"));
        assert!(!close_plausible("2026-07-15", "bad"));
        assert!(!close_plausible("bad", "2026-07-16T04:59:00Z"));
    }

    #[test]
    fn date_of_day_inverts_unix_ms() {
        for d in [
            "1970-01-01",
            "2000-02-29",
            "2026-10-20",
            "2026-12-31",
            "2100-03-01",
        ] {
            assert_eq!(date_of_day(unix_ms(d).unwrap() / DAY_MS), d);
        }
    }
}
