//! HTTP dates (`Sun, 06 Nov 1994 08:49:37 GMT`, the IMF-fixdate format of RFC 9110).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// Days since 1970-01-01 to (year, month 1-12, day 1-31).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (if m > 2 { m - 3 } else { m + 9 }) as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Formats a time as an HTTP date. Times before 1970 are clamped to the epoch.
pub fn format(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        DAYS[(days.rem_euclid(7)) as usize],
        d,
        MONTHS[(m - 1) as usize],
        y,
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Parses an IMF-fixdate. The two obsolete formats are not accepted.
pub fn parse(s: &str) -> Option<SystemTime> {
    let b = s.as_bytes();
    if b.len() != 29
        || &b[3..5] != b", "
        || b[7] != b' '
        || b[11] != b' '
        || b[16] != b' '
        || b[19] != b':'
        || b[22] != b':'
        || &b[25..] != b" GMT"
    {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse().ok() };
    let day_name = s.get(0..3)?;
    let month = MONTHS.iter().position(|m| Some(*m) == s.get(8..11))? as u32 + 1;
    let (d, y, hh, mm, ss) = (num(5..7)?, num(12..16)?, num(17..19)?, num(20..22)?, num(23..25)?);
    if !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 || y < 1970 {
        return None;
    }
    let days = days_from_civil(y, month, d as u32);
    // Reject dates that do not exist (31 February) and a weekday that does not match.
    if civil_from_days(days) != (y, month, d as u32) || DAYS[days.rem_euclid(7) as usize] != day_name {
        return None;
    }
    let secs = days * 86_400 + hh * 3600 + mm * 60 + ss.min(59);
    Some(UNIX_EPOCH + Duration::from_secs(u64::try_from(secs).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn known_dates() {
        assert_eq!(format(at(0)), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(format(at(784_111_777)), "Sun, 06 Nov 1994 08:49:37 GMT"); // the example in RFC 9110
        assert_eq!(format(at(951_782_400)), "Tue, 29 Feb 2000 00:00:00 GMT");
        assert_eq!(format(at(4_102_444_799)), "Thu, 31 Dec 2099 23:59:59 GMT");
        assert_eq!(format(UNIX_EPOCH - Duration::from_secs(10)), "Thu, 01 Jan 1970 00:00:00 GMT");
    }

    #[test]
    fn parse_is_the_inverse_of_format() {
        for secs in [0u64, 1, 86_399, 784_111_777, 951_782_400, 1_700_000_000, 4_102_444_799] {
            assert_eq!(parse(&format(at(secs))), Some(at(secs)), "{secs}");
        }
        // Every day for a few years round-trips.
        for day in 0..4000u64 {
            let t = at(day * 86_400 + 12_345);
            assert_eq!(parse(&format(t)), Some(t));
        }
    }

    #[test]
    fn invalid_dates_are_refused() {
        for bad in [
            "",
            "Sun, 06 Nov 1994 08:49:37 UTC",
            "Mon, 06 Nov 1994 08:49:37 GMT", // wrong weekday
            "Sun, 31 Feb 1994 08:49:37 GMT",
            "Sun, 06 Nov 1994 24:49:37 GMT",
            "Sunday, 06-Nov-94 08:49:37 GMT", // obsolete format
            "Sun Nov  6 08:49:37 1994",
            "Sun, 06 Foo 1994 08:49:37 GMT",
            "Sun, 06 Nov 1969 08:49:37 GMT",
        ] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }
}
