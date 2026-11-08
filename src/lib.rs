//! Receipts for the weather agent (spec 027, Fable review §5).
//!
//! Every receipt is a rune-record Receipt v1 whose `payload` is a [`Payload`]:
//! `type`, `seq` and `prev` sit inside the payload so the signature covers
//! them. Streams are per station: `seq` counts up from 0 with no holes and
//! `prev` is the previous receipt's `payload_hash`, so a deleted decision shows
//! as a gap ([`check_chain`]).
//!
//! Integers only, because receipt_v1 refuses floats: temperatures in whole °F
//! (or centi-°F where named), money in cents, probabilities in basis points,
//! sizes in contracts.
//!
//! Never in a payload: anything read from the venue other than our own orders
//! and fills (no book, no market price we did not trade at). The one exception
//! is [`MarketImplied`]: probabilities derived from the book, never the book.
//!
//! This crate is the verify side only (spec 027 wx-receipt-oss): the payload
//! types, the hash and chain checks, the offline verifier and the scorer. The
//! signing side (`sign`, `Stream`, `new_salt`) stays in `wx-receipt`.

// The METAR/CLI parsers check 9 recomputes the running max with. They live
// here so this crate builds alone; wx-obs re-exports this module (one parser).
pub mod obs;
// The CLI both binaries run (wx-receipt-verify, wx-receipt).
pub mod cli;
pub mod receipt;
pub mod score;
pub mod tsa;
pub mod verify;

use crate::receipt::jcs_bytes;
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What gets signed: the stream position plus one typed body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Payload {
    pub seq: u64,
    /// `payload_hash` of the previous receipt in this stream; null for seq 0.
    pub prev: Option<String>,
    #[serde(flatten)]
    pub body: Body,
}

/// One variant per row of the Fable §5 table; `type` is the serde tag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Body {
    #[serde(rename = "wx.forecast.commit")]
    ForecastCommit(ForecastCommit),
    #[serde(rename = "wx.forecast.reveal")]
    ForecastReveal(ForecastReveal),
    #[serde(rename = "wx.bet.commit")]
    BetCommit(BetCommit),
    #[serde(rename = "wx.bet.reveal")]
    BetReveal(BetReveal),
    #[serde(rename = "wx.bet.refused")]
    BetRefused(BetRefused),
    #[serde(rename = "wx.settlement")]
    Settlement(Settlement),
    #[serde(rename = "wx.refill")]
    Refill(Refill),
    #[serde(rename = "wx.rule_change")]
    RuleChange(RuleChange),
    #[serde(rename = "wx.model_version")]
    ModelVersion(ModelVersion),
    #[serde(rename = "wx.schedule")]
    Schedule(Schedule),
}

/// Published within seconds of the decision. Leaks nothing: the forecast, the
/// side and whether we bet are all behind `commitment`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForecastCommit {
    pub station: String,
    pub series: String,
    pub climate_day: String,
    pub slot_id: String,
    pub decision_time: String,
    /// [`commitment`] of the matching [`ForecastReveal`].
    pub commitment: String,
    pub agent: Agent,
    pub archive_ceiling: ArchiveCeiling,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Agent {
    pub tm_weather_sha: String,
    pub wx_model_version: String,
    pub calibrator_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArchiveCeiling {
    pub last_first_seen_used: String,
}

/// Published after market close for the climate day.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForecastReveal {
    /// `payload_hash` of the commit. Not covered by the commitment (see [`commitment`]).
    pub commit_ref: String,
    /// 32 OS-random bytes, hex (`wx_receipt::new_salt`, the signer side).
    pub salt: String,
    pub station: String,
    pub climate_day: String,
    pub slot_id: String,
    pub decision_time: String,
    pub inputs: Vec<Input>,
    pub running_max_f: Option<i32>,
    pub running_max_source: Option<String>,
    pub predictive: Predictive,
    pub brackets: Vec<Bracket>,
    pub advisory: Advisory,
    pub decision: Decision,
    /// The peak block (C020). Inside the reveal so the commitment covers it and
    /// the commit leaks nothing; absent on receipts made before it existed,
    /// which therefore serialize byte-identically. Boxed only to keep [`Body`]
    /// small (clippy::large_enum_variant); serde sees through the box.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak: Option<Box<Peak>>,
    /// The market-implied block (C060). In the reveal for the same reasons as
    /// `peak`; absent when no book was read, so older receipts are unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub market_implied: Option<Box<MarketImplied>>,
}

/// What the market implied at decision time, derived in memory from the book.
/// Probabilities only: never a price level, size or the book itself, and
/// `deny_unknown_fields` refuses any such field on the way in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarketImplied {
    pub bracket_p_bp: Vec<MarketBracket>,
    /// P(final high exceeds the running max by at least k °F), k = 1, 2, 3.
    pub p_gap_bp: Vec<GapP>,
    pub basis: Basis,
    pub computed_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarketBracket {
    pub ticker: String,
    pub p_bp: u32,
}

/// Which side of the book the probabilities were read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Basis {
    Mid,
    Ask,
}

/// What the forecast knew about the day's peak at decision time, and what it
/// made of it. Integers only. The model's parameters are never in here, only
/// their hash; nothing read from the venue but our own order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Peak {
    pub running_max_tenths_f: Option<i32>,
    pub running_max_f: Option<i32>,
    /// The observations the running max was computed from.
    pub obs: Vec<PeakObs>,
    /// The newest CLI product seen before the decision, if any.
    pub latest_cli_seen: Option<PeakCli>,
    /// P(final high exceeds the running max by at least k °F), k = 1, 2, 3.
    pub p_gap_bp: Vec<GapP>,
    pub signals: Vec<Signal>,
    /// SHA-256 hex of the committed model parameters.
    pub model_hash: String,
    pub bracket_p_bp: Vec<Bracket>,
    /// Our own order's limit price and size, when we placed one.
    pub limit_price_cents: Option<u32>,
    pub count: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeakObs {
    pub product_id: String,
    pub sha256: String,
    pub first_seen: String,
    /// SHA-256 hex of the report line the reading was taken from.
    pub line_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeakCli {
    pub product_id: String,
    pub first_seen: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GapP {
    pub k_f: u8,
    pub p_bp: u32,
}

/// A named station signal, e.g. `sea_breeze_onset_min`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Signal {
    pub name: String,
    pub value: i64,
}

/// An archived source product the forecast read, as the public manifest names it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Input {
    pub source: String,
    pub product_id: String,
    pub sha256: String,
    pub first_seen: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Predictive {
    pub mu_centi_f: i64,
    pub sigma_centi_f: u64,
    pub fallback: bool,
}

/// A temperature bracket in whole °F; `None` is an open end.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bracket {
    pub lo: Option<i32>,
    pub hi: Option<i32>,
    pub p_bp: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Advisory {
    pub local_model: LocalModel,
    pub text_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalModel {
    None,
    WidenBp(u32),
    Veto,
}

/// The gate's verdict for this slot. A market-sensitive refusal lives here as
/// `Reject`; the rest are published at once as [`BetRefused`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Decision {
    Accept {
        side: Side,
        bracket_idx: u32,
        limit_cents: u32,
        count: u32,
        fee_cents: u32,
        edge_bp: i32,
        kelly_bp: u32,
        cap_binding: bool,
    },
    Reject {
        rule_id: String,
        reason_code: String,
    },
    NoMarket,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Yes,
    No,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Buy,
    Sell,
}

/// Published within seconds of the order acknowledgement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BetCommit {
    /// `payload_hash` of the forecast commit this order acts on.
    pub reveal_commit_ref: String,
    pub client_order_id: String,
    /// [`commitment`] of the matching [`BetReveal`].
    pub commitment: String,
}

/// Our order and our fills only: the price and size we asked for and the
/// prices we actually traded at. Never the book.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BetReveal {
    pub commit_ref: String,
    pub salt: String,
    pub client_order_id: String,
    pub ticker: String,
    pub side: Side,
    pub action: Action,
    pub limit_cents: u32,
    pub count: u32,
    pub time_in_force: String,
    pub expiration_ts: Option<String>,
    pub placed_at: String,
    pub venue_order_id: String,
    pub fills: Vec<Fill>,
    pub status: String,
    pub cancelled_at: Option<String>,
    /// [`peak_hash`] of the forecast reveal's peak block this order acted on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_ref: Option<String>,
    /// [`market_implied_hash`] of the forecast reveal's market-implied block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub market_implied_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fill {
    pub count: u32,
    pub price_cents: u32,
    pub fee_cents: u32,
    pub filled_at: String,
}

/// A refusal whose reason is not market-sensitive (stale input, cap, kill
/// switch, forged owner, venue down), published at once.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BetRefused {
    pub station: String,
    pub climate_day: String,
    pub slot_id: String,
    pub decision_time: String,
    /// The gate rule that refused.
    pub rule_id: String,
    pub reason_code: String,
    pub evidence_hash: String,
    /// The ordered input sha256s whose newline-joined sha256 is
    /// `evidence_hash`, so a reader can check the hash without recomputing
    /// the row. Only `health_do_not_settle` sets it; omitted when empty, so
    /// every other refusal stays byte-identical.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_inputs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settlement {
    pub reveal_refs: Vec<String>,
    pub station: String,
    pub climate_day: String,
    /// The source our (paper) outcome was settled on.
    pub settlement_source: SettlementSource,
    /// The source the venue settles the market on, when it differs from how
    /// we settled: a TWC market we settle on the NWS CLI says `twc` here and
    /// `nws_cli` above. Omitted when absent, so older receipts stay
    /// byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub market_settlement_source: Option<SettlementSource>,
    /// The CLI's daily max; any settlement may carry it (with `cli`), and
    /// verify check 6 recomputes the outcome whenever both are present.
    pub observed_high_f: Option<i32>,
    /// The CLI product `observed_high_f` was read from.
    pub cli: Option<Cli>,
    pub outcome: Outcome,
    pub bracket_hit_idx: Option<u32>,
    pub pnl: Pnl,
    pub scores: Scores,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettlementSource {
    NwsCli,
    Twc,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cli {
    pub product_id: String,
    pub sha256: String,
    pub first_seen: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Won,
    Lost,
    /// Some Accepts on the station-day won and some lost; each slot's own
    /// result is recomputed from `observed_high_f` (NWS only).
    Mixed,
    Void,
    NoBet,
}

/// Our P&L in cents; `net_cents = gross_cents - fee_cents`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pnl {
    pub gross_cents: i64,
    pub fee_cents: i64,
    pub net_cents: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scores {
    pub brier_bp: u32,
    pub log_score_milli: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Refill {
    pub amount_cents: u64,
    pub bankroll_cap_after_cents: u64,
    pub transfer_ref_hash: String,
    pub founder_sig: String,
}

/// Effective only from `effective_from`, at least one slot after publishing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuleChange {
    pub kind: RuleKind,
    pub before_hash: String,
    pub after_hash: String,
    pub diff_json: String,
    pub effective_from: String,
    pub founder_sig: String,
}

/// The `diff_json` of a `kind: station_map` [`RuleChange`] (C040): the
/// stations that owe a `wx.schedule` for every climate day from the map's
/// `effective_from` on. `after_hash` is the SHA-256 of the `diff_json` string.
/// `effective_from` repeats the receipt's: the founder signs only
/// `after_hash`, so carrying it inside `diff_json` puts it under the
/// signature (R002) and the verifier requires the two to be equal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StationMap {
    pub effective_from: String,
    pub stations: Vec<StationRule>,
}

/// One mapped station. `slots_rule` and `close_rule` are opaque to the
/// verifier: published so the rule is on record, not interpreted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StationRule {
    pub station: String,
    pub slots_rule: String,
    pub close_rule: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleKind {
    GateConfig,
    StationMap,
    Schedule,
    Mode,
    KillReset,
}

/// Published before the model or calibrator is first used.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelVersion {
    pub wx_model_sha: String,
    pub calibrator_id: String,
    pub fit: Fit,
    pub validation: Validation,
    pub effective_from: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fit {
    /// `[first, last]` first_seen of the training inputs.
    pub train_first_seen_range: [String; 2],
    pub tiers_used: Vec<String>,
    pub params_hash: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Validation {
    pub holdout_days: u32,
    pub crps_diff_milli: i64,
    pub ci_lo_milli: i64,
    pub ci_hi_milli: i64,
}

/// The day's decision slots for one station, published before the first of
/// them. With it a skipped slot is visible: every slot must end up with exactly
/// one commit ([`check_schedule`]), so the agent cannot quietly sit out the
/// slots it would have lost.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Schedule {
    pub station: String,
    pub climate_day: String,
    pub slots: Vec<Slot>,
    pub published_at: String,
    /// When trading closes for this station-day, RFC 3339 UTC
    /// (`2026-07-16T04:59:00.000Z`): Kalshi's 23:59 local clock time for the
    /// series (DST-aware), converted to UTC by the agent and signed before the
    /// day, so the verifier needs no time-zone database. Check 4 takes the
    /// close from here.
    pub close_utc: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Slot {
    pub slot_id: String,
    pub decision_time: String,
}

/// SHA-256(JCS(reveal)), lowercase hex.
///
/// The reveal's `seq`, `prev` and `commit_ref` are left out of the hash: none
/// of them exists yet when the commit is made (the reveal's stream position is
/// only known after close, and `commit_ref` names the commit itself). Everything
/// else, the salt and `type` included, is covered.
pub fn commitment(reveal: &Payload) -> Result<String> {
    ensure!(
        matches!(reveal.body, Body::ForecastReveal(_) | Body::BetReveal(_)),
        "commitment: only forecast and bet reveals are committed"
    );
    let mut v = serde_json::to_value(reveal)?;
    let obj = v.as_object_mut().context("payload is not an object")?;
    for k in ["seq", "prev", "commit_ref"] {
        obj.remove(k);
    }
    let digest = ring::digest::digest(&ring::digest::SHA256, &jcs_bytes(&v)?);
    Ok(hex::encode(digest))
}

/// SHA-256(JCS(peak)), lowercase hex: what a bet's `peak_ref` names.
pub fn peak_hash(peak: &Peak) -> Result<String> {
    let bytes = jcs_bytes(&serde_json::to_value(peak)?)?;
    Ok(hex::encode(ring::digest::digest(
        &ring::digest::SHA256,
        &bytes,
    )))
}

/// SHA-256(JCS(market_implied)), lowercase hex: what a bet's `market_implied_ref` names.
pub fn market_implied_hash(m: &MarketImplied) -> Result<String> {
    let bytes = jcs_bytes(&serde_json::to_value(m)?)?;
    Ok(hex::encode(ring::digest::digest(
        &ring::digest::SHA256,
        &bytes,
    )))
}

/// Check one stream's signed receipts, in order: `seq` runs 0, 1, 2… and each
/// `prev` is the previous receipt's `payload_hash`. The error names the first
/// missing seq or broken link. Signatures are checked elsewhere.
pub fn check_chain(receipts: &[Value]) -> Result<()> {
    let mut prev_hash: Option<&str> = None;
    for (want, r) in (0u64..).zip(receipts) {
        let p = &r["payload"];
        let seq = p["seq"].as_u64().context("receipt payload has no seq")?;
        if seq > want {
            bail!(
                "seq gap: expected seq {want}, found seq {seq} (seq {want}..={} missing)",
                seq - 1
            );
        }
        ensure!(
            seq == want,
            "seq out of order: expected seq {want}, found seq {seq}"
        );
        let prev = p["prev"].as_str();
        if prev != prev_hash {
            bail!("broken prev link at seq {seq}: prev {prev:?}, expected {prev_hash:?}");
        }
        prev_hash = Some(
            r["payload_hash"]
                .as_str()
                .with_context(|| format!("receipt seq {seq} has no payload_hash"))?,
        );
    }
    Ok(())
}

/// Check a schedule against its station's signed receipts: every scheduled
/// slot has exactly one `wx.forecast.commit` or `wx.bet.refused` (a refusal is
/// that slot's commit), and nothing was committed for that station and day
/// outside the schedule. Receipts of other types, stations or days are ignored.
/// The schedule must also be published before every slot's decision time.
/// The error names the offending slot_id. Signatures and the chain are checked
/// elsewhere ([`check_chain`]).
pub fn check_schedule(schedule: &Schedule, commits: &[Value]) -> Result<()> {
    check_schedule_as_of(schedule, commits, None).map(|_| ())
}

/// [`check_schedule`] as of a verifier-given instant (unix ms): a slot with no
/// commit whose `decision_time` + [`verify::COMMIT_BOUND_SECS`] is still after
/// `as_of_ms` is not yet due, so its id is returned as pending instead of
/// failing. Every other error is unchanged. `None` is exactly
/// [`check_schedule`].
pub fn check_schedule_as_of(
    schedule: &Schedule,
    commits: &[Value],
    as_of_ms: Option<i64>,
) -> Result<Vec<String>> {
    use std::collections::BTreeMap;
    // slot_id -> commits seen for it.
    let mut seen: BTreeMap<&str, u32> = BTreeMap::new();
    for slot in &schedule.slots {
        ensure!(
            seen.insert(&slot.slot_id, 0).is_none(),
            "schedule lists slot {} twice",
            slot.slot_id
        );
        // Compared as unix ms, never as strings: a non-canonical timestamp
        // (offset, space for `T`, out-of-range field) is refused, not sorted.
        let ms = |t: &str, what: &str| {
            crate::verify::unix_ms(t)
                .with_context(|| format!("{what} {t:?} is not a canonical RFC 3339 UTC timestamp"))
        };
        ensure!(
            ms(&schedule.published_at, "schedule published_at")?
                < ms(
                    &slot.decision_time,
                    &format!("slot {} decision_time", slot.slot_id)
                )?,
            "schedule published at {} is not before slot {} at {}",
            schedule.published_at,
            slot.slot_id,
            slot.decision_time
        );
    }
    for r in commits {
        let p = &r["payload"];
        if !matches!(
            p["type"].as_str(),
            Some("wx.forecast.commit" | "wx.bet.refused")
        ) || p["station"] != schedule.station.as_str()
            || p["climate_day"] != schedule.climate_day.as_str()
        {
            continue;
        }
        let slot = p["slot_id"].as_str().context("commit has no slot_id")?;
        let n = seen
            .get_mut(slot)
            .with_context(|| format!("commit for slot {slot}, which is not in the schedule"))?;
        *n += 1;
        ensure!(*n == 1, "slot {slot} has more than one commit");
    }
    let mut pending = Vec::new();
    for (slot, _) in seen.iter().filter(|(_, n)| **n == 0) {
        // decision_time already parsed as canonical in the loop above.
        let due = schedule
            .slots
            .iter()
            .find(|s| s.slot_id == *slot)
            .and_then(|s| crate::verify::unix_ms(&s.decision_time));
        match (as_of_ms, due) {
            (Some(t), Some(d)) if d + crate::verify::COMMIT_BOUND_SECS * 1000 > t => {
                pending.push(slot.to_string())
            }
            _ => bail!("slot {slot} has no commit"),
        }
    }
    Ok(pending)
}
