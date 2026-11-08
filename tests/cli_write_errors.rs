//! Write errors on stdout (spec 027 wx-receipt-oss R017): a broken pipe is a
//! quiet exit 0, any other write error exits 2 without the usage text.

use std::io::{self, ErrorKind, Write};
use std::path::Path;

use wx_receipt_verify::cli::run;

/// A stdout whose every write fails with `kind`.
struct Failing(ErrorKind);

impl Write for Failing {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::from(self.0))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// An empty but loadable archive: `score` on it prints a header and a JSON
/// line, so the run reaches the writes (no receipts needed for that).
fn empty_archive() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir(d.path().join("receipts")).unwrap();
    std::fs::write(d.path().join("manifest.json"), "[]").unwrap();
    std::fs::write(d.path().join("ledger.json"), r#"{"net_cents":0}"#).unwrap();
    d
}

fn score_args(d: &Path) -> Vec<String> {
    vec!["score".into(), d.display().to_string()]
}

/// Guard for the two tests below: the archive loads and the report is
/// written, so their exit codes come from the write arm, not the usage arm.
#[test]
fn score_on_empty_archive_writes_and_exits_0() {
    let d = empty_archive();
    let mut out = Vec::new();
    assert_eq!(run("wx-receipt-verify", &score_args(d.path()), &mut out), 0);
    assert!(!out.is_empty());
}

#[test]
fn broken_pipe_on_stdout_exits_0() {
    let d = empty_archive();
    assert_eq!(
        run(
            "wx-receipt-verify",
            &score_args(d.path()),
            &mut Failing(ErrorKind::BrokenPipe)
        ),
        0
    );
}

#[test]
fn other_write_error_on_stdout_exits_2() {
    let d = empty_archive();
    assert_eq!(
        run(
            "wx-receipt-verify",
            &score_args(d.path()),
            &mut Failing(ErrorKind::PermissionDenied)
        ),
        2
    );
}
