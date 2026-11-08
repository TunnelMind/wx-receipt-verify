//! `wx-receipt-verify score`: the tests from wx-receipt/tests/score.rs that
//! need no signer, copied unchanged. The hand-worked archive tests sign their
//! own receipts and stay in wx-receipt.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use serde_json::Value;
use wx_receipt_verify::*;

#[test]
fn unreadable_archive_exits_2() {
    let d = tempfile::tempdir().unwrap();
    let st = Command::new(env!("CARGO_BIN_EXE_wx-receipt-verify"))
        .arg("score")
        .arg(d.path())
        .status()
        .unwrap();
    assert_eq!(st.code(), Some(2));
}

/// D8 day mean over the public log (spec 125 SC-002): for each group-A
/// settlement in fixtures/day-mean, `day_scores` over its reveal_refs, in
/// order, at observed_high_f equals the recorded `scores`.
#[test]
fn day_scores_match_recorded_group_a_settlements() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/day-mean");
    let payload = |p: &Path| -> Payload {
        let v: Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
        serde_json::from_value(v["payload"].clone()).unwrap()
    };
    let (mut settlements, mut reveals) = (vec![], BTreeMap::new());
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "json") {
            let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
            match payload(&p).body {
                Body::Settlement(s) => settlements.push(s),
                Body::ForecastReveal(fr) => {
                    reveals.insert(v["payload_hash"].as_str().unwrap().to_owned(), fr);
                }
                b => panic!("unexpected fixture {}: {b:?}", p.display()),
            }
        }
    }
    assert_eq!(settlements.len(), 8);
    for s in &settlements {
        let frs = s.reveal_refs.iter().map(|h| &reveals[h]);
        assert_eq!(
            score::day_scores(frs, s.observed_high_f),
            s.scores,
            "{} {}",
            s.station,
            s.climate_day
        );
    }

    // KDEN 2026-10-30: per-reveal Brier 14587/14632/966/8, mean 7548 (spec US1).
    let s = settlements
        .iter()
        .find(|s| s.station == "KDEN" && s.climate_day == "2026-10-30")
        .unwrap();
    let per: Vec<u32> = s
        .reveal_refs
        .iter()
        .map(|h| score::day_scores([&reveals[h]], s.observed_high_f).brier_bp)
        .collect();
    assert_eq!(per, [14587, 14632, 966, 8]);
    let day = score::day_scores(s.reveal_refs.iter().map(|h| &reveals[h]), s.observed_high_f);
    assert_eq!(day.brier_bp, 7548);
}
