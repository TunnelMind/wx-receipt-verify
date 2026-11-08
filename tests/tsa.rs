//! C003: RFC 3161 tokens. The fixtures are real freetsa and DigiCert answers
//! from the spike (tests/fixtures/README); the TSAs in `attach` tests are fakes.
//! Copied from wx-receipt/tests/tsa.rs minus the one test that needs a signer
//! (both_down_is_degraded_and_receipt_unchanged), which stays in wx-receipt.

use std::cell::Cell;

use anyhow::{bail, Result};
use serde_json::{json, Value};
use wx_receipt_verify::tsa::{attach, imprint_of, verify_token, TsaClient};

struct Fixture {
    tsr: Vec<u8>,
    imprint: [u8; 32],
    chain: String,
}

fn fixture(tsa: &str) -> Fixture {
    let dir = format!("{}/tests/fixtures/{tsa}", env!("CARGO_MANIFEST_DIR"));
    let mut imprint = [0u8; 32];
    let hex_imprint = std::fs::read_to_string(format!("{dir}/sample.imprint.txt")).unwrap();
    hex::decode_to_slice(hex_imprint.trim(), &mut imprint).unwrap();
    Fixture {
        tsr: std::fs::read(format!("{dir}/sample.tsr")).unwrap(),
        imprint,
        chain: std::fs::read_to_string(format!("{dir}/chain.pem")).unwrap(),
    }
}

// 2026-10-28T01:55:24Z and 01:55:35Z, as `openssl ts -reply -text` prints them.
#[test]
fn fixture_tokens_verify_with_their_gen_time() {
    let f = fixture("freetsa");
    let info = verify_token(&f.tsr, &f.imprint, &f.chain).unwrap();
    assert_eq!(info.gen_time, 1_793_152_524);
    let d = fixture("digicert");
    let info = verify_token(&d.tsr, &d.imprint, &d.chain).unwrap();
    assert_eq!(info.gen_time, 1_793_152_535);
}

#[test]
fn wrong_imprint_rejected() {
    for tsa in ["freetsa", "digicert"] {
        let f = fixture(tsa);
        let mut wrong = f.imprint;
        wrong[0] ^= 1;
        let e = verify_token(&f.tsr, &wrong, &f.chain).unwrap_err();
        assert!(e.to_string().contains("messageImprint"), "{tsa}: {e:#}");
    }
}

#[test]
fn wrong_chain_rejected() {
    let (f, d) = (fixture("freetsa"), fixture("digicert"));
    let e = verify_token(&f.tsr, &f.imprint, &d.chain).unwrap_err();
    assert!(e.to_string().contains("signer cert"), "{e:#}");
    let e = verify_token(&d.tsr, &d.imprint, &f.chain).unwrap_err();
    assert!(e.to_string().contains("signer cert"), "{e:#}");
}

#[test]
fn signer_without_root_rejected() {
    // Only the signer cert pinned: the walk cannot reach a self-signed root.
    let f = fixture("freetsa");
    // freetsa's chain.pem lists the signer first, then the root.
    let end = "-----END CERTIFICATE-----";
    let signer_only = &f.chain[..f.chain.find(end).unwrap() + end.len()];
    let e = verify_token(&f.tsr, &f.imprint, signer_only).unwrap_err();
    assert!(e.to_string().contains("self-signed root"), "{e:#}");
}

#[test]
fn flipped_signature_byte_rejected() {
    for tsa in ["freetsa", "digicert"] {
        let f = fixture(tsa);
        let mut bad = f.tsr.clone();
        // The token ends with the SignerInfo signature (as in the spike's control).
        let n = bad.len() - 10;
        bad[n] ^= 1;
        let e = verify_token(&bad, &f.imprint, &f.chain).unwrap_err();
        assert!(e.to_string().contains("CMS signature"), "{tsa}: {e:#}");
    }
}

/// A fake TSA: answers with `answer` (or fails when None) and counts calls.
struct Fake {
    name: &'static str,
    answer: Option<Vec<u8>>,
    calls: Cell<u32>,
}

impl Fake {
    fn new(name: &'static str, answer: Option<Vec<u8>>) -> Self {
        Fake {
            name,
            answer,
            calls: Cell::new(0),
        }
    }
}

impl TsaClient for Fake {
    fn name(&self) -> &str {
        self.name
    }
    fn timestamp(&self, _imprint: &[u8; 32]) -> Result<Vec<u8>> {
        self.calls.set(self.calls.get() + 1);
        match &self.answer {
            Some(t) => Ok(t.clone()),
            None => bail!("{} is down", self.name),
        }
    }
}

/// A receipt whose payload_hash is the fixture's imprint. A real signed
/// receipt cannot match a recorded token (that would need a SHA-256 preimage).
fn receipt_for(f: &Fixture) -> Value {
    json!({ "payload_hash": format!("0x{}", hex::encode(f.imprint)) })
}

#[test]
fn first_down_second_answers() {
    let f = fixture("freetsa");
    let receipt = receipt_for(&f);
    let (a, b) = (Fake::new("a", None), Fake::new("b", Some(f.tsr.clone())));
    let proof = attach(&[(&a, &f.chain), (&b, &f.chain)], &receipt).unwrap();
    assert_eq!(proof.method, "rfc3161");
    assert!(!proof.degraded);
    assert_eq!(proof.tsa.as_deref(), Some("b"));
    assert_eq!(proof.imprint, Some(hex::encode(f.imprint)));
    // The attached token is the TSA's bytes and verifies for this receipt.
    use base64::Engine as _;
    let tok = base64::engine::general_purpose::STANDARD
        .decode(proof.token.unwrap())
        .unwrap();
    assert_eq!(tok, f.tsr);
    verify_token(&tok, &imprint_of(&receipt).unwrap(), &f.chain).unwrap();
}

#[test]
fn first_answer_wins() {
    let f = fixture("freetsa");
    let (a, b) = (
        Fake::new("a", Some(f.tsr.clone())),
        Fake::new("b", Some(f.tsr.clone())),
    );
    let proof = attach(&[(&a, &f.chain), (&b, &f.chain)], &receipt_for(&f)).unwrap();
    assert_eq!(proof.tsa.as_deref(), Some("a"));
    assert_eq!((a.calls.get(), b.calls.get()), (1, 0));
}

#[test]
fn token_for_another_imprint_counts_as_down() {
    // A TSA that stamps the wrong imprint (or returns garbage) is skipped.
    let (f, d) = (fixture("freetsa"), fixture("digicert"));
    let a = Fake::new("a", Some(d.tsr.clone()));
    let b = Fake::new("b", Some(b"not der".to_vec()));
    let proof = attach(&[(&a, &d.chain), (&b, &f.chain)], &receipt_for(&f)).unwrap();
    assert!(proof.degraded);
    assert_eq!((a.calls.get(), b.calls.get()), (1, 1));
}

#[test]
fn token_outside_pinned_chain_counts_as_down() {
    // TSA a answers with a genuine digicert token for this imprint, but only
    // the freetsa chain is pinned for a: the token is no proof, so b is asked.
    let (f, d) = (fixture("freetsa"), fixture("digicert"));
    let a = Fake::new("a", Some(d.tsr.clone()));
    let b = Fake::new("b", Some(d.tsr.clone()));
    let proof = attach(&[(&a, &f.chain), (&b, &d.chain)], &receipt_for(&d)).unwrap();
    assert!(!proof.degraded);
    assert_eq!(proof.tsa.as_deref(), Some("b"));
    assert_eq!((a.calls.get(), b.calls.get()), (1, 1));
}
