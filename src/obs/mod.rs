//! The parts of wx-obs the verifier's check 9 needs: METAR report lines
//! (metar), the NWS CLI (cli), the °C/°F conversion (convert) and the running
//! max over a climate day (running_max). They live here, not in wx-obs, so
//! `wx-receipt-verify` has no path dependency; wx-obs re-exports this tree
//! (`pub use wx_receipt_verify::obs::…`), so there is one parser.

// At rust-version 1.85 clippy is silent here; once the pin reaches 1.87 it
// suggests `is_multiple_of` on the leap-year test. Allowed ahead of that rather
// than changing working logic written for an older floor.
#[allow(clippy::manual_is_multiple_of)]
pub mod cli;
pub mod convert;
pub mod metar;
pub mod running_max;
