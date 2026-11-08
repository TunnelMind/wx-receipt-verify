//! The vendored src/receipt.rs must stay identical in behaviour to the
//! rune-record rev it was copied from (pinned as a dev-dependency): the same
//! JCS bytes and the same verify outcome, error text included, on every
//! fixture plus hand-made edge values and tampered receipts.

use std::path::{Path, PathBuf};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use rune_record::receipt as rr_jcs;
use rune_record::receipt_v1 as rr;
use serde_json::{json, Value};
use wx_receipt_verify::receipt as ours;

fn json_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            json_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "json") {
            out.push(p);
        }
    }
}

fn fixtures() -> Vec<(PathBuf, Value)> {
    let mut files = Vec::new();
    json_files(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures"),
        &mut files,
    );
    files.sort();
    assert!(!files.is_empty(), "no fixtures found");
    files
        .into_iter()
        .map(|p| {
            let v = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
            (p, v)
        })
        .collect()
}

fn assert_jcs_same(v: &Value, what: &str) {
    assert_eq!(
        ours::jcs_bytes(v).unwrap(),
        rr_jcs::jcs_bytes(v).unwrap(),
        "JCS differs: {what}"
    );
}

/// Both Ok with the same fields, or both Err with the same message.
fn assert_verify_same(v: &Value, what: &str) -> bool {
    match (ours::verify(v), rr::verify(v)) {
        (Ok(a), Ok(b)) => {
            assert_eq!(
                (a.receipt_id, a.key_id, a.public_key),
                (b.receipt_id, b.key_id, b.public_key),
                "{what}"
            );
            true
        }
        (Err(a), Err(b)) => {
            assert_eq!(format!("{a:#}"), format!("{b:#}"), "{what}");
            false
        }
        (a, b) => panic!("verify disagrees on {what}: ours {a:?}, rune-record {b:?}"),
    }
}

#[test]
fn jcs_byte_identical_on_fixtures_and_edge_values() {
    for (p, v) in fixtures() {
        assert_jcs_same(&v, &p.display().to_string());
        assert_jcs_same(&v["payload"], &format!("{} payload", p.display()));
    }
    let edges = [
        json!({"b": 1, "a": 2, "aa": [3, {"z": null, "y": true}]}),
        // UTF-16 key order: U+E000 sorts before U+1F600 (a surrogate pair).
        json!({"\u{1F600}": 1, "\u{E000}": 2, "é": "ünïcödé \u{2028} \"q\" \\ \t"}),
        json!([
            0,
            -0,
            1,
            -1,
            9007199254740991i64,
            -9007199254740991i64,
            u64::MAX,
            i64::MIN
        ]),
        json!([1.5, 1e21, 1e-7, 0.1, 100.0]),
        json!({}),
        json!([]),
        json!(""),
        json!(null),
    ];
    for v in &edges {
        assert_jcs_same(v, &v.to_string());
    }
}

#[test]
fn verify_agrees_on_fixtures_and_tampered_copies() {
    let mut verified = 0;
    for (p, v) in fixtures() {
        if v.get("receipt_version").is_none() {
            continue;
        }
        let name = p.display().to_string();
        assert!(
            assert_verify_same(&v, &name),
            "fixture should verify: {name}"
        );
        verified += 1;

        // One flipped bit of the signature: both must refuse.
        let mut bad = v.clone();
        let mut sig = B64
            .decode(v["signature"]["value"].as_str().unwrap())
            .unwrap();
        sig[0] ^= 1;
        bad["signature"]["value"] = B64.encode(sig).into();
        assert!(!assert_verify_same(&bad, &format!("{name} sig flipped")));

        // A payload edit: both must refuse (payload_hash).
        let mut bad = v.clone();
        bad["payload"]["tampered"] = json!(1);
        assert!(!assert_verify_same(&bad, &format!("{name} payload edited")));

        // A signed envelope field edit: both must refuse (signature).
        let mut bad = v.clone();
        bad["receipt_id"] = json!("x");
        assert!(!assert_verify_same(
            &bad,
            &format!("{name} receipt_id edited")
        ));
    }
    assert!(verified > 0, "no receipt fixtures verified");

    for bad in [
        json!(null),
        json!([]),
        json!({"receipt_version": "2.0"}),
        json!({"receipt_version": "1.0"}),
    ] {
        assert!(!assert_verify_same(&bad, &bad.to_string()));
    }
}

#[test]
fn check_numbers_agrees() {
    const MAX: i64 = (1 << 53) - 1;
    for v in [
        json!(MAX),
        json!(-MAX),
        json!(MAX + 1),
        json!(1.0),
        json!({"a": [u64::MAX]}),
    ] {
        assert_eq!(
            ours::check_numbers(&v).map_err(|e| e.to_string()),
            rr::check_numbers(&v).map_err(|e| e.to_string()),
            "{v}"
        );
    }
}
