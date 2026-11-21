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
    pub map: HashMap<Key, u16>,
    pub categories: Vec<Vec<u8>>,
    pub unknown: u16,
    pub rank: Vec<u16>,
}

pub struct Campaigns {
    pub map: HashMap<Key, u16>,
    pub ids: Vec<Vec<u8>>,
    pub channels: Vec<Vec<u8>>,
}

pub fn read_manifest(dir: &Path) -> Result<Window, String> {
    let path = dir.join("manifest.json");
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let start = json_string(&text, "window_start").ok_or_else(|| format!("{}: no window_start", path.display()))?;
    let end = json_string(&text, "window_end").ok_or_else(|| format!("{}: no window_end", path.display()))?;
    let start = timestamp::parse(start.as_bytes()).ok_or_else(|| format!("{}: bad window_start {start}", path.display()))?;
    let end = timestamp::parse(end.as_bytes()).ok_or_else(|| format!("{}: bad window_end {end}", path.display()))?;
    if end <= start {
        return Err(format!("{}: window_end is not after window_start", path.display()));
    }
    Ok(Window { start, end })
}

fn json_string(text: &str, key: &str) -> Option<String> {
    let quoted = format!("\"{key}\"");
    let at = text.find(&quoted)? + quoted.len();
    let rest = text[at..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
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
    let mut by_name: HashMap<Vec<u8>, u16> = HashMap::new();
    for (product, category) in rows(&dir.join("products.csv"))? {
        let index = *by_name.entry(category.clone()).or_insert_with(|| {
            categories.push(category.clone());
            (categories.len() - 1) as u16
        });
        map.insert(Key::from_bytes(&product), index);
    }
    let unknown = *by_name.entry(b"unknown".to_vec()).or_insert_with(|| {
        categories.push(b"unknown".to_vec());
        (categories.len() - 1) as u16
    });
    let mut order: Vec<usize> = (0..categories.len()).collect();
    order.sort_by(|&a, &b| categories[a].cmp(&categories[b]));
    let mut rank = vec![0u16; categories.len()];
    for (position, &index) in order.iter().enumerate() {
        rank[index] = position as u16;
    }
    Ok(Products { map, categories, unknown, rank })
}

pub fn read_campaigns(dir: &Path) -> Result<Campaigns, String> {
    let mut map = HashMap::new();
    let mut ids = vec![b"organic".to_vec()];
    let mut channels = vec![b"organic".to_vec()];
    for (campaign, channel) in rows(&dir.join("campaigns.csv"))? {
        let index = ids.len() as u16;
        if map.insert(Key::from_bytes(&campaign), index).is_none() {
            ids.push(campaign);
            channels.push(channel);
        }
    }
    Ok(Campaigns { map, ids, channels })
}
