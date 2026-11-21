pub const SECOND: i64 = 1_000;
pub const MINUTE: i64 = 60 * SECOND;
pub const HOUR: i64 = 60 * MINUTE;
pub const DAY: i64 = 24 * HOUR;
pub const WEEK: i64 = 7 * DAY;

/// Parses the three valid forms (F1 `2026-10-03T23:58:10Z`, F2 with a
/// `+HH:MM` or `-HH:MM` offset, F3 epoch milliseconds in ASCII digits) into
/// UTC epoch milliseconds. Anything else is None.
pub fn parse(b: &[u8]) -> Option<i64> {
    match b.len() {
        20 if b[19] == b'Z' => civil(b),
        25 if b[19] == b'+' || b[19] == b'-' => {
            let ms = civil(b)?;
            if b[22] != b':' {
                return None;
            }
            let hours = two(b, 20)?;
            let minutes = two(b, 23)?;
            if hours > 23 || minutes > 59 {
                return None;
            }
            let offset = (hours * 60 + minutes) * MINUTE;
            Some(if b[19] == b'-' { ms + offset } else { ms - offset })
        }
        _ => digits(b),
    }
}

/// Days since the epoch of the UTC date holding `ms`.
pub fn day_of(ms: i64) -> i32 {
    ms.div_euclid(DAY) as i32
}

fn civil(b: &[u8]) -> Option<i64> {
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let year = four(b, 0)?;
    let month = two(b, 5)?;
    let day = two(b, 8)?;
    let hour = two(b, 11)?;
    let minute = two(b, 14)?;
    let second = two(b, 17)?;
    if !(1..=12).contains(&month)
        || day < 1
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    Some(days_from_civil(year, month, day) * DAY + hour * HOUR + minute * MINUTE + second * SECOND)
}

fn digit(c: u8) -> Option<i64> {
    c.is_ascii_digit().then(|| i64::from(c - b'0'))
}

fn two(b: &[u8], at: usize) -> Option<i64> {
    Some(digit(b[at])? * 10 + digit(b[at + 1])?)
}

fn four(b: &[u8], at: usize) -> Option<i64> {
    Some(two(b, at)? * 100 + two(b, at + 2)?)
}

fn digits(b: &[u8]) -> Option<i64> {
    if b.is_empty() {
        return None;
    }
    let mut value: i64 = 0;
    for &c in b {
        value = value.checked_mul(10)?.checked_add(digit(c)?)?;
    }
    Some(value)
}

fn is_leap_year(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
    }
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    era * 146_097 + year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_three_forms() {
        assert_eq!(parse(b"2026-10-03T23:58:10Z"), Some(1_791_071_890_000));
        assert_eq!(parse(b"2026-10-03T19:58:10-04:00"), Some(1_791_071_890_000));
        assert_eq!(parse(b"2026-10-04T01:28:10+01:30"), Some(1_791_071_890_000));
        assert_eq!(parse(b"1791071890000"), Some(1_791_071_890_000));
        assert_eq!(parse(b"0001791071890000"), Some(1_791_071_890_000));
        assert_eq!(parse(b"2026-10-03T23:58:10-00:00"), Some(1_791_071_890_000));
        assert_eq!(parse(b"2026-10-03T23:58:10+23:59"), Some(1_791_071_890_000 - 23 * HOUR - 59 * MINUTE));
        assert_eq!(parse(b"86400000"), Some(DAY));
        assert_eq!(parse(b"2028-02-29T00:00:00Z"), Some(days_from_civil(2028, 2, 29) * DAY));
        assert_eq!(parse(b"1970-01-01T00:00:00Z"), Some(0));
    }

    #[test]
    fn rejects_every_bad_form() {
        let arabic_indic = "\u{0662}\u{0665}\u{0660}";
        let full_width_year = "\u{FF12}\u{FF10}\u{FF12}\u{FF16}-10-03T23:58:10Z";
        for bad in [
            &b""[..],
            b"2026-10-03T23:58:10.000Z",
            b"2026-10-03T23:58:10z",
            b"2026-10-03 23:58:10Z",
            b"2026-10-03T23:58:10",
            b"2026-10-03t23:58:10Z",
            b"1791071890000.0",
            b"1.79e12",
            b"-5",
            b"2026-13-03T23:58:10Z",
            b"2026-02-30T23:58:10Z",
            b"2027-02-29T23:58:10Z",
            b"2026-10-03T24:00:00Z",
            b"2026-10-03T23:58:60Z",
            b"2026-10-03T23:58:10+24:00",
            b"2026-10-03T23:58:10+05:60",
            b"2026-10-03T23:58:10+0200",
            b"2026-10-03T23:58:10+02-00",
            b"123456789012345678901",
            arabic_indic.as_bytes(),
            full_width_year.as_bytes(),
        ] {
            assert_eq!(parse(bad), None, "{:?}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn day_of_floors() {
        assert_eq!(day_of(0), 0);
        assert_eq!(day_of(DAY - 1), 0);
        assert_eq!(day_of(DAY), 1);
        assert_eq!(day_of(-1), -1);
    }
}
