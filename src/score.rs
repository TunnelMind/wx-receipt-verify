//! The public scoring helper (C022): `wx-receipt score <dir>` scores every
//! peak block in an archive against the settled NWS high. Plain arithmetic
//! over the files; it trusts them, so run `verify` first for that.
//!
//! Every reveal with a peak is scored whatever its decision: refusals
//! (`Reject`, `NoMarket`) count too, or the score would only show the slots
//! the gate liked. Anything that cannot be scored is listed in `skipped` with
//! the reason, never dropped.
//!
//! Scales: probabilities in basis points (10000 = 1), as on the receipt.
//! Floats are used inside; every output value is a rounded integer.
//!
//! Output ([`ScoreReport::render`]): a plain table, one row per scored item,
//! then one `SKIP` row per skipped reveal, then a blank line, then the JSON
//! summary on the last line, prefixed `JSON ` (`… | tail -1 | cut -c6-`):
//!
//! ```text
//! {"scored":[{station,climate_day,slot_id,kind,brier_bp,log_score_milli}…],
//!  "by_station":{"<station>":{"<kind>":{n,brier_bp_mean,log_score_milli_mean}}},
//!  "skipped":[{station,climate_day,slot_id,reason}…]}
//! ```
//!
//! `kind` is `gap k=<k>` (one row per [`GapP`]) or `bracket`, from the peak
//! block; and `mkt gap k=<k>` (C061), the same question scored on the
//! market-implied block's p_gap, so ours and the market's sit side by side.
//! The market gap uses the peak's running max, else the reveal's. The
//! market's brackets are not scored: they name Kalshi tickers, and nothing
//! on the receipt maps a ticker to °F bounds, so each such block gets a
//! `market bracket not scored` skip. Rows are sorted by station,
//! climate_day, slot_id; within a slot, gaps by k, bracket, then mkt gaps.
//! `by_station` means are per (station, kind), never pooled: a gap Brier
//! runs 0..10000, a bracket Brier 0..20000, and each k is its own question.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::json;

use crate::verify::Archive;
use crate::*;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Scored {
    pub station: String,
    pub climate_day: String,
    pub slot_id: String,
    pub kind: String,
    pub brier_bp: i64,
    pub log_score_milli: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Skipped {
    pub station: String,
    pub climate_day: String,
    pub slot_id: String,
    pub reason: String,
}

#[derive(Debug, Default)]
pub struct ScoreReport {
    pub scored: Vec<Scored>,
    pub skipped: Vec<Skipped>,
}

/// Units fixed (C041): the standard Brier score summed over the outcome
/// partition, in bp. Brier score in bp of one forecast over outcomes: `p_bp` per outcome and
/// whether it happened. round(sum((p_bp - 10000*y)^2) / 10000), so one binary
/// item ranges 0..=10000 and a multi-bracket forecast 0..=20000. Every
/// `p_bp` must be <= 10000: nothing here clamps, [`score`] skips a row with
/// one above.
pub fn brier_bp(items: &[(u32, bool)]) -> i64 {
    let sum: f64 = items
        .iter()
        .map(|&(p, y)| (f64::from(p) - if y { 10000.0 } else { 0.0 }).powi(2))
        .sum();
    (sum / 10000.0).round() as i64
}

/// Units fixed (C041): natural log, milli-nats, floor 1 bp. Log score in
/// milli-nats of the probability `p_bp` given to what happened:
/// round(1000 * ln(max(p_bp, 1) / 10000)). Floored at 1 bp so a confident
/// miss scores -9210 instead of -inf. `p_bp` must be <= 10000 (not clamped;
/// [`score`] skips a row with one above).
pub fn log_score_milli(p_bp: u32) -> i64 {
    (1000.0 * (f64::from(p_bp.max(1)) / 10000.0).ln()).round() as i64
}

/// The bracket row's (brier_bp, log_score_milli) for peak brackets `bps` at
/// observed high `t`°F, or the skip reason.
/// Hit bracket by check 6's rule: both ends inclusive, None an open end.
pub(crate) fn bracket_score(bps: &[Bracket], t: i32) -> Result<(i64, i64), String> {
    if let Some(b) = bps.iter().find(|b| b.p_bp > 10000) {
        return Err(format!(
            "bracket p_bp {} > 10000: bracket not scored",
            b.p_bp
        ));
    }
    let items: Vec<(u32, bool)> = bps
        .iter()
        .map(|b| {
            (
                b.p_bp,
                b.lo.is_none_or(|lo| lo <= t) && b.hi.is_none_or(|hi| t <= hi),
            )
        })
        .collect();
    let Some(&(p_hit, _)) = items.iter().find(|&&(_, h)| h) else {
        return Err("no peak bracket contains observed_high_f: bracket not scored".into());
    };
    Ok((brier_bp(&items), log_score_milli(p_hit)))
}

/// The day mean (D8): a settlement's `scores` are the mean, over the
/// station-day's forecast reveals (all of its `reveal_refs`, in order), of
/// each reveal's bracket Brier (bp, over `fr.brackets` with `p_bp` clamped
/// to 10000) and log score (milli-nats) at observed high `t`, each mean
/// rounded to an integer (half away from zero). Hit bracket: both ends
/// inclusive, None open. A high in no listed bracket, or no high at all (a
/// void day with no METAR), scores every bracket a miss and log at 0 bp.
/// Zero reveals give {0, 0}. The recorder (wx-run settle) and verify check 6
/// both call this, so the two cannot define it differently.
pub fn day_scores<'a>(
    reveals: impl IntoIterator<Item = &'a ForecastReveal>,
    t: Option<i32>,
) -> Scores {
    let (mut n, mut brier, mut log) = (0i64, 0i64, 0i64);
    for fr in reveals {
        let h = t.and_then(|t| hit(fr, t));
        let items: Vec<(u32, bool)> = (0u32..)
            .zip(&fr.brackets)
            .map(|(i, b)| (b.p_bp.min(10_000), h == Some(i)))
            .collect();
        brier += brier_bp(&items);
        log += log_score_milli(h.map_or(0, |i| items[i as usize].0));
        n += 1;
    }
    let mean = |x: i64| {
        if n == 0 {
            0
        } else {
            (x as f64 / n as f64).round() as i64
        }
    };
    Scores {
        brier_bp: u32::try_from(mean(brier)).unwrap_or(0),
        log_score_milli: mean(log),
    }
}

/// Index of the bracket holding `t` °F, both ends inclusive, None open
/// (wx-run settle's `hit`, check 6's `bracket_hit`).
fn hit(fr: &ForecastReveal, t: i32) -> Option<u32> {
    fr.brackets
        .iter()
        .position(|b| b.lo.is_none_or(|lo| lo <= t) && b.hi.is_none_or(|hi| t <= hi))
        .and_then(|i| u32::try_from(i).ok())
}

/// One row per gap k at observed high `t` and running max `m`: (kind,
/// brier_bp, log_score_milli), kind `<prefix>gap k=<k>`, or the skip reason.
/// The outcome is t - m >= k. A p_bp above 10000 is not a probability:
/// refused, not clamped.
fn gap_rows(prefix: &str, gs: &[GapP], t: i32, m: i32) -> Vec<Result<(String, i64, i64), String>> {
    gs.iter()
        .map(|g| {
            let kind = format!("{prefix}gap k={}", g.k_f);
            if g.p_bp > 10000 {
                return Err(format!("{kind} p_bp {} > 10000: not scored", g.p_bp));
            }
            let y = t - m >= i32::from(g.k_f);
            let p_happened = if y { g.p_bp } else { 10000 - g.p_bp };
            Ok((kind, brier_bp(&[(g.p_bp, y)]), log_score_milli(p_happened)))
        })
        .collect()
}

/// Score every forecast reveal in `a` (see the module doc).
pub fn score(a: &Archive) -> ScoreReport {
    // reveal payload_hash -> the settlement that lists it.
    let mut settled = BTreeMap::new();
    for rec in &a.receipts {
        if let Body::Settlement(s) = &rec.payload.body {
            for rf in &s.reveal_refs {
                settled.entry(rf.as_str()).or_insert(s);
            }
        }
    }
    let mut out = ScoreReport::default();
    for rec in &a.receipts {
        let Body::ForecastReveal(fr) = &rec.payload.body else {
            continue;
        };
        let row = |kind: String, brier_bp, log_score_milli| Scored {
            station: fr.station.clone(),
            climate_day: fr.climate_day.clone(),
            slot_id: fr.slot_id.clone(),
            kind,
            brier_bp,
            log_score_milli,
        };
        let mut skip = |reason: &str| {
            out.skipped.push(Skipped {
                station: fr.station.clone(),
                climate_day: fr.climate_day.clone(),
                slot_id: fr.slot_id.clone(),
                reason: reason.into(),
            })
        };
        if fr.peak.is_none() && fr.market_implied.is_none() {
            skip("no peak block");
            continue;
        }
        let Some(s) = settled.get(rec.hash.as_str()) else {
            skip("unsettled: no settlement lists this reveal");
            continue;
        };
        // TWC settlements carry no observed high, only our word on the outcome.
        let Some(t) = s.observed_high_f else {
            skip("settlement has no observed_high_f (TWC)");
            continue;
        };
        // Each (kind, brier, log) is scored; each Err is skipped with its reason.
        let mut push = |rows: Vec<Result<(String, i64, i64), String>>| {
            for x in rows {
                match x {
                    Ok((kind, b, l)) => out.scored.push(row(kind, b, l)),
                    Err(reason) => skip(&reason),
                }
            }
        };
        let peak = fr.peak.as_deref();
        match peak {
            Some(p) => match p.running_max_f {
                Some(m) => push(gap_rows("", &p.p_gap_bp, t, m)),
                None => push(vec![Err(
                    "peak.running_max_f is null: p_gap not scored".into()
                )]),
            },
            None => push(vec![Err("no peak block".into())]),
        }
        if let Some(p) = peak {
            push(vec![
                bracket_score(&p.bracket_p_bp, t).map(|(b, l)| ("bracket".into(), b, l))
            ]);
        }
        if let Some(mi) = fr.market_implied.as_deref() {
            match peak.and_then(|p| p.running_max_f).or(fr.running_max_f) {
                Some(m) => push(gap_rows("mkt ", &mi.p_gap_bp, t, m)),
                None => push(vec![Err(
                    "no running_max_f on the peak or the reveal: market p_gap not scored".into(),
                )]),
            }
            // ponytail: tickers carry no °F bounds on the receipt; score these
            // once a ticker -> bracket map is committed somewhere verifiable.
            push(vec![Err(
                "market bracket not scored: tickers do not map to peak brackets".into(),
            )]);
        }
    }
    // Stable sorts keep the push order (gaps, bracket, mkt gaps) inside a slot.
    let key = |s: &str, d: &str, id: &str| (s.to_string(), d.to_string(), id.to_string());
    out.scored
        .sort_by_key(|r| key(&r.station, &r.climate_day, &r.slot_id));
    out.skipped
        .sort_by_key(|r| key(&r.station, &r.climate_day, &r.slot_id));
    out
}

impl ScoreReport {
    /// The JSON summary (module doc). Means are rounded to integers.
    pub fn summary(&self) -> serde_json::Value {
        let mut by: BTreeMap<&str, BTreeMap<&str, (i64, i64, i64)>> = BTreeMap::new();
        for r in &self.scored {
            let e = by
                .entry(&r.station)
                .or_default()
                .entry(&r.kind)
                .or_default();
            *e = (e.0 + 1, e.1 + r.brier_bp, e.2 + r.log_score_milli);
        }
        let mean = |sum: i64, n: i64| (sum as f64 / n as f64).round() as i64;
        let by_station: serde_json::Map<String, serde_json::Value> = by
            .into_iter()
            .map(|(st, kinds)| {
                let kinds: serde_json::Map<String, serde_json::Value> = kinds
                    .into_iter()
                    .map(|(kind, (n, b, l))| {
                        (
                            kind.to_string(),
                            json!({"n": n, "brier_bp_mean": mean(b, n), "log_score_milli_mean": mean(l, n)}),
                        )
                    })
                    .collect();
                (st.to_string(), kinds.into())
            })
            .collect();
        json!({"scored": self.scored, "by_station": by_station, "skipped": self.skipped})
    }

    /// The stdout text (module doc).
    pub fn render(&self) -> String {
        let mut s = String::from("station climate_day slot_id kind brier_bp log_score_milli\n");
        for r in &self.scored {
            s += &format!(
                "{} {} {} {} {} {}\n",
                r.station, r.climate_day, r.slot_id, r.kind, r.brier_bp, r.log_score_milli
            );
        }
        for r in &self.skipped {
            s += &format!(
                "SKIP {} {} {}: {}\n",
                r.station, r.climate_day, r.slot_id, r.reason
            );
        }
        s + &format!("\nJSON {}\n", self.summary())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arithmetic_by_hand() {
        // 1000 * ln(0.42) = -867.5 -> -868; ln(0.58) -> -544.7 -> -545.
        assert_eq!(log_score_milli(4200), -868);
        assert_eq!(log_score_milli(5800), -545);
        assert_eq!(log_score_milli(10000), 0);
        // p = 0 is floored to 1 bp: 1000 * ln(1e-4) = -9210.3.
        assert_eq!(log_score_milli(0), -9210);
        // (4200 - 10000)^2 / 10000 = 3364; 4200^2 / 10000 = 1764.
        assert_eq!(brier_bp(&[(4200, true)]), 3364);
        assert_eq!(brier_bp(&[(4200, false)]), 1764);
        // 3000^2 + (4000 - 10000)^2 + 3000^2 = 54e6 -> 5400.
        assert_eq!(
            brier_bp(&[(3000, false), (4000, true), (3000, false)]),
            5400
        );
    }
}
