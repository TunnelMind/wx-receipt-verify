//! METAR/SPECI report lines: the body temperature and the remarks groups that
//! carry tenths of a degree.
//!
//! Format source, unless a line says otherwise: NWS "Key to decode an ASOS
//! (METAR) observation", https://www.weather.gov/media/wrh/mesowest/metar_decode_key.pdf
//! (read 2026-10-27), cited below as [KEY]. It summarises FMH-1, which was not
//! read for this crate.
//!
//! Why the tenths matter: ASOS keeps the running max of 5-minute means, and
//! the whole-°C body temperature is rounded before any °F conversion
//! (https://www.weather.gov/lox/asostemperature, read 2026-10-27), so the
//! body value alone loses up to a degree Fahrenheit.

/// METAR is the hourly scheduled report, SPECI the unscheduled special [KEY].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportType {
    Metar,
    Speci,
}

/// One parsed report. Temperatures are i32: whole °C in the body, tenths of
/// a °C in the remarks. `None` means the group was absent (or not in its
/// exact shape, e.g. `1////`): [KEY] says a missing element's group is omitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metar {
    pub report_type: ReportType,
    /// ICAO identifier, four characters [KEY].
    pub station: String,
    /// Issue time DDHHMMZ, UTC [KEY]; the month and year are not in the report.
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    /// `AUTO`: fully automated, no human intervention [KEY].
    pub auto: bool,
    /// `COR`: correction to a previously disseminated observation [KEY,
    /// abbreviations]. Its position is UNVERIFIED here, so it counts either
    /// directly after the report type (`METAR COR KHOU ...`) or as any body
    /// token (`METAR KHOU 272353Z COR ...`).
    pub cor: bool,
    /// `$`: maintenance check indicator, the system needs maintenance [KEY].
    pub maintenance: bool,
    /// Body `TT/DD`: whole °C, `M` prefix for minus [KEY].
    pub temp_c: Option<i32>,
    pub dew_c: Option<i32>,
    /// The line had an `RMK` section; without one every field below is `None`.
    pub has_rmk: bool,
    /// `TsTTTsDDD`: hourly temperature and dew point, tenths °C, s = 1 below
    /// 0 °C [KEY].
    pub temp_tenths_c: Option<i32>,
    pub dew_tenths_c: Option<i32>,
    /// `1sTTT` / `2sTTT`: 6-hour max / min, tenths °C, sent at 00, 06, 12
    /// and 18 UTC [KEY].
    pub max_6h_tenths_c: Option<i32>,
    pub min_6h_tenths_c: Option<i32>,
    /// `4sTTTsTTT`: 24-hour max then min, tenths °C, sent at midnight local
    /// standard time [KEY].
    pub max_24h_tenths_c: Option<i32>,
    pub min_24h_tenths_c: Option<i32>,
    /// Sensor-status remarks (`PNO`, `TSNO`, ...) in report order, each once:
    /// the station says that sensor is out [KEY, sensor status indicators].
    /// Only the groups in `SENSOR_OUT` are kept.
    pub sensor_out: Vec<&'static str>,
}

/// The sensor-status remarks `Metar::sensor_out` keeps: [KEY]'s indicators
/// that actually occur in the archive. Measured 2026-11-02 over a copy of
/// `~/.tm-weather/archive` (days 2026-10-28..2026-11-03, `metar/` bodies,
/// 7737 distinct report lines, 7451 with `RMK`), remark tokens ending `NO`,
/// distinct reports: PNO 322 (rain gauge), TSNO 92 (lightning detector),
/// RVRNO 13 (runway visual range), PWINO 8 (present-weather identifier),
/// FZRANO 8 (freezing-rain sensor), VISNO 2 (secondary visibility; written
/// `VISNO RWY06`, the location token is not kept). SLPNO (35) also occurs but
/// is left out: it says sea-level pressure was not computed, not that a
/// sensor is out. CHINO is in [KEY] but in no archived report, so not listed.
pub const SENSOR_OUT: [&str; 6] = ["PNO", "TSNO", "RVRNO", "PWINO", "FZRANO", "VISNO"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    Empty,
    /// The first token is not `METAR` or `SPECI`.
    NotAReport(String),
    BadStation(String),
    BadTime(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Empty => write!(f, "empty report line"),
            ParseError::NotAReport(t) => write!(f, "not METAR or SPECI: {t:?}"),
            ParseError::BadStation(t) => write!(f, "bad station: {t:?}"),
            ParseError::BadTime(t) => write!(f, "bad DDHHMMZ time: {t:?}"),
        }
    }
}

impl std::error::Error for ParseError {}

/// The report lines of a stored body, e.g. the aviationweather.gov raw
/// response: one report per line, blanks skipped, surrounding space trimmed.
pub fn report_lines(body: &str) -> impl Iterator<Item = &str> {
    body.lines().map(str::trim).filter(|l| !l.is_empty())
}

/// Parse one report line. Remarks groups are matched only after `RMK` and
/// only by exact shape, so `T02330194`, `56002` or `60000` are never read as
/// a max/min group.
pub fn parse(line: &str) -> Result<Metar, ParseError> {
    // A trailing `=` ends a report in some feeds (UNVERIFIED for this source,
    // which has none); it is not part of the last group.
    let line = line.trim().trim_end_matches('=');
    let mut tokens = line.split_whitespace();
    let report_type = match tokens.next() {
        None => return Err(ParseError::Empty),
        Some("METAR") => ReportType::Metar,
        Some("SPECI") => ReportType::Speci,
        Some(t) => return Err(ParseError::NotAReport(t.to_owned())),
    };
    let mut station = tokens.next().ok_or(ParseError::BadStation(String::new()))?;
    // `COR` may also come before the station (`METAR COR KHOU ...`, form
    // UNVERIFIED against [KEY]). It is three characters, so it can never be
    // mistaken for a four-character station identifier.
    let cor_first = station == "COR";
    if cor_first {
        station = tokens
            .next()
            .ok_or(ParseError::BadStation(station.to_owned()))?;
    }
    if station.len() != 4 || !station.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(ParseError::BadStation(station.to_owned()));
    }
    let time = tokens.next().ok_or(ParseError::BadTime(String::new()))?;
    let (day, hour, minute) = parse_time(time).ok_or(ParseError::BadTime(time.to_owned()))?;

    let mut m = Metar {
        report_type,
        station: station.to_owned(),
        day,
        hour,
        minute,
        auto: false,
        cor: cor_first,
        maintenance: false,
        temp_c: None,
        dew_c: None,
        has_rmk: false,
        temp_tenths_c: None,
        dew_tenths_c: None,
        max_6h_tenths_c: None,
        min_6h_tenths_c: None,
        max_24h_tenths_c: None,
        min_24h_tenths_c: None,
        sensor_out: Vec::new(),
    };
    for t in tokens {
        // `$` closes the report, after the remarks in [KEY]'s example.
        if t == "$" {
            m.maintenance = true;
        } else if t == "RMK" {
            m.has_rmk = true;
        } else if !m.has_rmk {
            match t {
                "AUTO" => m.auto = true,
                "COR" => m.cor = true,
                _ => {
                    if let (None, Some((tc, dc))) = (m.temp_c, body_temp(t)) {
                        m.temp_c = Some(tc);
                        m.dew_c = dc;
                    }
                }
            }
        } else if let Some(&s) = SENSOR_OUT.iter().find(|s| **s == t) {
            if !m.sensor_out.contains(&s) {
                m.sensor_out.push(s);
            }
        } else {
            // First match wins if a group repeats.
            let b = t.as_bytes();
            match (b.len(), b[0]) {
                (9, b'T') if m.temp_tenths_c.is_none() => {
                    if let (Some(tt), Some(dd)) = (tenths(&b[1..5]), tenths(&b[5..9])) {
                        m.temp_tenths_c = Some(tt);
                        m.dew_tenths_c = Some(dd);
                    }
                }
                // Temperature without dew point: the form is UNVERIFIED (not
                // in [KEY]); accepted so a real tenths value is not dropped.
                (5, b'T') if m.temp_tenths_c.is_none() => m.temp_tenths_c = tenths(&b[1..5]),
                (5, b'1') if m.max_6h_tenths_c.is_none() => m.max_6h_tenths_c = tenths(&b[1..5]),
                (5, b'2') if m.min_6h_tenths_c.is_none() => m.min_6h_tenths_c = tenths(&b[1..5]),
                (9, b'4') if m.max_24h_tenths_c.is_none() => {
                    if let (Some(x), Some(n)) = (tenths(&b[1..5]), tenths(&b[5..9])) {
                        m.max_24h_tenths_c = Some(x);
                        m.min_24h_tenths_c = Some(n);
                    }
                }
                _ => {}
            }
        }
    }
    Ok(m)
}

/// `DDHHMMZ` [KEY], range-checked.
fn parse_time(t: &str) -> Option<(u8, u8, u8)> {
    let digits = t.strip_suffix('Z')?;
    if digits.len() != 6 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n = |i: usize| digits[i..i + 2].parse::<u8>().ok();
    let (d, h, mi) = (n(0)?, n(2)?, n(4)?);
    ((1..=31).contains(&d) && h <= 23 && mi <= 59).then_some((d, h, mi))
}

/// `sTTT`: sign digit (0 = at or above 0 °C, 1 = below [KEY]) then three
/// digits of tenths °C. Anything else (e.g. `////`) is `None`.
fn tenths(b: &[u8]) -> Option<i32> {
    let sign = match b.first()? {
        b'0' => 1,
        b'1' => -1,
        _ => return None,
    };
    let digits = b.get(1..4)?;
    if b.len() != 4 || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let v = digits
        .iter()
        .fold(0, |acc, d| acc * 10 + i32::from(d - b'0'));
    Some(sign * v)
}

/// Body `TT/DD` or `TT/` (dew point missing), whole °C, `M` = minus [KEY].
/// Exactly two digits each side, so `1/2SM` or `R11/P6000FT` never match.
fn body_temp(t: &str) -> Option<(i32, Option<i32>)> {
    let (l, r) = t.split_once('/')?;
    let whole = |s: &str| {
        let (neg, d) = s.strip_prefix('M').map_or((false, s), |d| (true, d));
        if d.len() != 2 || !d.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let v: i32 = d.parse().ok()?;
        Some(if neg { -v } else { v })
    };
    let temp = whole(l)?;
    let dew = if r.is_empty() { None } else { Some(whole(r)?) };
    Some((temp, dew))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenths_sign_and_shape() {
        assert_eq!(tenths(b"0233"), Some(233));
        assert_eq!(tenths(b"1105"), Some(-105));
        assert_eq!(tenths(b"0000"), Some(0));
        assert_eq!(tenths(b"2105"), None);
        assert_eq!(tenths(b"0///"), None);
    }

    #[test]
    fn body_temp_shapes() {
        assert_eq!(body_temp("23/19"), Some((23, Some(19))));
        assert_eq!(body_temp("M05/M12"), Some((-5, Some(-12))));
        assert_eq!(body_temp("02/"), Some((2, None)));
        assert_eq!(body_temp("1/2SM"), None);
        assert_eq!(body_temp("R11/P6000FT"), None);
    }

    #[test]
    fn sensor_out_remarks_once_in_order() {
        let m = parse("METAR KGKY 290353Z AUTO A3012 RMK AO2 SLPNO PWINO FZRANO PWINO $").unwrap();
        assert_eq!(m.sensor_out, ["PWINO", "FZRANO"]);
        // Only after RMK.
        assert!(parse("METAR KGKY 290353Z PNO A3012")
            .unwrap()
            .sensor_out
            .is_empty());
    }

    #[test]
    fn time_ranges() {
        assert_eq!(parse_time("272353Z"), Some((27, 23, 53)));
        assert_eq!(parse_time("322353Z"), None);
        assert_eq!(parse_time("272460Z"), None);
        assert_eq!(parse_time("272353"), None);
    }
}
