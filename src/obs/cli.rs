//! The NWS Daily Climate Report (CLI): the climate day, its MAXIMUM and LST
//! time, which issuance this is, and the WMO BBB marker.
//!
//! The layout claims below were read off the two real HGX/HOU products in
//! tests/fixtures (cli/HOU.json, issued 2026-10-27T07:25Z; cli_prelim/HOU.json,
//! issued 2026-10-27T21:25Z; read 2026-10-27). The NWS directive for the CLI
//! (NWSI 10-1004) was not read, so the layout is UNVERIFIED for other offices
//! beyond what each line says.

/// Which issuance a CLI is. Rule, from the fixtures and wx-archive's
/// `is_preliminary` (which records the same "VALID ... AS OF" line across
/// HGX, LOT, OKX, MFL, EWX and BOU, read 2026-10-28):
///
/// - `Prelim`: has `VALID TODAY AS OF <hhmm> PM` (cli_prelim fixture:
///   `VALID TODAY AS OF 0400 PM LOCAL TIME.`, a `TODAY` block).
/// - `Morning`: the same line with an AM time, the 06-07 AM partial some
///   offices (EWX, BOU) issue (cli_morning fixture, EWX's CLIAUS issued
///   2026-10-27T12:34Z: `VALID AS OF 0700 AM LOCAL TIME.`, no `TODAY`).
/// - `Final`: no such line (cli fixture: the 2:25 AM report for the day
///   before, a `YESTERDAY` block).
///
/// A corrected final (`CCA`) is still `Final`; see `bbb`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliKind {
    Prelim,
    Morning,
    Final,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClimateDay {
    pub year: u16,
    pub month: u8,
    pub day: u8,
}

/// What the MAXIMUM's value column held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaxTag {
    /// A number; `max_f` is `Some`.
    Value,
    /// `MM` ("MM INDICATES DATA IS MISSING", the product's legend): the
    /// line is there but the office has no value, unlike a product with no
    /// MAXIMUM line at all (`CliError::NoMaximum`). Any other unreadable
    /// token lands here too (none seen; UNVERIFIED).
    MaxMissing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cli {
    /// From `...<STATION> CLIMATE SUMMARY FOR OCTOBER 26 2026...`.
    pub climate_day: ClimateDay,
    /// MAXIMUM, whole °F. `None` for `MM` ("MM INDICATES DATA IS MISSING",
    /// the product's own legend), never 0.
    pub max_f: Option<i32>,
    /// Whether `max_f` is a value or `MM`; see `MaxTag`.
    pub max_tag: MaxTag,
    /// The value carried a trailing `R` ("R INDICATES RECORD WAS SET OR
    /// TIED", the legend), e.g. `92R`.
    pub record: bool,
    /// The MAXIMUM's time, local standard time, as (hour 0-23, minute). The
    /// column is headed `(LST)` in the product; LST all year per
    /// https://www.weather.gov/lox/asostemperature (read 2026-10-27).
    pub max_time_lst: Option<(u8, u8)>,
    pub kind: CliKind,
    /// The WMO heading's BBB group after the date-time (`RRx` delayed, `CCx`
    /// correction, `AAx` amendment, per https://www.weather.gov/tg/head, read
    /// 2026-10-27), e.g. `CCA`; `None` when the heading has none.
    pub bbb: Option<String>,
}

/// A CLI read from the api.weather.gov product JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliProduct {
    /// `issuanceTime` as sent, e.g. `2026-10-27T07:25:00+00:00`.
    pub issuance_time: String,
    pub report: Cli,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    Json(String),
    MissingField(&'static str),
    /// No `T1T2A1A2ii CCCC YYGGgg [BBB]` heading line in the first lines.
    NoWmoHeading,
    NoClimateDay,
    /// No MAXIMUM line in the `TEMPERATURE (F)` block (as opposed to `MM`).
    NoMaximum,
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CliError::Json(e) => write!(f, "product JSON: {e}"),
            CliError::MissingField(k) => write!(f, "product JSON has no {k}"),
            CliError::NoWmoHeading => write!(f, "no WMO heading line"),
            CliError::NoClimateDay => write!(f, "no CLIMATE SUMMARY FOR <date> line"),
            CliError::NoMaximum => write!(f, "no MAXIMUM line under TEMPERATURE (F)"),
        }
    }
}

impl std::error::Error for CliError {}

/// Parse the api.weather.gov `/products/{id}` JSON (fields `issuanceTime`,
/// `productText`, as in the fixtures).
pub fn parse_product_json(json: &str) -> Result<CliProduct, CliError> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| CliError::Json(e.to_string()))?;
    let field = |k: &'static str| v[k].as_str().ok_or(CliError::MissingField(k));
    Ok(CliProduct {
        issuance_time: field("issuanceTime")?.to_owned(),
        report: parse_text(field("productText")?)?,
    })
}

/// Parse the plain product text.
pub fn parse_text(text: &str) -> Result<Cli, CliError> {
    let bbb = wmo_bbb(text)?;
    let climate_day = text
        .lines()
        .find_map(climate_day)
        .ok_or(CliError::NoClimateDay)?;
    let Maximum {
        max_f,
        record,
        time,
    } = maximum(text)?;
    Ok(Cli {
        climate_day,
        max_f,
        max_tag: if max_f.is_some() {
            MaxTag::Value
        } else {
            MaxTag::MaxMissing
        },
        record,
        max_time_lst: time,
        kind: kind(text),
        bbb,
    })
}

/// The heading is the line after `000` in the fixtures (`CDUS44 KHGX 270725`);
/// shape per https://www.weather.gov/tg/head (read 2026-10-27): six-character
/// TTAAii, four-letter CCCC, six-digit YYGGgg, optional three-letter BBB.
fn wmo_bbb(text: &str) -> Result<Option<String>, CliError> {
    for line in text.lines().take(6) {
        let t: Vec<&str> = line.split_whitespace().collect();
        let heading = (t.len() == 3 || t.len() == 4)
            && t[0].len() == 6
            && t[1].len() == 4
            && t[2].len() == 6
            && t[2].bytes().all(|b| b.is_ascii_digit());
        if heading {
            return Ok(t
                .get(3)
                .filter(|b| b.len() == 3 && b.bytes().all(|c| c.is_ascii_uppercase()))
                .map(|b| (*b).to_owned()));
        }
    }
    Err(CliError::NoWmoHeading)
}

const MONTHS: [&str; 12] = [
    "JANUARY",
    "FEBRUARY",
    "MARCH",
    "APRIL",
    "MAY",
    "JUNE",
    "JULY",
    "AUGUST",
    "SEPTEMBER",
    "OCTOBER",
    "NOVEMBER",
    "DECEMBER",
];

/// `...THE HOUSTON/HOBBY AIRPORT CLIMATE SUMMARY FOR OCTOBER 26 2026...`
fn climate_day(line: &str) -> Option<ClimateDay> {
    let (_, rest) = line.split_once("CLIMATE SUMMARY FOR ")?;
    let mut t = rest.split_whitespace().map(|w| w.trim_end_matches('.'));
    let name = t.next()?;
    let month = MONTHS.iter().position(|m| *m == name)? as u8 + 1;
    let day: u8 = t.next()?.parse().ok()?;
    let year: u16 = t.next()?.parse().ok()?;
    // Days in the month, Gregorian leap rule for February.
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let last = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    (1..=last)
        .contains(&day)
        .then_some(ClimateDay { year, month, day })
}

/// MAXIMUM °F, its record flag and its LST (hour, minute).
struct Maximum {
    max_f: Option<i32>,
    record: bool,
    time: Option<(u8, u8)>,
}

/// The first `MAXIMUM` line after `TEMPERATURE (F)`, e.g.
/// `  MAXIMUM         82  12:55 PM  90    2010  79 ...` (BOU writes the
/// time without a colon, `1221 AM`; see `lst_time`). A trailing `R`
/// marks a record ("R INDICATES RECORD WAS SET OR TIED", the legend) and a
/// leading `-` a negative number (same legend).
fn maximum(text: &str) -> Result<Maximum, CliError> {
    let line = text
        .lines()
        .skip_while(|l| l.trim() != "TEMPERATURE (F)")
        .find(|l| l.trim_start().starts_with("MAXIMUM"))
        .ok_or(CliError::NoMaximum)?;
    let t: Vec<&str> = line.split_whitespace().collect();
    let raw = t.get(1).copied().unwrap_or("");
    let digits = raw.strip_suffix('R');
    let max_f = digits.unwrap_or(raw).parse::<i32>().ok();
    // No value means no time: with `MM` the time column is `MM` too and the
    // next tokens are the record value and year, never a time.
    let time = match (max_f, t.get(2), t.get(3)) {
        (Some(_), Some(hm), Some(ap)) => lst_time(hm, ap),
        _ => None,
    };
    Ok(Maximum {
        max_f,
        record: max_f.is_some() && digits.is_some(),
        time,
    })
}

/// `12:55` `PM` → (12, 55); `12:06` `AM` → (0, 6). BOU's CLI writes the
/// same column as four digits, `1221 AM` → (0, 21) (hand-built KBOU
/// fixture). In that colon-less form a single-digit hour has 3 digits,
/// `259 PM` → (14, 59): 3 or 4 digits are taken, the last two are minutes.
fn lst_time(hm: &str, ap: &str) -> Option<(u8, u8)> {
    let (h, m) = match hm.split_once(':') {
        Some(hm) => hm,
        None if (3..=4).contains(&hm.len()) && hm.bytes().all(|b| b.is_ascii_digit()) => {
            hm.split_at(hm.len() - 2)
        }
        None => return None,
    };
    if m.len() != 2 {
        return None;
    }
    let (h, m): (u8, u8) = (h.parse().ok()?, m.parse().ok()?);
    if !(1..=12).contains(&h) || m > 59 {
        return None;
    }
    match ap {
        "AM" => Some((h % 12, m)),
        "PM" => Some((h % 12 + 12, m)),
        _ => None,
    }
}

/// See `CliKind` for the rule and its evidence. A `VALID ... AS OF` line
/// whose time has neither AM nor PM is taken as `Prelim` (still in-day);
/// that case is UNVERIFIED.
fn kind(text: &str) -> CliKind {
    let Some(asof) = text.lines().find_map(|l| {
        l.trim_start()
            .starts_with("VALID")
            .then(|| l.split_once(" AS OF ").map(|(_, r)| r))
            .flatten()
    }) else {
        return CliKind::Final;
    };
    match asof.split_whitespace().nth(1) {
        Some("AM") => CliKind::Morning,
        _ => CliKind::Prelim,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn climate_day_rejects_impossible_dates() {
        let day = |d: &str| climate_day(&format!("...THE HOBBY CLIMATE SUMMARY FOR {d}..."));
        assert_eq!(day("FEBRUARY 31 2026"), None);
        assert_eq!(day("FEBRUARY 29 2026"), None);
        assert_eq!(day("APRIL 31 2026"), None);
        assert_eq!(
            day("FEBRUARY 29 2028"),
            Some(ClimateDay {
                year: 2028,
                month: 2,
                day: 29
            })
        );
        assert_eq!(
            day("OCTOBER 26 2026"),
            Some(ClimateDay {
                year: 2026,
                month: 10,
                day: 26
            })
        );
    }

    #[test]
    fn lst_times() {
        assert_eq!(lst_time("12:55", "PM"), Some((12, 55)));
        assert_eq!(lst_time("2:59", "PM"), Some((14, 59)));
        assert_eq!(lst_time("12:06", "AM"), Some((0, 6)));
        assert_eq!(lst_time("90", "2010"), None);
        assert_eq!(lst_time("13:00", "PM"), None);
        assert_eq!(lst_time("1221", "AM"), Some((0, 21)));
        assert_eq!(lst_time("0255", "PM"), Some((14, 55)));
        assert_eq!(lst_time("1300", "PM"), None);
        assert_eq!(lst_time("1260", "AM"), None);
        assert_eq!(lst_time("221", "AM"), Some((2, 21)));
        assert_eq!(lst_time("259", "PM"), Some((14, 59)));
        assert_eq!(lst_time("90", "PM"), None);
        assert_eq!(lst_time("12210", "AM"), None);
    }

    #[test]
    fn kinds() {
        assert_eq!(
            kind("VALID TODAY AS OF 0400 PM LOCAL TIME."),
            CliKind::Prelim
        );
        assert_eq!(
            kind("VALID TODAY AS OF 0700 AM LOCAL TIME."),
            CliKind::Morning
        );
        assert_eq!(kind("TEMPERATURE (F)\n YESTERDAY"), CliKind::Final);
    }
}
