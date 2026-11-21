pub const SECOND: i64 = 1_000;
pub const MINUTE: i64 = 60 * SECOND;
pub const HOUR: i64 = 60 * MINUTE;
pub const DAY: i64 = 24 * HOUR;
pub const WEEK: i64 = 7 * DAY;

/// Parses the three valid forms (F1 `2026-10-03T23:58:10Z`, F2 with a
/// `+HH:MM` or `-HH:MM` offset, F3 epoch milliseconds in ASCII digits) into
/// UTC epoch milliseconds. Anything else is None.
#[inline]
pub fn parse(b: &[u8]) -> Option<i64> {
    match b.len() {
        20 if b[19] == b'Z' => civil(b.first_chunk()?),
        25 if b[19] == b'+' || b[19] == b'-' => zoned(b.first_chunk()?),
        _ => digits(b),
    }
}

/// Days since the epoch of the UTC date holding `ms`.
pub fn day_of(ms: i64) -> i32 {
    ms.div_euclid(DAY) as i32
}

/// Eight bytes read as one little-endian word and matched against a pattern
/// in which `D` is an ASCII digit, `?` is any byte and every other byte
/// stands for itself.
struct Shape {
    /// XORed into the word: a digit becomes its value, a matching literal 0.
    xor: u64,
    /// Clears the bytes marked `?`.
    keep: u64,
    /// Added to the XORed bytes: sets a byte's top bit when the byte is over
    /// 9 at a digit or over 0 at a literal.
    over: u64,
}

const fn shape(pattern: &[u8; 8]) -> Shape {
    let mut shape = Shape { xor: 0, keep: 0, over: 0 };
    let mut i = 0;
    while i < 8 {
        let (xor, keep, over): (u8, u8, u8) = match pattern[i] {
            b'D' => (b'0', 0xFF, 0x7F - 9),
            b'?' => (0, 0, 0x7F),
            literal => (literal, 0xFF, 0x7F),
        };
        shape.xor |= (xor as u64) << (8 * i);
        shape.keep |= (keep as u64) << (8 * i);
        shape.over |= (over as u64) << (8 * i);
        i += 1;
    }
    shape
}

/// F1 and F2 bytes 0 to 7.
const DATE: Shape = shape(b"DDDD-DD-");
/// F1 and F2 bytes 8 to 15.
const DAY_TIME: Shape = shape(b"DDTDD:DD");
/// F1 and F2 bytes 12 to 19; `parse` checks byte 19, the zone or the sign.
const SECONDS: Shape = shape(b"D:DD:DD?");
/// F2 bytes 17 to 24; `parse` checks byte 19, the sign.
const OFFSET: Shape = shape(b"DD?DD:DD");
const DIGITS: Shape = shape(b"DDDDDDDD");

const TOP_BITS: u64 = 0x8080_8080_8080_8080;
/// Multipliers that join two-digit numbers into four-digit ones, and those
/// into one eight-digit number.
const PAIRS_TO_FOURS: u64 = 1 + (100 << 16);
const FOURS_TO_EIGHT: u64 = 1 + (10_000 << 32);

/// Days in each month of a common year by month number, 0 for the numbers
/// that name no month.
const MONTH_DAYS: [i64; 16] = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31, 0, 0, 0];

/// Days from March 1 to the first of each month by month number, with
/// January and February at the end of a year that starts in March.
const MARCH_DAYS: [i64; 16] = march_days();

/// The day count of `civil` on 1970-01-01.
const EPOCH: i64 = 865_565;

const fn march_days() -> [i64; 16] {
    let mut days = [0; 16];
    let mut month = 1;
    while month <= 12 {
        days[month] = (153 * ((month as i64 + 9) % 12) + 2) / 5;
        month += 1;
    }
    days
}

#[inline(always)]
fn load(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// The word's digit values, with 0 in every other byte, when it fits the
/// shape; None when any byte does not.
#[inline(always)]
fn fit(word: u64, shape: &Shape) -> Option<u64> {
    let values = (word ^ shape.xor) & shape.keep;
    (((values | values.wrapping_add(shape.over)) & TOP_BITS) == 0).then_some(values)
}

/// Byte i becomes 10 * v[i] + v[i + 1] for byte values v of at most 9: the
/// two-digit number that starts at byte i.
#[inline(always)]
fn pairs(v: u64) -> u64 {
    v.wrapping_mul(10).wrapping_add(v >> 8)
}

#[inline(always)]
fn byte(word: u64, i: u32) -> i64 {
    ((word >> (8 * i)) & 0xFF) as i64
}

/// F1, and the civil time of F2: bytes 0 to 18, with byte 19 checked by
/// `parse`.
#[inline(always)]
fn civil(b: &[u8; 20]) -> Option<i64> {
    let date = pairs(fit(load(b, 0), &DATE)?);
    let time = pairs(fit(load(b, 8), &DAY_TIME)?);
    let seconds = pairs(fit(load(b, 12), &SECONDS)?);
    let (century, year_in_century, month) = (byte(date, 0), byte(date, 2), byte(date, 5));
    let (day, hour, minute, second) = (byte(time, 0), byte(time, 3), byte(time, 6), byte(seconds, 5));
    // A leap year is one that 4 divides, except the years 100 divides and
    // 400 does not.
    let leap = (if year_in_century == 0 { century } else { year_in_century }) % 4 == 0;
    let m = (month & 15) as usize;
    let month_days = MONTH_DAYS[m] + i64::from(leap && month == 2);
    let valid = (((month - 1) as u64) < 12)
        & (((day - 1) as u64) < month_days as u64)
        & (hour < 24)
        & (minute < 60)
        & (second < 60);
    if !valid {
        return None;
    }
    // Years start in March, so a leap day ends its year; the 400 added years
    // keep the count positive from year 0 on.
    let y = (century * 100 + year_in_century + 400 - i64::from(month <= 2)) as u64;
    let days = (365 * y + y / 4 - y / 100 + y / 400) as i64 + MARCH_DAYS[m] + day - 1 - EPOCH;
    Some(days * DAY + hour * HOUR + minute * MINUTE + second * SECOND)
}

/// F2: the civil time with its `+HH:MM` or `-HH:MM` offset taken off.
#[inline(always)]
fn zoned(b: &[u8; 25]) -> Option<i64> {
    let ms = civil(b.first_chunk()?)?;
    let offset = pairs(fit(load(b, 17), &OFFSET)?);
    let (hours, minutes) = (byte(offset, 3), byte(offset, 6));
    if hours > 23 || minutes > 59 {
        return None;
    }
    let offset = (hours * 60 + minutes) * MINUTE;
    Some(if b[19] == b'-' { ms + offset } else { ms - offset })
}

/// F3. From 9 to 16 bytes are read as two words of 8 digits; other lengths
/// byte by byte.
#[inline(always)]
fn digits(b: &[u8]) -> Option<i64> {
    let n = b.len();
    if !(9..=16).contains(&n) {
        return digits_by_byte(b);
    }
    // The high word holds 16 - n zeros, then the first n - 8 bytes; the low
    // word holds the last 8.
    let shift = 8 * (16 - n) as u32;
    let high = fit((load(b, 0) << shift) | (DIGITS.xor & !(u64::MAX << shift)), &DIGITS)?;
    let low = fit(load(b, n - 8), &DIGITS)?;
    Some((eight(high) * 100_000_000 + eight(low)) as i64)
}

/// The number written by 8 digit values, the first in the lowest byte.
#[inline(always)]
fn eight(v: u64) -> u64 {
    let v = pairs(v) & 0x00FF_00FF_00FF_00FF;
    let v = (v.wrapping_mul(PAIRS_TO_FOURS) >> 16) & 0x0000_FFFF_0000_FFFF;
    v.wrapping_mul(FOURS_TO_EIGHT) >> 32
}

#[cold]
#[inline(never)]
fn digits_by_byte(b: &[u8]) -> Option<i64> {
    if b.is_empty() {
        return None;
    }
    let mut value: i64 = 0;
    for &c in b {
        value = value.checked_mul(10)?.checked_add(digit(c)?)?;
    }
    Some(value)
}

fn digit(c: u8) -> Option<i64> {
    c.is_ascii_digit().then(|| i64::from(c - b'0'))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The byte-at-a-time parser that the word-at-a-time one must agree with
    /// on every input.
    mod reference {
        use crate::timestamp::{DAY, HOUR, MINUTE, SECOND};

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

        pub fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
            let year = if month <= 2 { year - 1 } else { year };
            let era = year.div_euclid(400);
            let year_of_era = year - era * 400;
            let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
            era * 146_097 + year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year - 719_468
        }
    }

    use reference::days_from_civil;

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

    #[test]
    fn accepts_the_edges() {
        assert_eq!(parse(b"2000-02-29T00:00:00Z"), Some(days_from_civil(2000, 2, 29) * DAY));
        assert_eq!(parse(b"2400-02-29T00:00:00Z"), Some(days_from_civil(2400, 2, 29) * DAY));
        assert_eq!(parse(b"0000-02-29T00:00:00Z"), Some(days_from_civil(0, 2, 29) * DAY));
        assert_eq!(parse(b"0000-01-01T00:00:00Z"), Some(days_from_civil(0, 1, 1) * DAY));
        assert_eq!(parse(b"9999-12-31T23:59:59Z"), Some(days_from_civil(9999, 12, 31) * DAY + DAY - SECOND));
        assert_eq!(parse(b"1969-12-31T23:59:59Z"), Some(-SECOND));
        assert_eq!(parse(b"1970-01-01T00:00:00+00:00"), Some(0));
        assert_eq!(parse(b"1970-01-01T00:00:00-23:59"), Some(23 * HOUR + 59 * MINUTE));
        assert_eq!(parse(b"2028-03-01T00:30:00+01:00"), Some(days_from_civil(2028, 2, 29) * DAY + 23 * HOUR + 30 * MINUTE));
        assert_eq!(parse(b"0"), Some(0));
        assert_eq!(parse(b"000000000"), Some(0));
        assert_eq!(parse(b"123456789"), Some(123_456_789));
        assert_eq!(parse(b"9999999999999999"), Some(9_999_999_999_999_999));
        assert_eq!(parse(b"9223372036854775807"), Some(i64::MAX));
        assert_eq!(parse(b"0000009223372036854775807"), Some(i64::MAX));
    }

    #[test]
    fn rejects_each_broken_rule() {
        for bad in [
            &b"2026-00-03T23:58:10Z"[..],
            b"2026-10-00T23:58:10Z",
            b"2026-10-32T23:58:10Z",
            b"2026-04-31T23:58:10Z",
            b"1900-02-29T00:00:00Z",
            b"2100-02-29T00:00:00Z",
            b"2026-10-03T23:60:10Z",
            b"2026-10-03T99:58:10Z",
            b"2026-10-03T23:58:99Z",
            b"2026-1a-03T23:58:10Z",
            b"2026/10/03T23:58:10Z",
            b"2026-10-03T23-58-10Z",
            b"2026-10-03T23:58:10 ",
            b"2026-10-03T23:58:10-24:00",
            b"2026-10-03T23:58:10-05:60",
            b"2026-10-03T23:58:10+2:00",
            b"2026-10-03T23:58:10+02:0a",
            b"2026-10-03T23:58:10+02:000",
            b"2026-10-03T23:58:10Z+02:00",
            b"2026-10-03t23:58:10+02:00",
            b"2026-10-03 23:58:10+02:00",
            b"2026-10-03T23:58:60+02:00",
            b"2027-02-29T23:58:10-02:00",
            b"9223372036854775808",
            b"0000009223372036854775808",
            b"99999999999999999999",
            b"1791071890000 ",
            b" 1791071890000",
            b"+1791071890000",
            b"17910718900:0",
            b"179107189/000",
        ] {
            assert_eq!(parse(bad), None, "{:?}", String::from_utf8_lossy(bad));
            assert_eq!(reference::parse(bad), None, "{:?}", String::from_utf8_lossy(bad));
        }
    }

    /// Writes `value` into `out` as decimal digits, zeros on the left.
    fn put(out: &mut [u8], mut value: usize) {
        for c in out.iter_mut().rev() {
            *c = b'0' + (value % 10) as u8;
            value /= 10;
        }
    }

    fn agree(text: &[u8]) -> bool {
        let got = parse(text);
        assert_eq!(got, reference::parse(text), "{:?}", String::from_utf8_lossy(text));
        got.is_some()
    }

    #[test]
    fn agrees_with_the_reference_on_every_date() {
        let mut text = *b"0000-00-00T12:34:56Z";
        let mut accepted = 0;
        for year in 0..10_000 {
            put(&mut text[0..4], year);
            for month in 0..14 {
                put(&mut text[5..7], month);
                for day in 0..33 {
                    put(&mut text[8..10], day);
                    accepted += usize::from(agree(&text));
                }
            }
        }
        assert_eq!(accepted, 3_652_425);
        // Every two-digit month and day in years at the leap-rule edges.
        let mut accepted = 0;
        for year in [0, 1, 4, 100, 400, 1900, 1970, 2000, 2024, 2025, 2026, 2027, 2028, 2100, 9999] {
            put(&mut text[0..4], year);
            for month in 0..100 {
                put(&mut text[5..7], month);
                for day in 0..100 {
                    put(&mut text[8..10], day);
                    accepted += usize::from(agree(&text));
                }
            }
        }
        // Years 0, 4, 400, 2000, 2024 and 2028 are leap years; the other 9 are not.
        assert_eq!(accepted, 9 * 365 + 6 * 366);
    }

    #[test]
    fn agrees_with_the_reference_on_every_time_and_offset() {
        let mut utc = *b"2028-02-29T00:00:00Z";
        let mut offset = *b"2028-02-29T00:00:00+00:00";
        let mut accepted = 0;
        for hour in 0..100 {
            for minute in 0..100 {
                for second in 0..100 {
                    for text in [&mut utc[..], &mut offset[..]] {
                        put(&mut text[11..13], hour);
                        put(&mut text[14..16], minute);
                        put(&mut text[17..19], second);
                        accepted += usize::from(agree(text));
                    }
                }
            }
        }
        assert_eq!(accepted, 2 * 24 * 60 * 60);
        let mut text = *b"2028-02-29T00:30:00+00:00";
        let mut accepted = 0;
        for sign in [b'+', b'-'] {
            text[19] = sign;
            for hours in 0..100 {
                put(&mut text[20..22], hours);
                for minutes in 0..100 {
                    put(&mut text[23..25], minutes);
                    accepted += usize::from(agree(&text));
                }
            }
        }
        assert_eq!(accepted, 2 * 24 * 60);
    }

    #[test]
    fn agrees_with_the_reference_on_digit_strings() {
        let numbers = ["0", "7", "86400000", "1791071890000", "9999999999999999", "9223372036854775807", "9223372036854775808", "18446744073709551616"];
        let mut accepted = 0;
        for number in numbers {
            for len in 0..=25 {
                // Leading zeros up to `len` bytes, or the last `len` digits.
                let text: Vec<u8> = if len >= number.len() {
                    std::iter::repeat(b'0').take(len - number.len()).chain(number.bytes()).collect()
                } else {
                    number.as_bytes()[number.len() - len..].to_vec()
                };
                accepted += usize::from(agree(&text));
            }
        }
        assert!(accepted > 150, "{accepted}");
        let base = b"1791071890000123";
        for len in 1..=16 {
            for at in 0..len {
                for c in 0..=255u8 {
                    let mut text = base[..len].to_vec();
                    text[at] = c;
                    agree(&text);
                }
            }
        }
    }

    #[test]
    fn agrees_with_the_reference_on_every_byte_at_every_position() {
        for template in [&b"2028-02-29T23:59:59Z"[..], b"2026-10-03T19:58:10-04:00", b"2026-10-03T19:58:10+04:00"] {
            assert!(agree(template));
            for at in 0..template.len() {
                for c in 0..=255u8 {
                    let mut text = template.to_vec();
                    text[at] = c;
                    agree(&text);
                }
            }
        }
    }

    /// A small deterministic generator (splitmix64) for the randomized tests.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    #[test]
    fn agrees_with_the_reference_on_random_inputs() {
        let odd = [b'-', b':', b'T', b'Z', b't', b'z', b'+', b' ', b'.', b'/', b'e', 0x00, 0x7F, 0x80, 0xB0, 0xFF];
        let mut rng = Rng(61);
        let mut accepted = 0;
        for _ in 0..2_000_000 {
            let mut text: Vec<u8> = match rng.below(4) {
                0 | 1 => {
                    let mut text = format!(
                        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
                        rng.below(10_000),
                        rng.below(14),
                        rng.below(33),
                        rng.below(26),
                        rng.below(62),
                        rng.below(62)
                    )
                    .into_bytes();
                    if rng.below(2) == 0 {
                        text.push(b'Z');
                    } else {
                        let sign = if rng.below(2) == 0 { '+' } else { '-' };
                        text.extend(format!("{sign}{:02}:{:02}", rng.below(26), rng.below(62)).bytes());
                    }
                    text
                }
                2 => {
                    let len = rng.below(26);
                    (0..len).map(|_| b'0' + rng.below(10) as u8).collect()
                }
                _ => {
                    let len = rng.below(27);
                    (0..len).map(|_| if rng.below(2) == 0 { b'0' + rng.below(10) as u8 } else { odd[rng.below(odd.len())] }).collect()
                }
            };
            for _ in 0..rng.below(3) {
                if !text.is_empty() {
                    let at = rng.below(text.len());
                    text[at] = if rng.below(2) == 0 { b'0' + rng.below(10) as u8 } else { odd[rng.below(odd.len())] };
                }
            }
            accepted += usize::from(agree(&text));
        }
        assert!(accepted > 200_000, "{accepted}");
    }
}
