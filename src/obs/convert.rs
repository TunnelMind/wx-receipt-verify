//! Tenths of a °C (the METAR remarks groups) back to the whole °F that ASOS
//! measured.
//!
//! ASOS works in whole °F and reports °C by conversion: the remarks carry
//! tenths, the body a whole °C that is "1 °F low about half the time"
//! (https://www.weather.gov/lox/asostemperature, read 2026-10-27). So the
//! settlement number comes from the tenths, never from `Metar::temp_c`.

/// Tenths of a °C to whole °F, rounded to nearest with ties toward +∞
/// (half-up): `floor((tenths·9 + 1600 + 25) / 50)`, since °F = tenths·9/50 + 32.
///
/// Integer arithmetic only, and `div_euclid` floors where plain `/` would
/// truncate toward zero and round sub-zero values the wrong way. The tie rule
/// never decides a round trip: ASOS rounds a whole °F to the nearest tenth of
/// a °C, an error of at most 0.05 °C = 0.09 °F, so the exact °F value lands
/// within 0.09 of a whole number, never on a .5 tie.
pub fn tenths_c_to_whole_f(tenths: i32) -> i32 {
    (tenths * 9 + 1600 + 25).div_euclid(50)
}

/// Tenths of a °C to tenths of a °F, rounded to nearest with ties toward +∞
/// (half-up): `floor((tenths·9 + 2) / 5) + 320`, since °F·10 = tenths·9/5 + 320.
///
/// This is the contract for a receipt's `running_max_tenths_f`: the writer
/// must use this rounding, since the verifier recomputes it with this function
/// and compares exactly. `div_euclid` floors for sub-zero values, as above.
pub fn tenths_c_to_tenths_f(tenths: i32) -> i32 {
    (tenths * 9 + 2).div_euclid(5) + 320
}

#[cfg(test)]
mod tests {
    use super::{tenths_c_to_tenths_f, tenths_c_to_whole_f};
    use crate::obs::metar;

    /// The ASOS direction: whole °F to tenths °C, nearest. (f−32)·50/9 is
    /// never exactly .5 (an integer over 9), so the tie rule is moot here.
    fn f_to_tenths_c(f: i32) -> i32 {
        (2 * (f - 32) * 50 + 9).div_euclid(18)
    }

    #[test]
    fn round_trip_every_whole_f() {
        for f in -40..=130 {
            assert_eq!(tenths_c_to_whole_f(f_to_tenths_c(f)), f, "{f} °F");
        }
        // The range crosses 0 °C, so negative tenths are covered.
        assert_eq!(f_to_tenths_c(-40), -400);
        assert_eq!(f_to_tenths_c(31), -6);
    }

    #[test]
    fn negatives_floor_not_truncate() {
        // -0.6 °C = 30.92 °F; -1.9 °C = 28.58 °F; -40.0 °C = -40 °F.
        assert_eq!(tenths_c_to_whole_f(-6), 31);
        assert_eq!(tenths_c_to_whole_f(-19), 29);
        assert_eq!(tenths_c_to_whole_f(-400), -40);
        // Ties go up: 2.5 °C = 36.5 °F, -27.5 °C = -17.5 °F.
        assert_eq!(tenths_c_to_whole_f(25), 37);
        assert_eq!(tenths_c_to_whole_f(-275), -17);
    }

    #[test]
    fn body_whole_c_is_one_f_low() {
        // ASOS measured 85 °F = 29.44 °C: T-group 0294, body rounded to 29.
        let m =
            metar::parse("METAR KHOU 271753Z 18010KT 10SM FEW040 29/22 A2998 RMK AO2 T02940222")
                .unwrap();
        assert_eq!((m.temp_c, m.temp_tenths_c), (Some(29), Some(294)));
        assert_eq!(tenths_c_to_whole_f(m.temp_tenths_c.unwrap()), 85);
        // 29 °C = 84.2 °F: the body loses the degree.
        assert_eq!(tenths_c_to_whole_f(m.temp_c.unwrap() * 10), 84);
    }

    #[test]
    fn tenths_c_to_tenths_f_rounds_half_up() {
        assert_eq!(tenths_c_to_tenths_f(124), 543); // 12.4 °C = 54.32 °F
        assert_eq!(tenths_c_to_tenths_f(167), 621); // 16.7 °C = 62.06 °F
        assert_eq!(tenths_c_to_tenths_f(0), 320);
        assert_eq!(tenths_c_to_tenths_f(-400), -400);
        // -1.9 °C = 28.58 °F; plain `/` would truncate toward zero to 286.
        assert_eq!(tenths_c_to_tenths_f(-19), 286);
        // 0.5 °C = 32.9 °F exactly; -0.5 °C = 31.1 °F exactly.
        assert_eq!(tenths_c_to_tenths_f(5), 329);
        assert_eq!(tenths_c_to_tenths_f(-5), 311);
    }
}
