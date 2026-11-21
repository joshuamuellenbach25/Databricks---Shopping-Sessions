use std::path::Path;

use hashbrown::HashMap;

use crate::dialect;
use crate::ids::Key;
use crate::timestamp;

#[derive(Clone, Copy, Debug)]
pub struct Window {
    pub start: i64,
    pub end: i64,
}

pub struct Products {
    pub map: HashMap<Key, u32>,
    pub categories: Vec<Vec<u8>>,
    /// The category of a product not in the file.
    pub unknown: u32,
    /// Position of each category in byte-wise order, for ties.
    pub rank: Vec<u32>,
}

pub struct Campaigns {
    pub map: HashMap<Key, u32>,
    /// Index 0 is `organic`; the rest follow campaigns.csv.
    pub ids: Vec<Vec<u8>>,
    pub channels: Vec<Vec<u8>>,
}

pub fn read_manifest(dir: &Path) -> Result<Window, String> {
    let path = dir.join("manifest.json");
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    manifest_window(&text).map_err(|e| format!("{}: {e}", path.display()))
}

fn manifest_window(text: &str) -> Result<Window, String> {
    let members = json_object(text)?;
    let start = member(&members, "window_start")?;
    let end = member(&members, "window_end")?;
    let start = timestamp::parse(start.as_bytes()).ok_or_else(|| format!("bad window_start {start}"))?;
    let end = timestamp::parse(end.as_bytes()).ok_or_else(|| format!("bad window_end {end}"))?;
    if end <= start {
        return Err("window_end is not after window_start".to_string());
    }
    Ok(Window { start, end })
}

fn member<'a>(members: &'a [(String, Option<String>)], name: &str) -> Result<&'a str, String> {
    match members.iter().rev().find(|(key, _)| key == name) {
        Some((_, Some(value))) => Ok(value.as_str()),
        Some((_, None)) => Err(format!("{name} is not a string")),
        None => Err(format!("no {name}")),
    }
}

fn json_object(text: &str) -> Result<Vec<(String, Option<String>)>, String> {
    let mut json = Json { text, at: 0 };
    let mut members = Vec::new();
    json.expect(b'{')?;
    if !json.eat(b'}') {
        loop {
            let key = json.string()?;
            json.expect(b':')?;
            let value = if json.peek() == Some(b'"') {
                Some(json.string()?)
            } else {
                json.skip_value()?;
                None
            };
            members.push((key, value));
            if json.eat(b'}') {
                break;
            }
            json.expect(b',')?;
        }
    }
    if json.peek().is_some() {
        return Err(json.error());
    }
    Ok(members)
}

struct Json<'a> {
    text: &'a str,
    at: usize,
}

impl Json<'_> {
    fn error(&self) -> String {
        format!("not valid JSON at byte {}", self.at)
    }

    fn peek(&mut self) -> Option<u8> {
        let bytes = self.text.as_bytes();
        while matches!(bytes.get(self.at), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
        bytes.get(self.at).copied()
    }

    fn eat(&mut self, byte: u8) -> bool {
        let found = self.peek() == Some(byte);
        self.at += usize::from(found);
        found
    }

    fn expect(&mut self, byte: u8) -> Result<(), String> {
        if self.eat(byte) {
            Ok(())
        } else {
            Err(self.error())
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let text = self.text;
        let bytes = text.as_bytes();
        let mut out = String::new();
        loop {
            let run = self.at;
            while bytes.get(self.at).is_some_and(|&b| b >= 0x20 && b != b'"' && b != b'\\') {
                self.at += 1;
            }
            out.push_str(&text[run..self.at]);
            match bytes.get(self.at) {
                Some(b'"') => {
                    self.at += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.at += 1;
                    out.push(self.escape()?);
                }
                _ => return Err(self.error()),
            }
        }
    }

    fn escape(&mut self) -> Result<char, String> {
        let decoded = match self.text.as_bytes().get(self.at).copied() {
            Some(b'"') => '"',
            Some(b'\\') => '\\',
            Some(b'/') => '/',
            Some(b'b') => '\x08',
            Some(b'f') => '\x0c',
            Some(b'n') => '\n',
            Some(b'r') => '\r',
            Some(b't') => '\t',
            Some(b'u') => {
                self.at += 1;
                return self.code_point();
            }
            _ => return Err(self.error()),
        };
        self.at += 1;
        Ok(decoded)
    }

    fn code_point(&mut self) -> Result<char, String> {
        let unit = self.hex4()?;
        let bytes = self.text.as_bytes();
        if (0xD800..0xDC00).contains(&unit) && bytes.get(self.at) == Some(&b'\\') && bytes.get(self.at + 1) == Some(&b'u') {
            let resume = self.at;
            self.at += 2;
            let low = self.hex4()?;
            if (0xDC00..0xE000).contains(&low) {
                return Ok(char::from_u32(0x10000 + ((unit - 0xD800) << 10) + (low - 0xDC00)).unwrap_or(char::REPLACEMENT_CHARACTER));
            }
            self.at = resume;
        }
        Ok(char::from_u32(unit).unwrap_or(char::REPLACEMENT_CHARACTER))
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let digits = self.text.as_bytes().get(self.at..self.at + 4).ok_or_else(|| self.error())?;
        let mut value = 0;
        for &digit in digits {
            value = value * 16 + char::from(digit).to_digit(16).ok_or_else(|| self.error())?;
        }
        self.at += 4;
        Ok(value)
    }

    fn skip_value(&mut self) -> Result<(), String> {
        let mut open: Vec<u8> = Vec::new();
        loop {
            match self.peek() {
                Some(b'{') => {
                    self.at += 1;
                    if !self.eat(b'}') {
                        open.push(b'}');
                        self.string()?;
                        self.expect(b':')?;
                        continue;
                    }
                }
                Some(b'[') => {
                    self.at += 1;
                    if !self.eat(b']') {
                        open.push(b']');
                        continue;
                    }
                }
                Some(b'"') => {
                    self.string()?;
                }
                _ => self.scalar()?,
            }
            loop {
                let Some(&close) = open.last() else {
                    return Ok(());
                };
                if self.eat(close) {
                    open.pop();
                } else {
                    self.expect(b',')?;
                    if close == b'}' {
                        self.string()?;
                        self.expect(b':')?;
                    }
                    break;
                }
            }
        }
    }

    fn scalar(&mut self) -> Result<(), String> {
        let rest = &self.text.as_bytes()[self.at..];
        for word in [&b"true"[..], b"false", b"null", b"NaN", b"Infinity", b"-Infinity"] {
            if rest.starts_with(word) {
                self.at += word.len();
                return Ok(());
            }
        }
        let digits = |from: usize| from + rest[from..].iter().take_while(|b| b.is_ascii_digit()).count();
        let sign = usize::from(rest.first() == Some(&b'-'));
        let mut end = match rest.get(sign) {
            Some(b'0') => sign + 1,
            Some(b'1'..=b'9') => digits(sign),
            _ => return Err(self.error()),
        };
        if rest.get(end) == Some(&b'.') && rest.get(end + 1).is_some_and(u8::is_ascii_digit) {
            end = digits(end + 1);
        }
        if matches!(rest.get(end), Some(b'e' | b'E')) {
            let digit = end + 1 + usize::from(matches!(rest.get(end + 1), Some(b'+' | b'-')));
            if rest.get(digit).is_some_and(u8::is_ascii_digit) {
                end = digits(digit);
            }
        }
        self.at += end;
        Ok(())
    }
}

fn rows(path: &Path) -> Result<Vec<(Vec<u8>, Vec<u8>)>, String> {
    let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut out = Vec::new();
    for line in data.split(|&b| b == b'\n').skip(1) {
        let mut fields: [&[u8]; 2] = [&[]; 2];
        if dialect::split(line, &mut fields) {
            out.push((fields[0].to_vec(), fields[1].to_vec()));
        }
    }
    Ok(out)
}

pub fn read_products(dir: &Path) -> Result<Products, String> {
    let mut map = HashMap::new();
    let mut categories: Vec<Vec<u8>> = Vec::new();
    let mut by_name: HashMap<Vec<u8>, u32> = HashMap::new();
    let next = |categories: &Vec<Vec<u8>>| u32::try_from(categories.len()).map_err(|_| format!("products.csv: more than {} categories", u32::MAX));
    for (product, category) in rows(&dir.join("products.csv"))? {
        let index = next(&categories)?;
        let index = *by_name.entry(category.clone()).or_insert_with(|| {
            categories.push(category.clone());
            index
        });
        map.insert(Key::from_bytes(&product), index);
    }
    let index = next(&categories)?;
    let unknown = *by_name.entry(b"unknown".to_vec()).or_insert_with(|| {
        categories.push(b"unknown".to_vec());
        index
    });
    let mut order: Vec<usize> = (0..categories.len()).collect();
    order.sort_by(|&a, &b| categories[a].cmp(&categories[b]));
    let mut rank = vec![0u32; categories.len()];
    for (position, &index) in order.iter().enumerate() {
        rank[index] = position as u32;
    }
    Ok(Products { map, categories, unknown, rank })
}

pub fn read_campaigns(dir: &Path) -> Result<Campaigns, String> {
    let mut map = HashMap::new();
    let mut ids = vec![b"organic".to_vec()];
    let mut channels = vec![b"organic".to_vec()];
    for (campaign, channel) in rows(&dir.join("campaigns.csv"))? {
        let index = u32::try_from(ids.len()).map_err(|_| format!("campaigns.csv: more than {} campaigns", u32::MAX))?;
        if map.insert(Key::from_bytes(&campaign), index).is_none() {
            ids.push(campaign);
            channels.push(channel);
        }
    }
    Ok(Campaigns { map, ids, channels })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window_of(text: &str) -> Result<(i64, i64), String> {
        manifest_window(&text.replace('~', "\\")).map(|w| (w.start, w.end))
    }

    #[test]
    fn manifest_names_and_values_are_decoded_as_json() {
        let plain = window_of(r#"{"window_start": "2026-10-01T00:00:00Z", "window_end": "2026-10-15T00:00:00Z"}"#).unwrap();
        assert_eq!(plain, (timestamp::parse(b"2026-10-01T00:00:00Z").unwrap(), 1_792_022_400_000));
        for text in [
            r#"{"window_start":"2026-10-01T00:00:00~u005a","window_end":"2026-10-15T00:00:00Z"}"#,
            r#"{"window~u005Fstart":"2026-10-01T00:00:00Z","window_end":"2026~u002d10-15T00:00:00~u005A"}"#,
            r#"{"note":"window_start","window_start":"2026-10-01T00:00:00Z","window_end":"2026-10-15T00:00:00Z"}"#,
            r#"{"meta":{"window_start":"1999-01-01T00:00:00Z","list":[1,-2.5e+3,true,null,[],{}]},"window_start":"2026-10-01T00:00:00Z","window_end":"2026-10-15T00:00:00Z"}"#,
            r#"{"window_start":"2030-01-01T00:00:00Z","window_end":"2026-10-15T00:00:00Z","window_start":"2026-10-01T00:00:00Z"}"#,
            r#"{"window_start":"2026-10-01T00:00:00Z","say":"~"~/~~~b~f~n~r~t~ud83d~ude00~ud800","n":[0,-0,1.5,2E-3,NaN,Infinity,-Infinity],"window_end":"2026-10-15T00:00:00Z"}"#,
            " {\t\"window_start\" : \"2026-10-01T00:00:00Z\" ,\r\n \"window_end\":\"1792022400000\" } \n",
        ] {
            assert_eq!(window_of(text), Ok(plain), "{text}");
        }
    }

    #[test]
    fn manifest_errors_name_their_cause() {
        for (text, error) in [
            (r#"{"window_start":"2026-10-01T00:00:00Z"}"#, "no window_end"),
            (r#"{"window_start":1,"window_end":"2026-10-15T00:00:00Z"}"#, "window_start is not a string"),
            (r#"{"window_start":"2026-10-01T00:00:00Z","window_end":"2026-10-15T00:00:00Z","window_end":null}"#, "window_end is not a string"),
            (r#"{"window_start":"2026-10-01T00:00:00~u007a","window_end":"2026-10-15T00:00:00Z"}"#, "bad window_start 2026-10-01T00:00:00z"),
            (r#"{"window_start":"2026-10-15T00:00:00Z","window_end":"2026-10-01T00:00:00Z"}"#, "window_end is not after window_start"),
        ] {
            assert_eq!(window_of(text), Err(error.to_string()), "{text}");
        }
        for text in [
            "",
            "[]",
            r#"{window_start:"2026-10-01T00:00:00Z","window_end":"2026-10-15T00:00:00Z"}"#,
            r#"{"window_start":"2026-10-01T00:00:00Z","window_end":"2026-10-15T00:00:00Z",}"#,
            r#"{"window_start":"2026-10-01T00:00:00Z","window_end":"2026-10-15T00:00:00Z"} x"#,
            r#"{"window_start":"2026-10-01T00:00:00Z","window_end":"2026-10-15T00:00:00Z""#,
            r#"{"window_start":"2026-10-01T00:00:00~x5a","window_end":"2026-10-15T00:00:00Z"}"#,
            r#"{"window_start":"2026-10-01T00:00:00~u5a","window_end":"2026-10-15T00:00:00Z"}"#,
            "{\"window_start\":\"2026-10-01T00:00:00Z\t\",\"window_end\":\"2026-10-15T00:00:00Z\"}",
            r#"{"n":01,"window_start":"2026-10-01T00:00:00Z","window_end":"2026-10-15T00:00:00Z"}"#,
            r#"{"n":[1,],"window_start":"2026-10-01T00:00:00Z","window_end":"2026-10-15T00:00:00Z"}"#,
            r#"{"n":-,"window_start":"2026-10-01T00:00:00Z","window_end":"2026-10-15T00:00:00Z"}"#,
        ] {
            let error = window_of(text).unwrap_err();
            assert!(error.starts_with("not valid JSON at byte "), "{text}: {error}");
        }
    }

    #[test]
    fn campaign_and_category_indexes_past_65535_stay_distinct() {
        let dir = std::env::temp_dir().join(format!("lookups-wide-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut campaigns = String::from("campaign_id,channel\n");
        let mut products = String::from("product_id,category\n");
        for i in 1..=70_000 {
            campaigns.push_str(&format!("c_{i},web\n"));
            products.push_str(&format!("p{i},k{i}\n"));
        }
        std::fs::write(dir.join("campaigns.csv"), campaigns).unwrap();
        std::fs::write(dir.join("products.csv"), products).unwrap();
        let campaigns = read_campaigns(&dir).unwrap();
        let products = read_products(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(campaigns.ids.len(), 70_001);
        for i in [1u32, 65_535, 65_536, 65_537, 70_000] {
            let index = campaigns.map[&Key::from_bytes(format!("c_{i}").as_bytes())];
            assert_eq!(index, i);
            assert_eq!(campaigns.ids[index as usize], format!("c_{i}").into_bytes());
        }
        assert_eq!(products.categories.len(), 70_001);
        assert_eq!(products.unknown, 70_000);
        assert_eq!(products.categories[products.unknown as usize], b"unknown".to_vec());
        for i in [1, 65_536, 65_537, 70_000] {
            let index = products.map[&Key::from_bytes(format!("p{i}").as_bytes())];
            assert_eq!(products.categories[index as usize], format!("k{i}").into_bytes());
        }
        let mut ranks = products.rank.clone();
        ranks.sort_unstable();
        assert!(ranks.iter().copied().eq(0..70_001));
    }
}
