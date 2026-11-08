//! Lock-drift guard (R016, moved into `cargo test` by R019).
//!
//! The crate's own Cargo.lock is what outsiders build with (`cargo --locked`
//! in the public repo); the workspace Cargo.lock is what tm-weather CI builds
//! and tests with. If a package shared by both drifts to a different version,
//! CI's green says nothing about the crate as published. This is the same
//! check as scripts/standalone-check.sh, here so `cargo test --locked` runs it.
//! Std only, offline: the locks are scanned line by line, as the script's awk.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// (name, version) per `[[package]]`, in the order cargo writes them
/// (`name` first, then `version`). The lock's top-level `version = 4` sits
/// before any package, so it is never paired with a name.
fn pairs(lock: &str) -> BTreeSet<(String, String)> {
    let mut out = BTreeSet::new();
    let mut name: Option<String> = None;
    for line in lock.lines() {
        if line.starts_with("[[package]]") {
            name = None;
        } else if let Some(v) = line.strip_prefix("name = ") {
            name = Some(v.trim_matches('"').to_string());
        } else if let Some(v) = line.strip_prefix("version = ") {
            if let Some(n) = name.take() {
                out.insert((n, v.trim_matches('"').to_string()));
            }
        }
    }
    out
}

/// Drift = a crate-lock pair the workspace lock lacks, for a name the
/// workspace does lock. The workspace may also hold other versions of a name
/// (e.g. syn 2 next to syn 3 for other members); that alone is not drift, and
/// names in only one lock are fine (the workspace has more crates; the crate
/// lock has its dev-deps). Lines read "name crate_ver->workspace_ver[,ver...]".
fn drift(crate_lock: &str, ws_lock: &str) -> Vec<String> {
    let ws = pairs(ws_lock);
    let mut ws_versions: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (n, v) in &ws {
        ws_versions.entry(n).or_default().push(v);
    }
    pairs(crate_lock)
        .iter()
        .filter(|p| !ws.contains(*p))
        .filter_map(|(n, v)| {
            let wv = ws_versions.get(n.as_str())?;
            Some(format!("{n} {v}->{}", wv.join(",")))
        })
        .collect()
}

#[test]
fn crate_lock_matches_workspace_lock() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let ws_path = dir.join("../Cargo.lock");
    // A copy of the public repo (or standalone-check.sh's copy) has no
    // workspace above it, so there is nothing to compare against.
    let Ok(ws_lock) = std::fs::read_to_string(&ws_path) else {
        eprintln!(
            "NOTE: no workspace Cargo.lock at {}; lock-drift check skipped",
            ws_path.display()
        );
        return;
    };
    let crate_lock = std::fs::read_to_string(dir.join("Cargo.lock")).expect("crate Cargo.lock");
    let d = drift(&crate_lock, &ws_lock);
    assert!(
        d.is_empty(),
        "wx-receipt-verify/Cargo.lock drifted from the workspace Cargo.lock (name crate->workspace):\n  {}",
        d.join("\n  ")
    );
}

/// The comparison itself, on literal lock text, so a broken parser or filter
/// fails here even while the real locks happen to agree.
#[test]
fn drift_logic() {
    let crate_lock = r#"version = 4

[[package]]
name = "syn"
version = "2.0.1"

[[package]]
name = "serde"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "only-in-crate"
version = "0.1.0"
"#;
    let ws_lock = r#"version = 4

[[package]]
name = "syn"
version = "2.0.1"

[[package]]
name = "syn"
version = "3.0.0"

[[package]]
name = "serde"
version = "1.0.9"

[[package]]
name = "only-in-ws"
version = "9.9.9"
"#;
    // (a) serde 1.0.0 missing from the workspace is reported by name;
    // (b) the extra syn 3 in the workspace is not drift;
    // (c) only-in-crate, absent from the workspace, is not drift.
    assert_eq!(drift(crate_lock, ws_lock), vec!["serde 1.0.0->1.0.9"]);
    assert!(drift(crate_lock, crate_lock).is_empty());
}
