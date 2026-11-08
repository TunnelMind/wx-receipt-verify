//! `<prog> verify <dir> --pubkey <hex> --stations <json> --tsa-chain <pem>…`: the offline
//! verifier (src/verify.rs). Exit 0 when no `FAIL` line is left, 1 on any
//! failure (one `FAIL <check> …` line each), 2 when the archive or the
//! arguments cannot be read. A failure a valid signed erratum in
//! `<dir>/errata/` covers prints as `KNOWN …` instead and does not count.
//! `--as-of <RFC 3339 UTC>` is the verifier's "now": a slot with no commit
//! whose decision_time + 300 s is after it prints `NOTE pending: slot <id>`
//! instead of failing check 3. Without it nothing changes.
//!
//! `<prog> score <dir>`: the public scoring helper (src/score.rs). Brier
//! and log score of every peak block against the settled NWS high; no key
//! needed, since it is arithmetic over the archive, not a trust check. Exit 0
//! (skipped reveals included), 2 when the archive cannot be read.
//!
//! Shared by both binaries, `wx-receipt-verify` and `wx-receipt` (spec 027
//! wx-receipt-oss O003): one implementation, the program name passed in so the
//! usage and error text name the binary that was run.

use std::io::{ErrorKind, Write};
use std::path::Path;
use std::process::ExitCode;

use anyhow::{bail, ensure, Context, Result};

const USAGE: &str = "usage: wx-receipt-verify verify <dir> --pubkey <ed25519 hex> --stations <json> [--tsa-chain <pem>…] [--founder-pubkey <ed25519 hex>] [--accept-unproven-schedule] [--as-of <RFC 3339 UTC>]\n  <dir>/errata/*.json         optional signed errata; a covered FAIL prints as KNOWN\n  --stations <json>           {\"<station>\": {\"nws_settled\": bool, \"lst_offset_hours\"?: int}, …}:\n                              nws_settled: which settlements check 6 recomputes;\n                              lst_offset_hours: the station's standard-time UTC offset\n                              check 9 recomputes the running max with\n  --founder-pubkey <hex>      the pinned founder key a station_map rule_change's founder_sig\n                              must verify under; without it any station_map fails check 3\n  --accept-unproven-schedule  a schedule with no TSA token is a NOTE, not a check 3 failure\n                              (test / offline dev only, never for the public record)\n  --as-of <RFC 3339 UTC>      e.g. 2026-11-04T18:00:00Z, given by you, never read from the archive:\n                              a slot with no commit whose decision_time + 300 s is after it is\n                              NOTE pending, not a check 3 failure\n       wx-receipt-verify score <dir>";

/// The binary's whole `main`: process args in, process exit code out.
pub fn main(prog: &str) -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    ExitCode::from(run(prog, &args, &mut std::io::stdout().lock()))
}

/// Run one command, writing its report to `out`; returns the exit code (0 ok,
/// 1 a check failed, 2 unreadable archive or bad arguments, told on stderr).
pub fn run(prog: &str, args: &[String], out: &mut impl Write) -> u8 {
    match run_cmd(args, out) {
        Ok(true) => 0,
        Ok(false) => 1,
        Err(e) => match e.downcast_ref::<OutErr>() {
            // A broken pipe means the reader closed early (`… | head`): it
            // has all the output it wanted, so exit quietly with 0.
            Some(o) if o.0.kind() == ErrorKind::BrokenPipe => 0,
            // Any other failure to write the report is not a usage mistake.
            Some(o) => {
                eprintln!("{prog}: write error: {}", o.0);
                2
            }
            None => {
                // USAGE is written for wx-receipt-verify; name the binary actually run.
                eprintln!(
                    "{prog}: {e:#}\n{}",
                    USAGE.replace("wx-receipt-verify", prog)
                );
                2
            }
        },
    }
}

/// A failed write to `out`, tagged so `run` can tell it from an unreadable
/// archive (whose error chain may also hold an io::Error).
#[derive(Debug)]
struct OutErr(std::io::Error);

impl std::fmt::Display for OutErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for OutErr {}

fn run_cmd(args: &[String], out: &mut impl Write) -> Result<bool> {
    let Some((cmd, rest)) = args.split_first() else {
        bail!("no command");
    };
    if cmd == "score" {
        let [dir] = rest else {
            bail!("score takes exactly one archive directory");
        };
        let a = crate::verify::Archive::load(Path::new(dir))?;
        write!(out, "{}", crate::score::score(&a).render()).map_err(OutErr)?;
        return Ok(true);
    }
    ensure!(cmd == "verify", "unknown command {cmd:?}");
    let (mut dir, mut pubkey, mut stations, mut chains) = (None, None, None, Vec::new());
    let mut accept_unproven = false;
    let mut founder = None;
    let mut as_of = None;
    // --tsa-chain takes every following argument up to the next flag.
    let mut in_chains = false;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--pubkey" => {
                pubkey = Some(it.next().context("--pubkey needs a value")?);
                in_chains = false;
            }
            "--stations" => {
                stations = Some(it.next().context("--stations needs a value")?);
                in_chains = false;
            }
            "--founder-pubkey" => {
                founder = Some(it.next().context("--founder-pubkey needs a value")?);
                in_chains = false;
            }
            "--as-of" => {
                let t = it.next().context("--as-of needs a value")?;
                as_of = Some(crate::verify::unix_ms(t).with_context(|| {
                    format!("--as-of {t:?} is not a canonical RFC 3339 UTC timestamp")
                })?);
                in_chains = false;
            }
            "--tsa-chain" => in_chains = true,
            "--accept-unproven-schedule" => {
                accept_unproven = true;
                in_chains = false;
            }
            s if s.starts_with("--") => bail!("unknown flag {s}"),
            s if in_chains => chains.push(
                std::fs::read_to_string(s).with_context(|| format!("reading TSA chain {s}"))?,
            ),
            s if dir.is_none() => dir = Some(s),
            s => bail!("unexpected argument {s:?}"),
        }
    }
    let dir = dir.context("no archive directory")?;
    let pubkey = pubkey.context("--pubkey is required")?;
    let is_key = |k: &str| k.len() == 64 && k.bytes().all(|c| c.is_ascii_hexdigit());
    ensure!(is_key(pubkey), "--pubkey must be 32 bytes of hex");
    // Optional: only a station_map rule_change needs it, and that fails
    // closed without it (verify::check_station_maps).
    if let Some(f) = founder {
        ensure!(is_key(f), "--founder-pubkey must be 32 bytes of hex");
    }
    // Required: without it check 6 would have to trust the agent's own
    // settlement_source label, so verification fails closed.
    let stations = stations.context("--stations is required")?;
    let stations = std::fs::read_to_string(stations)
        .with_context(|| format!("reading stations file {stations}"))?;
    let stations = crate::verify::parse_stations(&stations)?;
    let report = crate::verify::verify_as_of(
        &crate::verify::Archive::load(Path::new(dir))?,
        pubkey,
        &chains,
        &stations,
        accept_unproven,
        founder.map(String::as_str),
        as_of,
    );
    // Order: notes, then errata-covered KNOWN rows, then real failures.
    for line in report
        .notes
        .iter()
        .chain(&report.known)
        .chain(&report.failures)
    {
        writeln!(out, "{line}").map_err(OutErr)?;
    }
    if report.failures.is_empty() {
        match report.known.len() {
            0 => writeln!(out, "OK: all ten checks pass").map_err(OutErr)?,
            n => {
                writeln!(out, "OK: all ten checks pass ({n} known, see errata)").map_err(OutErr)?
            }
        }
    }
    Ok(report.failures.is_empty())
}
