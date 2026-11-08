//! RFC 3161 timestamps over a receipt's `payload_hash` (ADR-037, spike S003).
//!
//! [`attach`] asks each TSA in turn for a token and keeps the first that
//! verifies under that TSA's pinned chain; the result is a [`Proof`] held
//! BESIDE the signed receipt, never written into its `timestamp_proof` (that
//! field is inside the signed bytes). [`verify_token`]
//! is the spike's `docs/tsa-spike/verify/verify-tsr.mjs` in Rust: CMS signature,
//! messageDigest, ESS signingCertificate, signer EKU, a chain walk that may end
//! only at a self-signed root inside the pinned chain, the imprint, and genTime
//! inside every chain cert's validity window. No system trust store.
//!
//! ponytail: definite-length DER only (what TSAs emit); BER indefinite lengths,
//! RSA-PSS, P-521 and a subjectKeyIdentifier sid are refused rather than guessed.

use anyhow::{bail, ensure, Context, Result};
use base64::Engine as _;
use ring::digest;
use ring::signature::{self as sig, UnparsedPublicKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One time-stamping authority.
///
/// SEAM: no HTTP implementation here. The runtime that owns the network
/// implements this by POSTing a DER TimeStampReq (`application/timestamp-query`)
/// for the imprint to the TSA's URL; this crate stays network-free so tests and
/// offline verification never pull an HTTP stack.
pub trait TsaClient {
    /// Recorded in the proof (the TSA URL) so a verifier knows which pinned
    /// chain to check the token against.
    fn name(&self) -> &str;
    /// The DER TimeStampResp for a SHA-256 messageImprint.
    fn timestamp(&self, imprint_sha256: &[u8; 32]) -> Result<Vec<u8>>;
}

/// An attached timestamp proof, kept beside the signed receipt. Not signed, but
/// integers only all the same.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Proof {
    /// `rfc3161`, or `none` when every TSA failed.
    pub method: String,
    /// True when no TSA answered: the receipt stands on its own `timestamp`
    /// claim. No time is invented or backdated in its place.
    pub degraded: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tsa: Option<String>,
    /// Always `sha256` for a token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash_alg: Option<String>,
    /// The messageImprint, lowercase hex: the receipt's `payload_hash` bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imprint: Option<String>,
    /// Base64 of the whole DER TimeStampResp as the TSA returned it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

impl Proof {
    fn degraded() -> Self {
        Proof {
            method: "none".into(),
            degraded: true,
            tsa: None,
            hash_alg: None,
            imprint: None,
            token: None,
        }
    }
}

/// What a verified token says.
#[derive(Debug, Clone, PartialEq)]
pub struct TokenInfo {
    /// TSTInfo genTime, unix seconds (fraction dropped).
    pub gen_time: i64,
}

/// The messageImprint for a signed receipt. `payload_hash` is "0x" + hex
/// SHA-256(JCS(payload)) (receipt.rs, from rune-record), so its 32 bytes already ARE
/// the SHA-256 the TSA stamps: the token times the payload, with no second hash.
pub fn imprint_of(receipt: &Value) -> Result<[u8; 32]> {
    let h = receipt["payload_hash"]
        .as_str()
        .context("receipt has no payload_hash")?;
    let h = h
        .strip_prefix("0x")
        .context("payload_hash is not 0x-prefixed")?;
    let mut out = [0u8; 32];
    hex::decode_to_slice(h, &mut out).context("payload_hash is not 32 bytes of hex")?;
    Ok(out)
}

/// Timestamp `receipt`'s `payload_hash`: try `clients` in order, each with its
/// own pinned chain (PEM), and keep the first token that [`verify_token`]
/// accepts under THAT client's chain. If every one fails, the proof is
/// `{method: none, degraded: true}`. The receipt is only read, never changed.
pub fn attach(clients: &[(&dyn TsaClient, &str)], receipt: &Value) -> Result<Proof> {
    let imprint = imprint_of(receipt)?;
    for (c, chain_pem) in clients {
        // A TSA that errors, refuses, stamps some other imprint, or signs with
        // a cert outside its pinned chain counts as down: a token the verifier
        // would reject is no proof, so the next TSA gets its turn.
        let Ok(tsr) = c.timestamp(&imprint) else {
            continue;
        };
        if verify_token(&tsr, &imprint, chain_pem).is_err() {
            continue;
        }
        return Ok(Proof {
            method: "rfc3161".into(),
            degraded: false,
            tsa: Some(c.name().into()),
            hash_alg: Some("sha256".into()),
            imprint: Some(hex::encode(imprint)),
            token: Some(base64::engine::general_purpose::STANDARD.encode(&tsr)),
        });
    }
    Ok(Proof::degraded())
}

/// Verify a DER TimeStampResp against the expected imprint, trusting only the
/// certificates in `chain_pem` (signer up to a self-signed root).
pub fn verify_token(tsr: &[u8], imprint: &[u8; 32], chain_pem: &str) -> Result<TokenInfo> {
    let t = parse(tsr)?;
    let ders = pem_certs(chain_pem)?;
    let certs = ders
        .iter()
        .map(|d| Cert::parse(d))
        .collect::<Result<Vec<_>>>()?;

    // Signer by issuer AND serial together: a serial alone is only unique per
    // issuer (spike review, low finding).
    let signer = certs
        .iter()
        .find(|c| c.issuer == t.sid_issuer && c.serial == t.sid_serial)
        .context("signer cert (issuer + serial) is not in the pinned chain")?;

    // CMS: the signature covers the signed attrs re-tagged as a SET (RFC 5652 §5.4).
    let mut signed = t.signed_attrs.to_vec();
    signed[0] = 0x31;
    if let Some(implied) = t.sig_alg.1 {
        ensure!(
            implied == t.digest_alg,
            "signatureAlgorithm hash disagrees with digestAlgorithm"
        );
    }
    verify_sig(signer, t.sig_alg.0, t.digest_alg, &signed, t.signature)
        .context("CMS signature does not verify")?;

    let md = t
        .attr(OID_MESSAGE_DIGEST)
        .context("no messageDigest attribute")?;
    ensure!(
        der::octets(md)? == t.digest_alg.digest(t.tst_der).as_slice(),
        "messageDigest does not match TSTInfo"
    );
    ess_matches(&t, signer.der)?;
    ensure!(
        signer.eku.iter().any(|o| o == OID_EKU_TIMESTAMPING),
        "signer cert lacks the timeStamping EKU"
    );

    // Walk signer -> issuer inside the pinned chain until a self-signed root.
    let mut path = vec![signer];
    loop {
        let c = *path.last().unwrap();
        if c.issuer == c.subject && c.verified_by(c).is_ok() {
            break;
        }
        ensure!(path.len() < 8, "chain longer than 8 certs");
        let up = certs
            .iter()
            .find(|i| !std::ptr::eq(*i, c) && i.subject == c.issuer && c.verified_by(i).is_ok())
            .context("chain does not reach a self-signed root in the pinned chain")?;
        path.push(up);
    }

    ensure!(
        t.imprint_alg == Some(Hash::Sha256),
        "messageImprint is not SHA-256"
    );
    ensure!(t.imprint == imprint, "messageImprint does not match");

    let gen_time = generalized_time(t.gen_time)?;
    ensure!(
        path.iter()
            .all(|c| c.not_before <= gen_time && gen_time <= c.not_after),
        "genTime is outside a chain cert's validity window"
    );
    Ok(TokenInfo { gen_time })
}

const OID_MESSAGE_DIGEST: &str = "1.2.840.113549.1.9.4";
const OID_SIGNING_CERT: &str = "1.2.840.113549.1.9.16.2.12";
const OID_SIGNING_CERT_V2: &str = "1.2.840.113549.1.9.16.2.47";
const OID_EKU_TIMESTAMPING: &str = "1.3.6.1.5.5.7.3.8";

#[derive(Debug, Clone, Copy, PartialEq)]
enum Hash {
    Sha1,
    Sha256,
    Sha384,
    Sha512,
}

impl Hash {
    fn from_oid(oid: &str) -> Option<Hash> {
        Some(match oid {
            "1.3.14.3.2.26" => Hash::Sha1,
            "2.16.840.1.101.3.4.2.1" => Hash::Sha256,
            "2.16.840.1.101.3.4.2.2" => Hash::Sha384,
            "2.16.840.1.101.3.4.2.3" => Hash::Sha512,
            _ => return None,
        })
    }

    fn digest(self, data: &[u8]) -> Vec<u8> {
        let alg = match self {
            Hash::Sha1 => &digest::SHA1_FOR_LEGACY_USE_ONLY,
            Hash::Sha256 => &digest::SHA256,
            Hash::Sha384 => &digest::SHA384,
            Hash::Sha512 => &digest::SHA512,
        };
        digest::digest(alg, data).as_ref().to_vec()
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum KeyKind {
    Rsa,
    Ecdsa,
}

/// A signature algorithm OID: its key kind and, for the combined OIDs, its hash.
/// Plain `rsaEncryption` takes its hash from the SignerInfo digestAlgorithm.
fn sig_alg(oid: &str) -> Result<(KeyKind, Option<Hash>)> {
    use Hash::*;
    use KeyKind::*;
    Ok(match oid {
        "1.2.840.113549.1.1.1" => (Rsa, None),
        "1.2.840.113549.1.1.5" => (Rsa, Some(Sha1)),
        "1.2.840.113549.1.1.11" => (Rsa, Some(Sha256)),
        "1.2.840.113549.1.1.12" => (Rsa, Some(Sha384)),
        "1.2.840.113549.1.1.13" => (Rsa, Some(Sha512)),
        "1.2.840.10045.4.3.2" => (Ecdsa, Some(Sha256)),
        "1.2.840.10045.4.3.3" => (Ecdsa, Some(Sha384)),
        "1.2.840.10045.4.3.4" => (Ecdsa, Some(Sha512)),
        _ => bail!("unsupported signature algorithm {oid}"),
    })
}

/// Check `signature` over `msg` with `key`'s public key: RSA PKCS#1 v1.5 or
/// ECDSA (DER signature) on P-256 / P-384. Anything else is refused.
fn verify_sig(key: &Cert, kind: KeyKind, hash: Hash, msg: &[u8], signature: &[u8]) -> Result<()> {
    let bad = || anyhow::anyhow!("bad signature");
    match (kind, key.spki_alg.as_str(), key.spki_curve.as_deref()) {
        (KeyKind::Rsa, "1.2.840.113549.1.1.1", _) => {
            // SHA-1 is allowed because a pinned root's self-signature may use
            // it (DigiCert Assured ID); every cert here is pinned by us anyway.
            let alg = match hash {
                Hash::Sha1 => &sig::RSA_PKCS1_2048_8192_SHA1_FOR_LEGACY_USE_ONLY,
                Hash::Sha256 => &sig::RSA_PKCS1_2048_8192_SHA256,
                Hash::Sha384 => &sig::RSA_PKCS1_2048_8192_SHA384,
                Hash::Sha512 => &sig::RSA_PKCS1_2048_8192_SHA512,
            };
            UnparsedPublicKey::new(alg, key.spki_key)
                .verify(msg, signature)
                .map_err(|_| bad())
        }
        (KeyKind::Ecdsa, "1.2.840.10045.2.1", Some("1.2.840.10045.3.1.7")) => {
            let alg = match hash {
                Hash::Sha256 => &sig::ECDSA_P256_SHA256_ASN1,
                Hash::Sha384 => &sig::ECDSA_P256_SHA384_ASN1,
                _ => bail!("P-256 with {hash:?} is not supported"),
            };
            UnparsedPublicKey::new(alg, key.spki_key)
                .verify(msg, signature)
                .map_err(|_| bad())
        }
        (KeyKind::Ecdsa, "1.2.840.10045.2.1", Some("1.3.132.0.34")) => {
            // p384 rather than ring: ring has no P-384 with SHA-512, which is
            // what freetsa signs with. Verifying the prehash covers every hash.
            use p384::ecdsa::signature::hazmat::PrehashVerifier;
            ensure!(hash != Hash::Sha1, "ECDSA with SHA-1 is not supported");
            let vk = p384::ecdsa::VerifyingKey::from_sec1_bytes(key.spki_key)
                .map_err(|_| anyhow::anyhow!("bad P-384 public key"))?;
            let s = p384::ecdsa::Signature::from_der(signature).map_err(|_| bad())?;
            vk.verify_prehash(&hash.digest(msg), &s).map_err(|_| bad())
        }
        (k, alg, curve) => bail!("unsupported key {alg} {curve:?} for {k:?}"),
    }
}

/// The parts of a TimeStampResp the checks need, all borrowed from it.
struct Token<'a> {
    tst_der: &'a [u8],
    imprint_alg: Option<Hash>,
    imprint: &'a [u8],
    gen_time: &'a [u8],
    sid_issuer: &'a [u8],
    sid_serial: &'a [u8],
    digest_alg: Hash,
    signed_attrs: &'a [u8],
    attrs: Vec<(String, der::Tlv<'a>)>,
    sig_alg: (KeyKind, Option<Hash>),
    signature: &'a [u8],
}

impl<'a> Token<'a> {
    /// First value of a signed attribute.
    fn attr(&self, oid: &str) -> Option<der::Tlv<'a>> {
        self.attrs.iter().find(|(o, _)| o == oid).map(|(_, v)| *v)
    }
}

fn parse(tsr: &[u8]) -> Result<Token<'_>> {
    use der::{at, kids, oid, top};
    let resp = kids(top(tsr)?)?;
    let status = at(&kids(at(&resp, 0)?)?, 0)?;
    ensure!(
        status.tag == 0x02 && matches!(status.body, [0] | [1]),
        "PKIStatus is not granted"
    );
    let content_info = kids(at(&resp, 1).context("response carries no token")?)?;
    ensure!(
        oid(at(&content_info, 0)?)? == "1.2.840.113549.1.7.2",
        "token is not CMS SignedData"
    );
    let sd = kids(at(&kids(at(&content_info, 1)?)?, 0)?)?;
    let econ = kids(at(&sd, 2)?)?;
    ensure!(
        oid(at(&econ, 0)?)? == "1.2.840.113549.1.9.16.1.4",
        "eContentType is not TSTInfo"
    );
    let tst_der = der::octets(at(&kids(at(&econ, 1)?)?, 0)?)?;
    let tst = kids(top(tst_der)?)?;
    let mi = kids(at(&tst, 2)?)?;

    let signer_infos = *sd.last().unwrap();
    ensure!(signer_infos.tag == 0x31, "no signerInfos SET");
    let si = kids(at(&kids(signer_infos)?, 0)?)?;
    let sid = at(&si, 1)?;
    ensure!(sid.tag == 0x30, "sid is not issuerAndSerialNumber");
    let sid = kids(sid)?;
    let attrs_t = *si
        .iter()
        .find(|t| t.tag == 0xa0)
        .context("no signed attributes")?;
    let attrs = kids(attrs_t)?
        .into_iter()
        .map(|a| {
            let a = kids(a)?;
            Ok((oid(at(&a, 0)?)?, at(&kids(at(&a, 1)?)?, 0)?))
        })
        .collect::<Result<Vec<_>>>()?;
    let after = &si[si.iter().position(|t| t.tag == 0xa0).unwrap() + 1..];
    let gen_time = at(&tst, 4)?;
    ensure!(gen_time.tag == 0x18, "genTime is not GeneralizedTime");

    Ok(Token {
        tst_der,
        imprint_alg: Hash::from_oid(&oid(at(&kids(at(&mi, 0)?)?, 0)?)?),
        imprint: der::octets(at(&mi, 1)?)?,
        gen_time: gen_time.body,
        sid_issuer: at(&sid, 0)?.raw,
        sid_serial: at(&sid, 1)?.body,
        digest_alg: Hash::from_oid(&oid(at(&kids(at(&si, 2)?)?, 0)?)?)
            .context("unsupported digestAlgorithm")?,
        signed_attrs: attrs_t.raw,
        attrs,
        sig_alg: sig_alg(&oid(at(&kids(at(after, 0)?)?, 0)?)?)?,
        signature: der::octets(at(after, 1)?)?,
    })
}

/// ESS signingCertificate(V2): each one present must name the signer cert by
/// hash, and at least one must be present.
fn ess_matches(t: &Token, cert_der: &[u8]) -> Result<()> {
    use der::{at, kids, oid};
    let v2 = t.attr(OID_SIGNING_CERT_V2);
    let v1 = t.attr(OID_SIGNING_CERT);
    ensure!(
        v1.is_some() || v2.is_some(),
        "no ESS signingCertificate attribute"
    );
    if let Some(a) = v2 {
        let id = kids(at(&kids(a)?, 0)?)?; // certs SEQUENCE OF ESSCertIDv2
        let f = kids(at(&id, 0)?)?;
        // hashAlgorithm is DEFAULT sha256, so it may be absent.
        let (alg, h) = if at(&f, 0)?.tag == 0x30 {
            let alg = Hash::from_oid(&oid(at(&kids(at(&f, 0)?)?, 0)?)?)
                .context("unsupported ESS hash")?;
            (alg, at(&f, 1)?)
        } else {
            (Hash::Sha256, at(&f, 0)?)
        };
        ensure!(
            der::octets(h)? == alg.digest(cert_der).as_slice(),
            "ESS signingCertificateV2 does not match the signer cert"
        );
    }
    if let Some(a) = v1 {
        let id = kids(at(&kids(at(&kids(a)?, 0)?)?, 0)?)?;
        ensure!(
            der::octets(at(&id, 0)?)? == Hash::Sha1.digest(cert_der).as_slice(),
            "ESS signingCertificate does not match the signer cert"
        );
    }
    Ok(())
}

/// One X.509 certificate from the pinned chain, borrowed from its DER.
struct Cert<'a> {
    der: &'a [u8],
    tbs: &'a [u8],
    sig_alg: (KeyKind, Option<Hash>),
    signature: &'a [u8],
    serial: &'a [u8],
    issuer: &'a [u8],
    subject: &'a [u8],
    not_before: i64,
    not_after: i64,
    spki_alg: String,
    spki_curve: Option<String>,
    spki_key: &'a [u8],
    eku: Vec<String>,
}

impl<'a> Cert<'a> {
    fn parse(cert_der: &'a [u8]) -> Result<Self> {
        use der::{at, bits, kids, oid, top};
        let c = kids(top(cert_der)?)?;
        let tbs_t = at(&c, 0)?;
        let mut tbs = kids(tbs_t)?;
        if at(&tbs, 0)?.tag == 0xa0 {
            tbs.remove(0); // explicit version
        }
        let validity = kids(at(&tbs, 3)?)?;
        let spki = kids(at(&tbs, 5)?)?;
        let spki_alg = kids(at(&spki, 0)?)?;
        let mut eku = Vec::new();
        if let Some(ext) = tbs.iter().find(|t| t.tag == 0xa3) {
            for e in kids(at(&kids(*ext)?, 0)?)? {
                let e = kids(e)?;
                if oid(at(&e, 0)?)? == "2.5.29.37" {
                    let v = der::octets(*e.last().unwrap())?;
                    for o in kids(top(v)?)? {
                        eku.push(oid(o)?);
                    }
                }
            }
        }
        let alg = sig_alg(&oid(at(&kids(at(&c, 1)?)?, 0)?)?)?;
        ensure!(alg.1.is_some(), "cert signature algorithm names no hash");
        Ok(Cert {
            der: cert_der,
            tbs: tbs_t.raw,
            sig_alg: alg,
            signature: bits(at(&c, 2)?)?,
            serial: at(&tbs, 0)?.body,
            issuer: at(&tbs, 2)?.raw,
            subject: at(&tbs, 4)?.raw,
            not_before: x509_time(at(&validity, 0)?)?,
            not_after: x509_time(at(&validity, 1)?)?,
            spki_alg: oid(at(&spki_alg, 0)?)?,
            spki_curve: spki_alg.get(1).and_then(|p| oid(*p).ok()),
            spki_key: bits(at(&spki, 1)?)?,
            eku,
        })
    }

    /// `self`'s signature checks under `issuer`'s key.
    fn verified_by(&self, issuer: &Cert) -> Result<()> {
        let hash = self.sig_alg.1.context("no hash")?;
        verify_sig(issuer, self.sig_alg.0, hash, self.tbs, self.signature)
    }
}

/// Every CERTIFICATE block of a PEM bundle, as DER.
fn pem_certs(pem: &str) -> Result<Vec<Vec<u8>>> {
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";
    let mut out = Vec::new();
    let mut rest = pem;
    while let Some(i) = rest.find(BEGIN) {
        let body = &rest[i + BEGIN.len()..];
        let j = body.find(END).context("unterminated PEM certificate")?;
        let b64: String = body[..j].split_whitespace().collect();
        out.push(
            base64::engine::general_purpose::STANDARD
                .decode(b64)
                .context("bad base64 in PEM")?,
        );
        rest = &body[j + END.len()..];
    }
    ensure!(!out.is_empty(), "pinned chain holds no certificates");
    Ok(out)
}

/// A cert validity time: UTCTime (YYMMDDHHMMSSZ, 1950–2049) or GeneralizedTime.
fn x509_time(t: der::Tlv) -> Result<i64> {
    match t.tag {
        0x17 => {
            let yy: u32 =
                std::str::from_utf8(t.body.get(..2).context("short UTCTime")?)?.parse()?;
            let century: &[u8] = if yy < 50 { b"20" } else { b"19" };
            generalized_time(&[century, t.body].concat())
        }
        0x18 => generalized_time(t.body),
        _ => bail!("validity is not a time"),
    }
}

/// GeneralizedTime `YYYYMMDDHHMMSS[.f…]Z` as unix seconds; the fraction is dropped.
fn generalized_time(b: &[u8]) -> Result<i64> {
    let s = std::str::from_utf8(b)?;
    let ok = s.len() >= 15
        && s.ends_with('Z')
        && s.as_bytes()[..14].iter().all(u8::is_ascii_digit)
        && (s.len() == 15
            || (s.as_bytes()[14] == b'.'
                && s.len() > 16
                && s.as_bytes()[15..s.len() - 1].iter().all(u8::is_ascii_digit)));
    ensure!(ok, "not UTC GeneralizedTime: {s}");
    let n = |r: std::ops::Range<usize>| s[r].parse::<i64>().unwrap();
    let (y, mo, d, h, mi, sec) = (n(0..4), n(4..6), n(6..8), n(8..10), n(10..12), n(12..14));
    ensure!(
        (1..=12).contains(&mo) && (1..=31).contains(&d) && h < 24 && mi < 60 && sec < 61,
        "time out of range: {s}"
    );
    Ok(days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + sec)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (H. Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// A minimal DER reader, as in the spike: tag byte, definite lengths only.
mod der {
    use anyhow::{bail, ensure, Context, Result};

    #[derive(Debug, Clone, Copy)]
    pub struct Tlv<'a> {
        pub tag: u8,
        /// The whole element, header included.
        pub raw: &'a [u8],
        pub body: &'a [u8],
    }

    /// The element at the start of `buf`, and the bytes after it.
    fn next(buf: &[u8]) -> Result<(Tlv<'_>, &[u8])> {
        let tag = *buf.first().context("DER: empty")?;
        ensure!(tag & 0x1f != 0x1f, "DER: multi-byte tags not supported");
        let first = *buf.get(1).context("DER: no length")?;
        let (len, hdr) = match first {
            0x80 => bail!("DER: indefinite length (BER) not supported"),
            l if l < 0x80 => (l as usize, 2),
            l => {
                let n = (l & 0x7f) as usize;
                ensure!(n <= 4, "DER: length too long");
                let bytes = buf.get(2..2 + n).context("DER: truncated length")?;
                (
                    bytes.iter().fold(0usize, |a, b| a << 8 | *b as usize),
                    2 + n,
                )
            }
        };
        let end = hdr
            .checked_add(len)
            .filter(|e| *e <= buf.len())
            .context("DER: element overruns buffer")?;
        let t = Tlv {
            tag,
            raw: &buf[..end],
            body: &buf[hdr..end],
        };
        Ok((t, &buf[end..]))
    }

    /// `buf` must be exactly one element.
    pub fn top(buf: &[u8]) -> Result<Tlv<'_>> {
        let (t, rest) = next(buf)?;
        ensure!(rest.is_empty(), "DER: trailing bytes");
        Ok(t)
    }

    /// The children of a constructed element.
    pub fn kids(t: Tlv<'_>) -> Result<Vec<Tlv<'_>>> {
        ensure!(
            t.tag & 0x20 != 0,
            "DER: tag {:#x} is not constructed",
            t.tag
        );
        let mut out = Vec::new();
        let mut rest = t.body;
        while !rest.is_empty() {
            let (c, r) = next(rest)?;
            out.push(c);
            rest = r;
        }
        Ok(out)
    }

    pub fn at<'a>(v: &[Tlv<'a>], i: usize) -> Result<Tlv<'a>> {
        v.get(i)
            .copied()
            .with_context(|| format!("DER: missing field {i}"))
    }

    pub fn oid(t: Tlv) -> Result<String> {
        ensure!(t.tag == 0x06 && !t.body.is_empty(), "DER: not an OID");
        let b = t.body;
        let mut out = format!("{}.{}", b[0] / 40, b[0] % 40);
        let mut v: u64 = 0;
        for x in &b[1..] {
            v = v << 7 | (x & 0x7f) as u64;
            if x & 0x80 == 0 {
                out.push_str(&format!(".{v}"));
                v = 0;
            }
        }
        Ok(out)
    }

    pub fn octets(t: Tlv<'_>) -> Result<&[u8]> {
        ensure!(t.tag == 0x04, "DER: not an OCTET STRING");
        Ok(t.body)
    }

    /// BIT STRING content with no unused bits.
    pub fn bits(t: Tlv<'_>) -> Result<&[u8]> {
        ensure!(
            t.tag == 0x03 && t.body.first() == Some(&0),
            "DER: not a whole-byte BIT STRING"
        );
        Ok(&t.body[1..])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        assert_eq!(generalized_time(b"19700101000000Z").unwrap(), 0);
        assert_eq!(
            generalized_time(b"20261028015524.123Z").unwrap(),
            1_793_152_524
        );
        assert!(generalized_time(b"20261028015524+0100").is_err());
        let utc = |s: &[u8]| {
            x509_time(der::Tlv {
                tag: 0x17,
                raw: s,
                body: s,
            })
        };
        assert_eq!(utc(b"491231235959Z").unwrap(), 2_524_607_999);
        assert_eq!(utc(b"500101000000Z").unwrap(), -631_152_000);
    }
}
