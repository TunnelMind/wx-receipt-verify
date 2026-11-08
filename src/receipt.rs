//! Receipt v1 verification and the JCS canonicaliser, VENDORED (verify-only).
//!
//! Source: https://github.com/TunnelMind/rune-record
//!   rev 0959ac5feca909fb9adb98a0e3f7be460aca4c4c
//!   files: src/receipt.rs (`jcs_bytes`, `sha256_hex`),
//!          src/signing.rs (`verify_ed25519`),
//!          src/receipt_v1.rs (`RECEIPT_VERSION`, `Verified`, `sha256_0x`,
//!          `check_numbers`, `signing_bytes`, `verify`)
//! Licence: rune-record is Apache-2.0, and so are this file and this crate.
//!
//! Why vendored: crates.io refuses a crate whose [dependencies] carry a git
//! source, and this verifier must publish (spec 027 wx-receipt-oss, R001/R004).
//! The code is copied unchanged, error strings included, so verify output is
//! byte-for-byte what rune-record gives; the parity test in tests/ keeps the
//! two in step (rune-record stays a git dev-dependency for that test only).
//!
//! VERIFY-ONLY: do not add signing here (no keys, no `sign`, no ids or
//! clocks). The signing side lives in wx-receipt over rune-record itself.
//!
//! Wire invariants: `payload_hash` is "0x" + hex SHA-256 of the payload's JCS
//! bytes; the Ed25519 signature covers the JCS bytes of the receipt with
//! `payload` and `signature.value` removed. Trust boundary: `verify` proves
//! integrity under the key EMBEDDED in the receipt; whether that key is the
//! pinned one is the caller's check (verify.rs). Fails closed, never panics.

use anyhow::{bail, ensure, Context, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use ring::signature::{UnparsedPublicKey, ED25519};
use serde_json::Value;

pub const RECEIPT_VERSION: &str = "1.0";

/// RFC 8785 canonical bytes.
pub fn jcs_bytes(v: &Value) -> Result<Vec<u8>> {
    serde_json_canonicalizer::to_vec(v).context("JCS canonicalization failed")
}

/// Lowercase hex SHA-256.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(ring::digest::digest(&ring::digest::SHA256, bytes).as_ref())
}

/// Ed25519 verification of `sig` over `msg` under the hex-encoded public key.
/// Bad hex, a wrong-length key or a forged signature are all the same `Err`.
pub fn verify_ed25519(pubkey_hex: &str, msg: &[u8], sig: &[u8]) -> Result<()> {
    let pk = hex::decode(pubkey_hex).context("decoding pubkey hex")?;
    UnparsedPublicKey::new(&ED25519, pk)
        .verify(msg, sig)
        .map_err(|_| anyhow::anyhow!("signature verification failed"))
}

/// What a successful [`verify`] vouches for. Whether `public_key` is a
/// trusted key is the caller's call.
#[derive(Debug)]
pub struct Verified {
    pub receipt_id: String,
    pub key_id: String,
    pub public_key: String,
}

fn sha256_0x(bytes: &[u8]) -> String {
    format!("0x{}", sha256_hex(bytes))
}

/// Refuse what JS would format or round differently from Rust: non-integers
/// and integers outside ±(2^53-1).
pub fn check_numbers(v: &Value) -> Result<()> {
    const MAX_SAFE: u64 = (1 << 53) - 1;
    match v {
        Value::Number(n) => {
            let safe = n.as_u64().is_some_and(|u| u <= MAX_SAFE)
                || n.as_i64().is_some_and(|i| i.unsigned_abs() <= MAX_SAFE);
            ensure!(safe, "receipt: number {n} is not a JS-safe integer");
        }
        Value::Array(a) => a.iter().try_for_each(check_numbers)?,
        Value::Object(m) => m.values().try_for_each(check_numbers)?,
        _ => {}
    }
    Ok(())
}

/// The bytes the signature covers.
fn signing_bytes(receipt: &Value) -> Result<Vec<u8>> {
    let mut signing = receipt.clone();
    let obj = signing
        .as_object_mut()
        .context("receipt must be an object")?;
    obj.remove("payload");
    if let Some(sig) = obj.get_mut("signature").and_then(Value::as_object_mut) {
        sig.remove("value");
    }
    jcs_bytes(&signing)
}

/// Pure-crypto verification: version, payload hash, and the signature against
/// the embedded public key. No network. A pass means "untampered since signed
/// by `public_key`".
pub fn verify(receipt: &Value) -> Result<Verified> {
    let str_at = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);

    // Exactly "1.0"; unknown majors MUST be rejected.
    let version = str_at(receipt, "receipt_version").unwrap_or_default();
    if version != RECEIPT_VERSION {
        bail!("unsupported receipt_version {version:?} (expected {RECEIPT_VERSION})");
    }

    let payload = receipt.get("payload").context("missing payload")?;
    let want = str_at(receipt, "payload_hash").context("missing payload_hash")?;
    ensure!(
        sha256_0x(&jcs_bytes(payload)?) == want,
        "payload_hash does not match payload"
    );

    let sig = receipt.get("signature").context("missing signature")?;
    ensure!(
        str_at(sig, "algorithm").as_deref() == Some("Ed25519"),
        "unsupported signature.algorithm"
    );
    let public_key = str_at(sig, "public_key").context("missing signature.public_key")?;
    let raw_pubkey = B64
        .decode(&public_key)
        .context("decoding signature.public_key base64")?;
    ensure!(
        raw_pubkey.len() == 32,
        "signature.public_key is not 32 bytes"
    );
    let value = B64
        .decode(str_at(sig, "value").context("missing signature.value")?)
        .context("decoding signature.value base64")?;
    verify_ed25519(&hex::encode(&raw_pubkey), &signing_bytes(receipt)?, &value)?;

    Ok(Verified {
        receipt_id: str_at(receipt, "receipt_id").context("missing receipt_id")?,
        key_id: str_at(sig, "key_id").context("missing signature.key_id")?,
        public_key,
    })
}
