//! The `wx-receipt-verify` binary; the CLI itself is `wx_receipt_verify::cli`.

fn main() -> std::process::ExitCode {
    wx_receipt_verify::cli::main("wx-receipt-verify")
}
