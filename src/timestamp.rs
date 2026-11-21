pub const SECOND: i64 = 1_000;
pub const MINUTE: i64 = 60 * SECOND;
pub const HOUR: i64 = 60 * MINUTE;
pub const DAY: i64 = 24 * HOUR;
pub const WEEK: i64 = 7 * DAY;

#[inline]
pub fn parse(b: &[u8]) -> Option<i64> {
    match b.len() {
        20 if b[19] == b'Z' => civil(b.first_chunk()?),
        25 if b[19] == b'+' || b[19] == b'-' => zoned(b.first_chunk()?),
        _ => digits(b),
    }
}

pub fn day_of(ms: i64) -> i32 {
    ms.div_euclid(DAY) as i32
}

#[derive(Clone, Copy, Debug)]
pub struct Months {
    words: [u64; MONTHS],
    day0: [i64; MONTHS],
    first: [i64; MONTHS],
    last: [i64; MONTHS],
}

const MONTHS: usize = 3;

impl Months {
    pub fn new(from: i64, to: i64) -> Months {
        let mut months = Months { words: [0; MONTHS], day0: [0; MONTHS], first: [1; MONTHS], last: [0; MONTHS] };
        let mut used = 0;
        for day in from..=to.min(from.saturating_add(92)) {
            let Some((year, month, date)) = civil_from_days(day) else {
                continue;
            };
            let text = format!("{year:04}-{month:02}-{date:02}T00:00:00Z");
            if parse(text.as_bytes()) != Some(day * DAY) {
                continue;
            }
            let word = load(text.as_bytes(), 0);
            let k = match months.words[..used].iter().position(|&w| w == word) {
                Some(k) => k,
                None if used < MONTHS => {
                    months.words[used] = word;
                    months.day0[used] = (day - date) * DAY;
                    months.first[used] = date;
                    months.last[used] = date;
                    used += 1;
                    used - 1
                }
                None => continue,
            };
            if months.day0[k] + date * DAY == day * DAY {
                months.first[k] = months.first[k].min(date);
                months.last[k] = months.last[k].max(date);
            }
        }
        months
    }

    #[inline(always)]
    pub fn parse(&self, b: &[u8]) -> Option<i64> {
        match b.len() {
            20 if b[19] == b'Z' => self.civil(b.first_chunk()?),
            25 if b[19] == b'+' || b[19] == b'-' => self.zoned(b.first_chunk()?),
            _ => digits(b),
        }
    }

    #[inline(always)]
    fn civil(&self, b: &[u8; 20]) -> Option<i64> {
        let word = load(b, 0);
        let (in0, in1, in2) = (word == self.words[0], word == self.words[1], word == self.words[2]);
        if !(in0 | in1 | in2) {
            return civil_cold(b);
        }
        let k = if in2 { 2 } else { usize::from(in1) };
        let time = pairs(fit(load(b, 8), &DAY_TIME)?);
        let seconds = pairs(fit(load(b, 12), &SECONDS)?);
        let (day, hour, minute, second) = (byte(time, 0), byte(time, 3), byte(time, 6), byte(seconds, 5));
        if day < self.first[k] || day > self.last[k] {
            return civil_cold(b);
        }
        if hour >= 24 || minute >= 60 || second >= 60 {
            return None;
        }
        Some(self.day0[k] + day * DAY + hour * HOUR + minute * MINUTE + second * SECOND)
    }

    #[inline(always)]
    fn zoned(&self, b: &[u8; 25]) -> Option<i64> {
        let ms = self.civil(b.first_chunk()?)?;
        offset(b, ms)
    }
}

#[cold]
#[inline(never)]
fn civil_cold(b: &[u8; 20]) -> Option<i64> {
    civil(b)
}

fn civil_from_days(day: i64) -> Option<(i64, i64, i64)> {
    if !(-800_000..=3_000_000).contains(&day) {
        return None;
    }
    let z = day + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let date = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (0..=9999).contains(&year).then_some((year, month, date))
}

struct Shape {
    xor: u64,
    keep: u64,
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

const DATE: Shape = shape(b"DDDD-DD-");
const DAY_TIME: Shape = shape(b"DDTDD:DD");
const SECONDS: Shape = shape(b"D:DD:DD?");
const OFFSET: Shape = shape(b"DD?DD:DD");
const DIGITS: Shape = shape(b"DDDDDDDD");

const TOP_BITS: u64 = 0x8080_8080_8080_8080;
const PAIRS_TO_FOURS: u64 = 1 + (100 << 16);
const FOURS_TO_EIGHT: u64 = 1 + (10_000 << 32);

const MONTH_DAYS: [i64; 16] = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31, 0, 0, 0];

const MARCH_DAYS: [i64; 16] = march_days();

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

#[inline(always)]
fn fit(word: u64, shape: &Shape) -> Option<u64> {
    let values = (word ^ shape.xor) & shape.keep;
    (((values | values.wrapping_add(shape.over)) & TOP_BITS) == 0).then_some(values)
}

#[inline(always)]
fn pairs(v: u64) -> u64 {
    v.wrapping_mul(10).wrapping_add(v >> 8)
}

#[inline(always)]
fn byte(word: u64, i: u32) -> i64 {
    ((word >> (8 * i)) & 0xFF) as i64
}

#[inline(always)]
fn civil(b: &[u8; 20]) -> Option<i64> {
    let date = pairs(fit(load(b, 0), &DATE)?);
    let time = pairs(fit(load(b, 8), &DAY_TIME)?);
    let seconds = pairs(fit(load(b, 12), &SECONDS)?);
    let (century, year_in_century, month) = (byte(date, 0), byte(date, 2), byte(date, 5));
    let (day, hour, minute, second) = (byte(time, 0), byte(time, 3), byte(time, 6), byte(seconds, 5));
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
    let y = (century * 100 + year_in_century + 400 - i64::from(month <= 2)) as u64;
    let days = (365 * y + y / 4 - y / 100 + y / 400) as i64 + MARCH_DAYS[m] + day - 1 - EPOCH;
    Some(days * DAY + hour * HOUR + minute * MINUTE + second * SECOND)
}

#[inline(always)]
fn zoned(b: &[u8; 25]) -> Option<i64> {
    let ms = civil(b.first_chunk()?)?;
    offset(b, ms)
}

#[inline(always)]
fn offset(b: &[u8; 25], ms: i64) -> Option<i64> {
    let offset = pairs(fit(load(b, 17), &OFFSET)?);
    let (hours, minutes) = (byte(offset, 3), byte(offset, 6));
    if hours > 23 || minutes > 59 {
        return None;
    }
    let offset = (hours * 60 + minutes) * MINUTE;
    Some(if b[19] == b'-' { ms + offset } else { ms - offset })
}

#[inline(always)]
fn digits(b: &[u8]) -> Option<i64> {
    let n = b.len();
    if !(9..=16).contains(&n) {
        return digits_by_byte(b);
    }
    let shift = 8 * (16 - n) as u32;
    let high = fit((load(b, 0) << shift) | (DIGITS.xor & !(u64::MAX << shift)), &DIGITS)?;
    let low = fit(load(b, n - 8), &DIGITS)?;
    Some((eight(high) * 100_000_000 + eight(low)) as i64)
}

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

    #[test]
    fn civil_from_days_inverts_days_from_civil() {
        let mut checked = 0;
        let mut day = days_from_civil(0, 1, 1);
        while day <= days_from_civil(9999, 12, 31) {
            let (year, month, date) = civil_from_days(day).unwrap();
            assert_eq!(days_from_civil(year, month, date), day, "{day}");
            assert!((1..=12).contains(&month) && (1..=31).contains(&date), "{day}");
            checked += 1;
            day += 13;
        }
        assert!(checked > 280_000, "{checked}");
        assert_eq!(civil_from_days(days_from_civil(0, 1, 1) - 1), None);
        assert_eq!(civil_from_days(days_from_civil(9999, 12, 31) + 1), None);
        assert_eq!(civil_from_days(0), Some((1970, 1, 1)));
        assert_eq!(civil_from_days(days_from_civil(2028, 2, 29)), Some((2028, 2, 29)));
    }

    fn month_tables() -> Vec<(Months, i64, i64)> {
        let mut tables = Vec::new();
        for (from, to) in [((2025, 7, 15), (2025, 8, 5)), ((2026, 1, 20), (2026, 2, 12)), ((2027, 12, 20), (2028, 1, 10)), ((2028, 2, 14), (2028, 3, 8)), ((2100, 2, 20), (2100, 3, 3)), ((2026, 1, 25), (2026, 3, 3))] {
            let (from, to) = (days_from_civil(from.0, from.1, from.2), days_from_civil(to.0, to.1, to.2));
            tables.push((Months::new(from, to), from, to));
        }
        tables.push((Months::new(10, 9), 10, 9));
        tables
    }

    fn months_agree(months: &Months, text: &[u8]) -> bool {
        let got = months.parse(text);
        assert_eq!(got, parse(text), "{:?} with {months:?}", String::from_utf8_lossy(text));
        got.is_some()
    }

    #[test]
    fn months_use_every_day_of_their_range_and_no_other() {
        for (months, from, to) in month_tables() {
            let mut fast = 0;
            for k in 0..MONTHS {
                for day in months.first[k]..=months.last[k] {
                    let epoch_day = months.day0[k] / DAY + day;
                    assert!((from..=to).contains(&epoch_day), "{months:?}");
                    let (year, month, date) = civil_from_days(epoch_day).unwrap();
                    assert_eq!(months.words[k], load(format!("{year:04}-{month:02}-").as_bytes(), 0));
                    assert_eq!(date, day);
                    fast += 1;
                }
            }
            assert_eq!(fast, (to - from + 1).max(0), "{months:?}");
        }
    }

    #[test]
    fn months_take_the_first_93_days_of_a_long_range() {
        for months in [Months::new(0, i64::MAX), Months::new(0, 400)] {
            let days: i64 = (0..MONTHS).map(|k| (months.last[k] - months.first[k] + 1).max(0)).sum();
            assert_eq!(days, 31 + 28 + 31);
            for text in [&b"1970-02-14T00:00:00Z"[..], b"1970-04-02T00:00:00Z", b"1970-12-31T23:59:59Z"] {
                months_agree(&months, text);
            }
        }
        let months = Months::new(i64::MAX - 5, i64::MAX);
        assert!((0..MONTHS).all(|k| months.first[k] > months.last[k]));
        assert_eq!(civil_from_days(i64::MAX), None);
        assert_eq!(civil_from_days(i64::MIN), None);
    }

    #[test]
    fn months_agree_with_parse_on_every_date_and_time() {
        let times: [&[u8]; 7] = [b"T00:00:00", b"T23:59:59", b"T12:34:56", b"T24:00:00", b"T23:60:00", b"T23:59:60", b"t12:34:56"];
        let zones: [&[u8]; 7] = [b"Z", b"+00:00", b"-10:00", b"+12:00", b"+23:59", b"-24:00", b"+05:60"];
        for (months, _, _) in month_tables() {
            let mut accepted = 0;
            for year in [2025, 2026, 2027, 2028, 2100] {
                for month in 0..=13 {
                    for day in 0..=32 {
                        let date = format!("{year:04}-{month:02}-{day:02}");
                        for time in times {
                            for zone in zones {
                                let text = [date.as_bytes(), time, zone].concat();
                                accepted += usize::from(months_agree(&months, &text));
                            }
                        }
                    }
                }
            }
            assert!(accepted > 10_000, "{accepted}");
        }
    }

    #[test]
    fn months_agree_with_parse_on_every_byte_at_every_position() {
        for (months, _, _) in month_tables() {
            for template in [&b"2025-07-31T23:59:59Z"[..], b"2026-02-01T00:00:00+05:30", b"2028-02-29T12:00:00-04:00", b"2028-01-01T00:00:00Z", b"2100-02-28T00:00:00Z", b"1752624000000"] {
                agree(template);
                months_agree(&months, template);
                for at in 0..template.len() {
                    for c in 0..=255u8 {
                        let mut text = template.to_vec();
                        text[at] = c;
                        months_agree(&months, &text);
                    }
                }
            }
            months_agree(&months, &[0u8; 20]);
            months_agree(&months, &[[0u8; 19].as_slice(), b"Z"].concat());
            months_agree(&months, &[[0u8; 8].as_slice(), b"31T00:00:00Z"].concat());
        }
    }

    #[test]
    fn months_agree_with_parse_on_random_inputs() {
        let mut rng = Rng(67);
        let tables = month_tables();
        let mut accepted = 0;
        for _ in 0..1_000_000 {
            let (months, from, to) = &tables[rng.below(tables.len())];
            let day = from - 3 + rng.below((to - from + 7).max(1) as usize) as i64;
            let (year, month, date) = civil_from_days(day).unwrap_or((2026, 1, 1));
            let mut text = format!("{year:04}-{month:02}-{date:02}T{:02}:{:02}:{:02}", rng.below(25), rng.below(61), rng.below(61)).into_bytes();
            match rng.below(3) {
                0 => text.push(b'Z'),
                1 => text.extend(format!("{}{:02}:{:02}", if rng.below(2) == 0 { '+' } else { '-' }, rng.below(25), rng.below(61)).bytes()),
                _ => text = (day * DAY + rng.below(DAY as usize) as i64).to_string().into_bytes(),
            }
            if rng.below(4) == 0 {
                let at = rng.below(text.len());
                text[at] = [b'0', b'9', b':', b'-', b'T', b'Z', b'x', 0x00, 0xFF][rng.below(9)];
            }
            accepted += usize::from(months_agree(months, &text));
        }
        assert!(accepted > 500_000, "{accepted}");
    }
}
