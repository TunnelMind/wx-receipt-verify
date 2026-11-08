//! The running maximum for a (station, climate day, as-of instant), from the
//! stored METAR/SPECI bodies, in whole °F.
//!
//! Times are plain `i64` Unix seconds, UTC. The caller supplies every
//! instant (each body's fetch time, the as-of); nothing here reads a clock.
//!
//! Why first-seen gates `as_of`, not the report's issue time: a backtest may
//! only use what was knowable at the decision (spec 027, "As-of honesty").
//! A report issued at 21:53Z that we first stored at 22:10Z was not known at
//! 22:00Z, so it must not move the 22:00Z running max.

use std::collections::BTreeMap;

use crate::obs::cli::ClimateDay;
use crate::obs::convert::tenths_c_to_whole_f;
use crate::obs::metar;

/// Each stored report line (trimmed text) mapped to its first-seen time: the
/// earliest `fetched_utc` of any body that contains that exact line. A min
/// over the whole set, so the order of `bodies` never matters and a body
/// added later (a SPECI, a re-fetch) cannot move an earlier report's value.
pub fn first_seen<'a>(bodies: &[(i64, &'a str)]) -> BTreeMap<&'a str, i64> {
    let mut seen = BTreeMap::new();
    for &(fetched, body) in bodies {
        for line in metar::report_lines(body) {
            let t = seen.entry(line).or_insert(fetched);
            *t = (*t).min(fetched);
        }
    }
    seen
}

/// The max whole °F (and the max tenths °C it came from) for `station` over
/// the climate day `climate_day`
/// (a [`ClimateDay`] in local standard time), using only reports first seen at
/// or before `as_of`. Candidates are each report's T-group temperature, its
/// 6-hour max group (`1sTTT`) and its 24-hour max group (`4sTTTsTTT`), all
/// converted by `tenths_c_to_whole_f`; the whole-°C body value is never used
/// (it reads up to 1 °F low, see convert). `None` when no report in the day
/// carries a tenths value.
///
/// `lst_offset_hours` is the station's standard-time UTC offset, e.g. -6 for
/// CST, and is applied all year. The NWS observation period for a calendar
/// day "spans from midnight to midnight local standard time", which "does not
/// change during daylight saving time, so the daily observation period during
/// daylight saving time spans from 1 AM to 1 AM local daylight time"
/// (https://www.weather.gov/lot/weather_observations_faq, read 2026-10-27).
/// For CST that is 06:00Z to 06:00Z in July as in January.
///
/// A max group covers the hours before its report, so it counts only when
/// its whole window lies inside that observation period:
/// - T-group: the report's issue time is in the day.
/// - 6-hour group: `[issued - 6 h, issued]` is in the day. For EST the
///   05:53Z group spans the 05:00Z boundary, so it counts toward neither day.
/// - 24-hour group: counts only from the report in the hour before the day's
///   end (ASOS sends the LST-midnight group on the :53 report before the
///   hour), and only for the day it closes. A report after the end (the next
///   day's 00:53 LST) does not credit it.
///
/// A `COR` report replaces the report it corrects (same station, same
/// resolved issue instant) once the COR is first seen at or before `as_of`;
/// before that the original counts. A COR with no original counts as usual.
/// When several CORs for one instant are past the gate, the newest wins: the
/// one with the greatest first-seen time (a tie goes to the greater line text,
/// so the result never depends on input order). Only that COR counts.
///
/// A line first seen at or before `as_of` that does not parse (station
/// unreadable included), or a `station` line whose DDHHMMZ is no real date,
/// is not used but is counted in `unparsed`, so a caller can tell a quiet
/// day from a feed it could not read. Lines for other stations are skipped
/// uncounted.
///
/// ponytail: a straddling 6-hour group is dropped, not used as a lower bound
/// for the day holding its later part. Add that bound if a backtest needs it.
pub fn running_max(
    station: &str,
    climate_day: ClimateDay,
    as_of: i64,
    bodies: &[(i64, &str)],
    lst_offset_hours: i32,
) -> RunningMax {
    let ClimateDay { year, month, day } = climate_day;
    let start = days_from_civil(i64::from(year), u32::from(month), u32::from(day)) * 86_400
        - i64::from(lst_offset_hours) * 3600;
    let end = start + 86_400;
    let mut unparsed = 0;
    let reports: Vec<(metar::Metar, i64, (i64, &str))> = first_seen(bodies)
        .into_iter()
        .filter(|&(_, seen)| seen <= as_of)
        .filter_map(|(line, seen)| {
            let Ok(r) = metar::parse(line) else {
                unparsed += 1;
                return None;
            };
            if r.station != station {
                return None;
            }
            // Resolve against the first-seen fetch: the report predates every
            // body that holds it, and the earliest is the tightest bound.
            let Some(issued) = resolve_issue(seen, r.day, r.hour, r.minute) else {
                unparsed += 1;
                return None;
            };
            Some((r, issued, (seen, line)))
        })
        .collect();
    // The newest COR per issue instant, keyed (first-seen, line). Only CORs
    // already past the as-of gate are here, so a later COR cannot suppress
    // its original, or an earlier COR, early.
    let mut newest_cor: BTreeMap<i64, (i64, &str)> = BTreeMap::new();
    for &(ref r, issued, key) in &reports {
        if r.cor {
            let k = newest_cor.entry(issued).or_insert(key);
            *k = (*k).max(key);
        }
    }
    let max_tenths_c = reports
        .into_iter()
        .filter(|(r, issued, key)| match newest_cor.get(issued) {
            Some(newest) => r.cor && newest == key,
            None => true,
        })
        .flat_map(|(r, issued, _)| {
            let in_day = (start..end).contains(&issued);
            [
                r.temp_tenths_c.filter(|_| in_day),
                r.max_6h_tenths_c
                    .filter(|_| in_day && issued - 6 * 3600 >= start),
                r.max_24h_tenths_c
                    .filter(|_| (end - 3600..end).contains(&issued)),
            ]
        })
        .flatten()
        .max();
    // tenths_c_to_whole_f is monotone, so converting the max tenths gives the
    // same whole °F as taking the max of the converted values.
    let max_f = max_tenths_c.map(tenths_c_to_whole_f);
    RunningMax {
        max_tenths_c,
        max_f,
        unparsed,
    }
}

/// What `running_max` returns: the max tenths °C as reported, the max whole
/// °F converted from it (both `None` when no report in the day carries a
/// tenths value), and how many in-gate lines it dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunningMax {
    pub max_tenths_c: Option<i32>,
    pub max_f: Option<i32>,
    pub unparsed: usize,
}

/// Up to 10 minutes of skew is tolerated: our fetch stamp can trail the
/// report's issue minute, which would otherwise resolve a month early.
///
/// wx-health (`wx-health/src/archive.rs`, its `observed_utc`) uses this too,
/// so a report line resolves to the same instant in the running max and in
/// the health gate. Keep it ONE definition: no copies.
pub const SKEW_SECS: i64 = 600;

/// A report carries only DDHHMMZ, no month or year: it is the latest instant
/// with that day/hour/minute at or before the fetch plus the skew allowance
/// ([`SKEW_SECS`]), stepping back a month (or two, past a short February)
/// when the day is later than that or does not exist in that month.
///
/// wx-health (`wx-health/src/archive.rs`, its `observed_utc`) uses this too,
/// so a report line resolves to the same instant in the running max and in
/// the health gate. Keep it ONE definition: no copies.
pub fn resolve_issue(fetched: i64, day: u8, hour: u8, minute: u8) -> Option<i64> {
    let latest = fetched + SKEW_SECS;
    // The month comes from `latest` too, so a fetch at 07-31 23:55 still
    // places a report issued 01 00:02 on August 1st.
    let (fy, fm, _) = civil_from_days(latest.div_euclid(86_400));
    (0..3).find_map(|back| {
        let months = fy * 12 + i64::from(fm) - 1 - back;
        let (y, m) = (months.div_euclid(12), months.rem_euclid(12) as u32 + 1);
        let days = days_from_civil(y, m, u32::from(day));
        // A day like 31 April normalises into May: not a real date.
        if civil_from_days(days) != (y, m, u32::from(day)) {
            return None;
        }
        let t = days * 86_400 + i64::from(hour) * 3600 + i64::from(minute) * 60;
        (t <= latest).then_some(t)
    })
}

/// The last minute of `climate_day` on the station's local wall clock.
///
/// The climate day runs midnight to midnight local standard time all year
/// (NWS FAQ cited on [`running_max`]), so under daylight saving time its last
/// minute is 00:59 CDT/MDT on the next date, after the market's 11:59 PM
/// clock-time close; under standard time it is 23:59 on the same date. The
/// gate uses this to see that the climate day's last hour lies past close.
///
/// `lst_offset_hours` is the station's standard-time UTC offset (-6 for CST),
/// as in [`running_max`]. `local_offset_hours` is the wall-clock UTC offset in
/// force at the day's end (-5 under CDT, -6 under CST). The caller decides
/// whether DST is in force; this crate has no time-zone data.
pub fn climate_day_end_local_clock(
    climate_day: ClimateDay,
    lst_offset_hours: i32,
    local_offset_hours: i32,
) -> LocalClock {
    let ClimateDay { year, month, day } = climate_day;
    let midnight = days_from_civil(i64::from(year), u32::from(month), u32::from(day)) * 86_400;
    let end = midnight - i64::from(lst_offset_hours) * 3600 + 86_400;
    // Last minute as UTC, shifted to the wall clock, measured from the date's
    // wall-clock midnight.
    let since = end - 60 + i64::from(local_offset_hours) * 3600 - midnight;
    let (days, secs) = (since.div_euclid(86_400), since.rem_euclid(86_400));
    LocalClock {
        next_day: days > 0,
        hour: (secs / 3600) as u8,
        minute: (secs % 3600 / 60) as u8,
    }
}

/// A wall-clock minute relative to a climate day's calendar date; `next_day`
/// is set when it falls on the following date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalClock {
    pub next_day: bool,
    pub hour: u8,
    pub minute: u8,
}

/// Days since 1970-01-01 for a proleptic Gregorian date. Howard Hinnant's
/// `days_from_civil`, https://howardhinnant.github.io/date_algorithms.html.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let (m, d) = (i64::from(m), i64::from(d));
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The inverse, `civil_from_days`, same source.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (
        if m <= 2 {
            yoe + era * 400 + 1
        } else {
            yoe + era * 400
        },
        m,
        d,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(y: i64, m: u32, d: u32, h: i64, mi: i64) -> i64 {
        days_from_civil(y, m, d) * 86_400 + h * 3600 + mi * 60
    }

    const CST: i32 = -6;
    const fn jul(day: u8) -> ClimateDay {
        ClimateDay {
            year: 2026,
            month: 7,
            day,
        }
    }
    const JUL15: ClimateDay = jul(15);

    #[test]
    fn climate_day_ends_at_0059_next_day_under_dst() {
        let at = |next_day, hour, minute| LocalClock {
            next_day,
            hour,
            minute,
        };
        // CDT: LST midnight is 01:00 CDT, so the last minute is 00:59 next day.
        assert_eq!(climate_day_end_local_clock(JUL15, CST, -5), at(true, 0, 59));
        // CST: wall clock and LST agree.
        let jan = ClimateDay {
            year: 2026,
            month: 1,
            day: 15,
        };
        assert_eq!(
            climate_day_end_local_clock(jan, CST, CST),
            at(false, 23, 59)
        );
        // MDT: same shape one zone west.
        assert_eq!(climate_day_end_local_clock(JUL15, -7, -6), at(true, 0, 59));
    }

    #[test]
    fn civil_round_trip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn issue_time_takes_month_from_fetch() {
        // Fetched 2026-08-01 00:10Z, report 312353Z: the 31st is July's.
        assert_eq!(
            resolve_issue(utc(2026, 8, 1, 0, 10), 31, 23, 53),
            Some(utc(2026, 7, 31, 23, 53))
        );
        // Fetched 2026-03-01, day 30: no 30 February, so January.
        assert_eq!(
            resolve_issue(utc(2026, 3, 1, 0, 10), 30, 12, 0),
            Some(utc(2026, 1, 30, 12, 0))
        );
        assert_eq!(
            resolve_issue(utc(2026, 7, 15, 21, 58), 15, 21, 53),
            Some(utc(2026, 7, 15, 21, 53))
        );
    }

    #[test]
    fn issue_time_tolerates_ten_minutes_of_skew() {
        let jul = Some(utc(2026, 7, 15, 21, 53));
        assert_eq!(resolve_issue(utc(2026, 7, 15, 21, 50), 15, 21, 53), jul);
        assert_eq!(resolve_issue(utc(2026, 7, 15, 21, 43), 15, 21, 53), jul);
        // 11 minutes is past the allowance: back to June.
        assert_eq!(
            resolve_issue(utc(2026, 7, 15, 21, 42), 15, 21, 53),
            Some(utc(2026, 6, 15, 21, 53))
        );
        // Skew across a month boundary takes the month from fetch + skew.
        assert_eq!(
            resolve_issue(utc(2026, 7, 31, 23, 55), 1, 0, 2),
            Some(utc(2026, 8, 1, 0, 2))
        );
    }

    const M1753: &str =
        "METAR KHOU 151753Z 18010KT 10SM FEW040 32/22 A2998 RMK AO2 T03170222 10322";
    const M1853: &str = "METAR KHOU 151853Z 18010KT 10SM FEW040 31/22 A2998 RMK AO2 T03060222";
    const S1812: &str = "SPECI KHOU 151812Z 18010KT 3SM TSRA BKN030 28/22 A2998 RMK AO2 T02780222";

    #[test]
    fn speci_mid_list_keeps_earlier_first_seen() {
        let (t1, ts, t2) = (
            utc(2026, 7, 15, 17, 58),
            utc(2026, 7, 15, 18, 15),
            utc(2026, 7, 15, 18, 58),
        );
        let b1 = M1753.to_owned();
        let bs = format!("{S1812}\n{M1753}");
        let b2 = format!("{M1853}\n{S1812}\n{M1753}");
        let without = first_seen(&[(t1, &b1), (t2, &b2)]);
        let with = first_seen(&[(t1, &b1), (ts, &bs), (t2, &b2)]);
        let reordered = first_seen(&[(t2, &b2), (ts, &bs), (t1, &b1)]);
        assert_eq!(without[M1753], t1);
        assert_eq!(without[M1853], t2);
        // b2 already held the SPECI, so without bs it is first seen at t2.
        assert_eq!(without[S1812], t2);
        for fs in [&with, &reordered] {
            assert_eq!(fs[M1753], t1);
            assert_eq!(fs[M1853], t2);
            assert_eq!(fs[S1812], ts);
        }
    }

    #[test]
    fn running_max_at_2130z_counts_t_groups_and_18z_six_hour_group() {
        // One cumulative body per hour, fetched 5 min after issue, newest
        // first like the aviationweather.gov response.
        let lines = [
            // 29.4 °C = 84.92 °F -> 85
            (
                15,
                "METAR KHOU 151553Z 18008KT 10SM FEW040 29/22 A3000 RMK AO2 T02940222",
            ),
            // 31.1 °C = 87.98 °F -> 88
            (
                16,
                "METAR KHOU 151653Z 18008KT 10SM FEW040 31/22 A3000 RMK AO2 T03110222",
            ),
            // T 31.7 °C = 89.06 °F -> 89; 6-hour max 32.2 °C = 89.96 °F -> 90
            (17, M1753),
            // 30.6 °C = 87.08 °F -> 87
            (18, M1853),
            (
                19,
                "METAR KHOU 151953Z 18008KT 10SM FEW040 31/22 A3000 RMK AO2 T03110222",
            ),
            (
                20,
                "METAR KHOU 152053Z 18008KT 10SM FEW040 31/22 A3000 RMK AO2 T03110222",
            ),
            // 33.9 °C = 93.02 °F -> 93, but first seen 21:58Z, after as_of.
            (
                21,
                "METAR KHOU 152153Z 18008KT 10SM FEW040 34/22 A3000 RMK AO2 T03390222",
            ),
        ];
        let bodies: Vec<(i64, String)> = (0..lines.len())
            .map(|i| {
                let body: Vec<&str> = lines[..=i].iter().rev().map(|l| l.1).collect();
                (utc(2026, 7, 15, lines[i].0, 58), body.join("\n"))
            })
            .collect();
        let bodies: Vec<(i64, &str)> = bodies.iter().map(|(t, b)| (*t, b.as_str())).collect();
        let at = |h, mi| running_max("KHOU", JUL15, utc(2026, 7, 15, h, mi), &bodies, CST).max_f;
        // Max of 85, 88, 89, 90 (the 6-hour group), 87, 88, 88.
        assert_eq!(at(21, 30), Some(90));
        assert_eq!(at(22, 0), Some(93));
        // Before the 17:58Z fetch only 85 and 88 were known.
        assert_eq!(at(17, 57), Some(88));
        assert_eq!(
            running_max("KIAH", JUL15, utc(2026, 7, 15, 21, 30), &bodies, CST).max_f,
            None
        );
    }

    #[test]
    fn as_of_gates_on_first_seen_not_issue_time() {
        // Issued 21:53Z, before as_of 22:00Z, but first stored at 22:10Z.
        let line = "METAR KHOU 152153Z 18008KT 10SM FEW040 34/22 A3000 RMK AO2 T03390222";
        let bodies = [(utc(2026, 7, 15, 22, 10), line)];
        assert_eq!(
            running_max("KHOU", JUL15, utc(2026, 7, 15, 22, 0), &bodies, CST).max_f,
            None
        );
        assert_eq!(
            running_max("KHOU", JUL15, utc(2026, 7, 15, 22, 10), &bodies, CST).max_f,
            Some(93)
        );
    }

    #[test]
    fn lst_day_boundary_is_0600z_on_a_cdt_date() {
        // 2026-07-15 is in CDT, but the climate day is CST: 06:00Z = 01:00 CDT.
        // 30.0 °C = 86.0 °F at 05:53Z belongs to the 14th; 25.0 °C = 77.0 °F
        // at 06:00Z opens the 15th.
        let body = "SPECI KHOU 150600Z 18005KT 10SM CLR 25/22 A3000 RMK AO2 T02500222\n\
                    METAR KHOU 150553Z 18005KT 10SM CLR 30/22 A3000 RMK AO2 T03000222";
        let bodies = [(utc(2026, 7, 15, 6, 5), body)];
        let as_of = utc(2026, 7, 15, 6, 5);
        assert_eq!(
            running_max("KHOU", jul(14), as_of, &bodies, CST).max_f,
            Some(86)
        );
        assert_eq!(
            running_max("KHOU", JUL15, as_of, &bodies, CST).max_f,
            Some(77)
        );
    }

    #[test]
    fn tenths_beat_the_lower_whole_c_body() {
        // T-group 29.4 °C = 84.92 °F -> 85; body 29 °C = 84.2 °F -> 84.
        let line = "METAR KHOU 151753Z 18010KT 10SM FEW040 29/22 A2998 RMK AO2 T02940222";
        let bodies = [(utc(2026, 7, 15, 17, 58), line)];
        let got = running_max("KHOU", JUL15, utc(2026, 7, 15, 18, 0), &bodies, CST).max_f;
        assert_eq!(got, Some(85));
        assert_eq!(tenths_c_to_whole_f(29 * 10), 84);
        // A report with only the body temperature contributes nothing.
        let no_tenths = [(
            utc(2026, 7, 15, 17, 58),
            "METAR KHOU 151753Z 18010KT 29/22 A2998",
        )];
        assert_eq!(
            running_max("KHOU", JUL15, utc(2026, 7, 15, 18, 0), &no_tenths, CST).max_f,
            None
        );
    }

    const EST: i32 = -5;

    #[test]
    fn six_hour_group_straddling_day_start_does_not_count() {
        // EST day 15 starts 05:00Z. The 05:53Z group (32.2 °C -> 90) covers
        // 23:53Z-05:53Z, mostly the 14th: only the T-group 25.0 °C -> 77.
        let line = "METAR KJFK 150553Z 18005KT 10SM CLR 25/22 A3000 RMK AO2 T02500222 10322";
        let bodies = [(utc(2026, 7, 15, 5, 58), line)];
        let as_of = utc(2026, 7, 15, 6, 0);
        assert_eq!(
            running_max("KJFK", JUL15, as_of, &bodies, EST).max_f,
            Some(77)
        );
        // Issued in the 15th, so it gives the 14th nothing either.
        assert_eq!(
            running_max("KJFK", jul(14), as_of, &bodies, EST).max_f,
            None
        );
        // A group alone, with no T-group, gives the day nothing.
        let only = [(
            utc(2026, 7, 15, 5, 58),
            "METAR KJFK 150553Z 18005KT 10SM CLR 25/22 A3000 RMK AO2 10322",
        )];
        assert_eq!(running_max("KJFK", JUL15, as_of, &only, EST).max_f, None);
    }

    #[test]
    fn six_hour_group_inside_the_day_counts_eastern() {
        // 17:53Z window 11:53Z-17:53Z is inside 05:00Z-05:00Z: T 89, 6 h 90.
        let line = M1753.replace("KHOU", "KJFK");
        let bodies = [(utc(2026, 7, 15, 17, 58), line.as_str())];
        let got = running_max("KJFK", JUL15, utc(2026, 7, 15, 18, 0), &bodies, EST).max_f;
        assert_eq!(got, Some(90));
    }

    #[test]
    fn twenty_four_hour_group_counts_from_lst_midnight_for_the_day_it_closes() {
        // CST day 15 ends 06:00Z on the 16th. ASOS sends the midnight group on
        // the 05:53Z report: its 24-hour max 33.9 °C -> 93 credits the 15th,
        // and the 16th, which has not started, gets nothing.
        let l0553 = "METAR KHOU 160553Z 18005KT 10SM CLR 25/22 A3000 RMK AO2 T02500222 403390211";
        let b0553 = [(utc(2026, 7, 16, 5, 58), l0553)];
        let at0600 = utc(2026, 7, 16, 6, 0);
        assert_eq!(
            running_max("KHOU", JUL15, at0600, &b0553, CST).max_f,
            Some(93)
        );
        assert_eq!(
            running_max("KHOU", jul(16), at0600, &b0553, CST).max_f,
            None
        );
        // The same group on the 06:53Z report (00:53 LST on the 16th) is after
        // the 15th's end: no 24-hour credit to the 15th, and the 16th gets
        // only its T-group 25.0 °C -> 77.
        let l0653 = "METAR KHOU 160653Z 18005KT 10SM CLR 25/22 A3000 RMK AO2 T02500222 403390211";
        let b0653 = [(utc(2026, 7, 16, 6, 58), l0653)];
        let at0700 = utc(2026, 7, 16, 7, 0);
        assert_eq!(running_max("KHOU", JUL15, at0700, &b0653, CST).max_f, None);
        assert_eq!(
            running_max("KHOU", jul(16), at0700, &b0653, CST).max_f,
            Some(77)
        );
        // Not yet seen at 05:55Z: the as-of gate still applies.
        let early = utc(2026, 7, 16, 5, 55);
        assert_eq!(running_max("KHOU", JUL15, early, &b0553, CST).max_f, None);
        // The same group on a non-midnight report credits nothing.
        let noon = "METAR KHOU 151753Z 18010KT 10SM FEW040 25/22 A2998 RMK AO2 T02500222 403390211";
        let bodies = [(utc(2026, 7, 15, 17, 58), noon)];
        let as_of = utc(2026, 7, 15, 18, 0);
        assert_eq!(
            running_max("KHOU", JUL15, as_of, &bodies, CST).max_f,
            Some(77)
        );
        assert_eq!(
            running_max("KHOU", jul(14), as_of, &bodies, CST).max_f,
            None
        );
    }

    // T 35.0 °C = 95 °F; its COR T 32.8 °C = 91.04 °F -> 91.
    const ORIG: &str = "METAR KHOU 151753Z 18010KT 10SM FEW040 35/22 A2998 RMK AO2 T03500222";
    const COR: &str = "METAR KHOU 151753Z COR 18010KT 10SM FEW040 33/22 A2998 RMK AO2 T03280222";

    #[test]
    fn cor_replaces_original_from_its_first_seen() {
        let b2 = format!("{COR}\n{ORIG}");
        let bodies = [
            (utc(2026, 7, 15, 17, 58), ORIG),
            (utc(2026, 7, 15, 18, 20), b2.as_str()),
        ];
        let at = |h, mi| running_max("KHOU", JUL15, utc(2026, 7, 15, h, mi), &bodies, CST).max_f;
        assert_eq!(at(18, 10), Some(95));
        assert_eq!(at(18, 30), Some(91));
    }

    #[test]
    fn later_cor_replaces_earlier_cor() {
        // T 33.9 °C = 93.02 °F -> 93; then COR (32.8 °C) -> 91.
        let cor_a = "METAR KHOU 151753Z COR 18010KT 10SM FEW040 34/22 A2998 RMK AO2 T03390222";
        let bodies = [
            (utc(2026, 7, 15, 17, 58), ORIG),
            (utc(2026, 7, 15, 18, 20), cor_a),
            (utc(2026, 7, 15, 18, 40), COR),
        ];
        let at = |h, mi| running_max("KHOU", JUL15, utc(2026, 7, 15, h, mi), &bodies, CST).max_f;
        assert_eq!(at(18, 10), Some(95));
        assert_eq!(at(18, 30), Some(93));
        assert_eq!(at(18, 50), Some(91));
    }

    #[test]
    fn cor_for_other_station_or_minute_leaves_original() {
        let other_stn = COR.replace("KHOU", "KIAH");
        let other_min = COR.replace("151753Z", "151754Z");
        for cor in [other_stn, other_min] {
            let bodies = [
                (utc(2026, 7, 15, 17, 58), ORIG),
                (utc(2026, 7, 15, 18, 20), cor.as_str()),
            ];
            let got = running_max("KHOU", JUL15, utc(2026, 7, 15, 18, 30), &bodies, CST).max_f;
            assert_eq!(got, Some(95));
        }
    }

    #[test]
    fn unparsable_in_gate_line_is_counted_not_dropped() {
        let t = utc(2026, 7, 15, 17, 58);
        let garbage = format!("METAR KHOU 99xxxxZ\n{M1753}");
        let at = utc(2026, 7, 15, 18, 0);
        let clean = running_max("KHOU", JUL15, at, &[(t, M1753)], CST);
        let dirty = running_max("KHOU", JUL15, at, &[(t, &garbage)], CST);
        assert_eq!(
            clean,
            RunningMax {
                max_tenths_c: Some(322),
                max_f: Some(90),
                unparsed: 0
            }
        );
        assert_eq!(
            dirty,
            RunningMax {
                max_tenths_c: Some(322),
                max_f: Some(90),
                unparsed: 1
            }
        );
        // Not yet seen at as_of: not counted either.
        let early = running_max("KHOU", JUL15, t - 60, &[(t, &garbage)], CST);
        assert_eq!(
            early,
            RunningMax {
                max_tenths_c: None,
                max_f: None,
                unparsed: 0
            }
        );
        // Another station's line is skipped, not a failure; an unreadable
        // station is.
        let mixed = format!(
            "{}\nMETAR K 151753Z\n{M1753}",
            M1753.replace("KHOU", "KIAH")
        );
        let got = running_max("KHOU", JUL15, at, &[(t, &mixed)], CST);
        assert_eq!(
            got,
            RunningMax {
                max_tenths_c: Some(322),
                max_f: Some(90),
                unparsed: 1
            }
        );
    }
}
