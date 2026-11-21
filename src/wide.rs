use std::path::PathBuf;

use crate::events::CHUNK;
use crate::ids::Key;
use crate::lookups::{Campaigns, Products, Window};
use crate::people::People;
use crate::sessions::{Accum, UserRow};
use crate::timestamp::{self, DAY, MINUTE, WEEK};

const GAP: i64 = 30 * MINUTE;

#[derive(Clone, Copy)]
struct Event {
    person: Key,
    ts: i64,
    price: u32,
    cat: u32,
}

#[derive(Clone, Copy)]
struct EventCopy {
    event: Key,
    ingest: i64,
    surviving: Option<Event>,
}

#[derive(Clone, Copy)]
struct Click {
    person: Key,
    ts: i64,
    id: Key,
    campaign: u32,
}

pub struct Tables {
    pub rows: Vec<UserRow>,
    pub accum: Accum,
    pub base_day: i32,
}

pub fn run(event_files: &[PathBuf], click_files: &[PathBuf], threads: usize, window: Window, products: &Products, campaigns: &Campaigns, people: &People) -> Result<Tables, String> {
    let first = i32::try_from(window.start.div_euclid(DAY));
    let last = i32::try_from((window.end - 1).div_euclid(DAY));
    let (Ok(base_day), Ok(last)) = (first, last) else {
        return Err(format!("manifest.json: the window from {} to {} ms reaches days that a 32-bit date cannot hold", window.start, window.end));
    };
    let days = (i64::from(last) - i64::from(base_day) + 1) as usize;
    let events = surviving_events(event_files, threads, window, products, people)?;
    let clicks = valid_clicks(click_files, threads, window, campaigns, people)?;
    let mut tables = Tables { rows: Vec::new(), accum: Accum::new(campaigns.ids.len(), days), base_day };
    let mut e = 0;
    let mut c = 0;
    while e < events.len() {
        let person = events[e].person;
        let events_end = e + events[e..].partition_point(|event| event.person == person);
        while c < clicks.len() && clicks[c].person < person {
            c += 1;
        }
        let clicks_end = c + clicks[c..].partition_point(|click| click.person == person);
        person_rows(person, &events[e..events_end], &clicks[c..clicks_end], products, &mut tables);
        e = events_end;
        c = clicks_end;
    }
    Ok(tables)
}

fn surviving_events(paths: &[PathBuf], threads: usize, window: Window, products: &Products, people: &People) -> Result<Vec<Event>, String> {
    let parsed = crate::scan::run_fields::<7, _, _, _>(paths, threads, CHUNK, Vec::<EventCopy>::new, |copies, fields| {
        if !fields[0].is_empty() {
            let ingest = timestamp::parse(fields[3]).unwrap_or(i64::MAX);
            copies.push(EventCopy { event: Key::from_bytes(fields[0]), ingest, surviving: surviving(fields, ingest, window, products) });
        }
    })
    .map_err(|e| format!("events: {e}"))?;
    let mut copies: Vec<EventCopy> = Vec::with_capacity(parsed.iter().map(Vec::len).sum());
    for part in parsed {
        copies.extend(part);
    }
    copies.sort_unstable_by_key(|copy| (copy.event, copy.ingest));
    copies.dedup_by_key(|copy| copy.event);
    let mut events: Vec<Event> = copies.iter().filter_map(|copy| copy.surviving).map(|event| Event { person: people.name_of(event.person), ..event }).collect();
    drop(copies);
    events.sort_unstable_by_key(|event| (event.person, event.ts));
    Ok(events)
}

fn surviving(fields: &[&[u8]; 7], ingest: i64, window: Window, products: &Products) -> Option<Event> {
    let [_, user, ts, _, kind, product, price] = *fields;
    let purchase = kind == b"purchase";
    if user.is_empty() || product.is_empty() || !(purchase || kind == b"view" || kind == b"cart") {
        return None;
    }
    let ts = timestamp::parse(ts).filter(|ts| (window.start..window.end).contains(ts))?;
    if ingest.saturating_sub(ts) > DAY {
        return None;
    }
    let (price, cat) = if purchase {
        (parse_price(price)?, products.map.get(&Key::from_bytes(product)).copied().unwrap_or(products.unknown))
    } else {
        (0, 0)
    };
    Some(Event { person: Key::from_bytes(user), ts, price, cat })
}

fn parse_price(text: &[u8]) -> Option<u32> {
    if text.is_empty() || !text.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let digits = &text[text.iter().take_while(|&&c| c == b'0').count()..];
    if digits.len() > 8 {
        return None;
    }
    let value = digits.iter().fold(0, |value, &c| value * 10 + u32::from(c - b'0'));
    (1..=10_000_000).contains(&value).then_some(value)
}

fn valid_clicks(paths: &[PathBuf], threads: usize, window: Window, campaigns: &Campaigns, people: &People) -> Result<Vec<Click>, String> {
    let parsed = crate::scan::run_fields::<4, _, _, _>(paths, threads, CHUNK, Vec::<Click>::new, |clicks, fields| {
        let [id, user, campaign_id, ts] = *fields;
        if id.is_empty() || user.is_empty() {
            return;
        }
        let ts = timestamp::parse(ts).filter(|ts| (window.start - WEEK..window.end).contains(ts));
        if let (Some(ts), Some(&campaign)) = (ts, campaigns.map.get(&Key::from_bytes(campaign_id))) {
            clicks.push(Click { person: Key::from_bytes(user), ts, id: Key::from_bytes(id), campaign });
        }
    })
    .map_err(|e| format!("clicks: {e}"))?;
    let mut clicks: Vec<Click> = parsed.into_iter().flatten().map(|click| Click { person: people.name_of(click.person), ..click }).collect();
    clicks.sort_unstable_by(|a, b| a.person.cmp(&b.person).then(a.ts.cmp(&b.ts)).then_with(|| a.id.cmp_bytes(b.id)));
    Ok(clicks)
}

fn credit(clicks: &[Click], start: i64) -> u32 {
    let i = clicks.partition_point(|click| click.ts <= start);
    if i == 0 || start - clicks[i - 1].ts > WEEK {
        return 0;
    }
    let latest = clicks[i - 1].ts;
    clicks[clicks.partition_point(|click| click.ts < latest)].campaign
}

fn person_rows(person: Key, events: &[Event], clicks: &[Click], products: &Products, tables: &mut Tables) {
    let mut row: Option<UserRow> = None;
    let mut cats: Vec<(u32, i64)> = Vec::new();
    let mut k = 0;
    while k < events.len() {
        let start = events[k].ts;
        let mut m = k + 1;
        while m < events.len() && events[m].ts - events[m - 1].ts <= GAP {
            m += 1;
        }
        let day = timestamp::day_of(start);
        if row.as_ref().is_some_and(|row| row.day != day) {
            finish(row.take(), &mut cats, products, &mut tables.rows);
        }
        let current = row.get_or_insert(UserRow { person, day, sessions: 0, events: 0, revenue: 0, top: None });
        let mut revenue = 0;
        for event in events[k..m].iter().filter(|event| event.price > 0) {
            let price = i64::from(event.price);
            revenue += price;
            match cats.iter_mut().find(|(cat, _)| *cat == event.cat) {
                Some(slot) => slot.1 += price,
                None => cats.push((event.cat, price)),
            }
        }
        current.sessions += 1;
        current.events += (m - k) as i64;
        current.revenue += revenue;
        let index = (i64::from(day) - i64::from(tables.base_day)) as usize;
        let cell = tables.accum.at(credit(clicks, start), index);
        cell.sessions += 1;
        if revenue > 0 {
            cell.purchasing += 1;
            cell.revenue += revenue;
        }
        let lo = clicks.partition_point(|click| click.ts < start - WEEK);
        let hi = clicks.partition_point(|click| click.ts <= start);
        let mut assisting: Vec<u32> = clicks[lo..hi].iter().map(|click| click.campaign).collect();
        assisting.sort_unstable();
        assisting.dedup();
        for campaign in assisting {
            tables.accum.at(campaign, index).assisted += 1;
        }
        k = m;
    }
    finish(row, &mut cats, products, &mut tables.rows);
}

fn finish(row: Option<UserRow>, cats: &mut Vec<(u32, i64)>, products: &Products, rows: &mut Vec<UserRow>) {
    if let Some(mut row) = row {
        let mut top: Option<(u32, i64)> = None;
        for &(cat, sum) in cats.iter() {
            if top.map_or(true, |(best, most)| sum > most || (sum == most && products.rank[cat as usize] < products.rank[best as usize])) {
                top = Some((cat, sum));
            }
        }
        row.top = top.map(|(cat, _)| cat);
        rows.push(row);
    }
    cats.clear();
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::timestamp::HOUR;
    use crate::{clicks, events, links, lookups, scan, sessions};

    type UserLine = (Vec<u8>, i32, i64, i64, i64, Option<Vec<u8>>);
    type CampaignLine = (Vec<u8>, Vec<u8>, i32, i64, i64, i64, i64);
    type Lines = (Vec<UserLine>, Vec<CampaignLine>);

    struct Data {
        window: Window,
        products: Products,
        campaigns: Campaigns,
        people: People,
        event_files: Vec<PathBuf>,
        click_files: Vec<PathBuf>,
    }

    fn data(dir: &Path) -> Data {
        let pairs = links::collect(&scan::csv_files(&dir.join("links")).unwrap(), 2).unwrap();
        Data {
            window: lookups::read_manifest(dir).unwrap(),
            products: lookups::read_products(dir).unwrap(),
            campaigns: lookups::read_campaigns(dir).unwrap(),
            people: People::build(pairs.into_iter().flatten(), 0),
            event_files: scan::csv_files(&dir.join("events")).unwrap(),
            click_files: scan::csv_files(&dir.join("clicks")).unwrap(),
        }
    }

    fn lines(rows: &[UserRow], accum: &Accum, base_day: i32, data: &Data) -> Lines {
        let mut users: Vec<UserLine> = rows.iter().map(|r| (r.person.to_vec(), r.day, r.sessions, r.events, r.revenue, r.top.map(|c| data.products.categories[c as usize].clone()))).collect();
        users.sort();
        let mut cells: Vec<CampaignLine> = Vec::new();
        for (campaign, id) in data.campaigns.ids.iter().enumerate() {
            for day in 0..accum.days {
                let cell = accum.cells[campaign * accum.days + day];
                if cell.sessions != 0 || cell.assisted != 0 {
                    cells.push((id.clone(), data.campaigns.channels[campaign].clone(), base_day + day as i32, cell.sessions, cell.purchasing, cell.revenue, cell.assisted));
                }
            }
        }
        cells.sort();
        (users, cells)
    }

    fn wide(data: &Data) -> Lines {
        let tables = run(&data.event_files, &data.click_files, 3, data.window, &data.products, &data.campaigns, &data.people).unwrap();
        lines(&tables.rows, &tables.accum, tables.base_day, data)
    }

    fn packed(data: &Data) -> Lines {
        assert!(events::fits(data.window, &data.products));
        let event_ctx = events::Context { window: data.window, products: &data.products };
        let event_buckets = events::dedup(events::parse(&data.event_files, 3, &event_ctx).unwrap(), 2, &data.people);
        let click_ctx = clicks::Context { window: data.window, campaigns: &data.campaigns, people: &data.people };
        let click_buckets = clicks::collect(&data.click_files, 3, &click_ctx).unwrap();
        let base_day = timestamp::day_of(data.window.start);
        let days = (timestamp::day_of(data.window.end - 1) - base_day + 1) as usize;
        let rules = sessions::Rules { origin: data.window.start, base_day, days, products: &data.products };
        let mut scratch = sessions::Scratch::new(days);
        let mut rows = Vec::new();
        let mut accum = Accum::new(data.campaigns.ids.len(), days);
        let none = Vec::new();
        for (b, event_parts) in event_buckets.iter().enumerate() {
            sessions::run_bucket(event_parts, click_buckets.get(b).unwrap_or(&none), &rules, &mut scratch, &mut rows, &mut accum);
        }
        lines(&rows, &accum, base_day, data)
    }

    fn dataset(name: &str, manifest: &str, files: &[(&str, &str, Vec<String>)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wide-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for folder in ["events", "clicks", "links"] {
            std::fs::create_dir_all(dir.join(folder)).unwrap();
        }
        std::fs::write(dir.join("manifest.json"), manifest).unwrap();
        for (path, header, rows) in files {
            let text: String = std::iter::once(*header).chain(rows.iter().map(String::as_str)).map(|line| format!("{line}\n")).collect();
            std::fs::write(dir.join(path), text).unwrap();
        }
        dir
    }

    fn day(text: &str) -> i32 {
        timestamp::day_of(timestamp::parse(format!("{text}T00:00:00Z").as_bytes()).unwrap())
    }

    fn strings(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|line| line.to_string()).collect()
    }

    #[test]
    fn an_eighteen_day_window_keeps_its_last_days() {
        let dir = dataset(
            "long",
            "{\"window_start\": \"2026-10-01T00:00:00Z\", \"window_end\": \"2026-10-19T00:00:00Z\"}",
            &[
                ("products.csv", "product_id,category", strings(&["p,shoes"])),
                ("campaigns.csv", "campaign_id,channel", strings(&["c_1,search"])),
                ("links/a.csv", "user_id_a,user_id_b", Vec::new()),
                (
                    "events/a.csv",
                    "event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents",
                    strings(&["e1,u,2026-10-01T00:00:00Z,2026-10-01T00:00:00Z,view,p,", "e2,u,2026-10-18T23:59:59Z,2026-10-19T00:00:05Z,purchase,p,500", "e3,u,2026-10-19T00:00:00Z,2026-10-19T00:00:00Z,view,p,"]),
                ),
                ("clicks/a.csv", "click_id,user_id,campaign_id,ts", strings(&["k0,u,c_1,2026-09-24T00:00:00Z", "k1,u,c_1,2026-10-18T23:00:00Z", "k2,u,c_1,2026-09-23T23:59:59Z"])),
            ],
        );
        let data = data(&dir);
        assert!(!events::fits(data.window, &data.products));
        let (users, cells) = wide(&data);
        std::fs::remove_dir_all(&dir).unwrap();
        let shoes = Some(b"shoes".to_vec());
        assert_eq!(users, vec![(b"u".to_vec(), day("2026-10-01"), 1, 1, 0, None), (b"u".to_vec(), day("2026-10-18"), 1, 1, 500, shoes)]);
        assert_eq!(
            cells,
            vec![(b"c_1".to_vec(), b"search".to_vec(), day("2026-10-01"), 1, 0, 0, 1), (b"c_1".to_vec(), b"search".to_vec(), day("2026-10-18"), 1, 1, 500, 1)]
        );
    }

    #[test]
    fn more_than_256_categories_keep_their_names() {
        let products: Vec<String> = (0..256).map(|i| format!("p{i},k{i}")).collect();
        let dir = dataset(
            "categories",
            "{\"window_start\": \"2026-10-01T00:00:00Z\", \"window_end\": \"2026-10-15T00:00:00Z\"}",
            &[
                ("products.csv", "product_id,category", products),
                ("campaigns.csv", "campaign_id,channel", Vec::new()),
                ("links/a.csv", "user_id_a,user_id_b", Vec::new()),
                (
                    "events/a.csv",
                    "event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents",
                    strings(&[
                        "e1,u,2026-10-02T10:00:00Z,2026-10-02T10:00:00Z,purchase,p255,300",
                        "e2,u,2026-10-02T10:01:00Z,2026-10-02T10:01:00Z,purchase,zz,200",
                        "e3,u,2026-10-02T10:02:00Z,2026-10-02T10:02:00Z,purchase,q,200",
                        "e4,u,2026-10-03T10:00:00Z,2026-10-03T10:00:00Z,purchase,p70,100",
                        "e5,u,2026-10-03T10:01:00Z,2026-10-03T10:01:00Z,purchase,p7,100",
                        "e6,v,2026-10-03T10:00:00Z,2026-10-03T10:00:00Z,purchase,p0,50",
                    ]),
                ),
                ("clicks/a.csv", "click_id,user_id,campaign_id,ts", Vec::new()),
            ],
        );
        let data = data(&dir);
        assert_eq!(data.products.categories.len(), 257);
        assert!(!events::fits(data.window, &data.products));
        let (users, cells) = wide(&data);
        std::fs::remove_dir_all(&dir).unwrap();
        let name = |text: &str| Some(text.as_bytes().to_vec());
        assert_eq!(
            users,
            vec![
                (b"u".to_vec(), day("2026-10-02"), 1, 3, 700, name("unknown")),
                (b"u".to_vec(), day("2026-10-03"), 1, 2, 200, name("k7")),
                (b"v".to_vec(), day("2026-10-03"), 1, 1, 50, name("k0")),
            ]
        );
        assert_eq!(
            cells,
            vec![(b"organic".to_vec(), b"organic".to_vec(), day("2026-10-02"), 1, 1, 700, 0), (b"organic".to_vec(), b"organic".to_vec(), day("2026-10-03"), 2, 2, 250, 0)]
        );
    }

    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 33) % n
        }

        fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
            items[self.below(items.len() as u64) as usize]
        }
    }

    fn random_dataset(seed: u64) -> PathBuf {
        let mut rng = Rng(seed);
        let start = timestamp::parse(b"2026-10-01T00:00:00Z").unwrap();
        let mut events = Vec::new();
        for n in 0..3000 {
            let ts = start - HOUR + rng.below((7 * DAY + 2 * HOUR) as u64) as i64;
            let first_ingest = ts + [0, 5_000, DAY, DAY + 1, 2 * DAY, -10_000][rng.below(6) as usize];
            let mut copies = Vec::new();
            for c in 0..1 + rng.below(3) as i64 {
                let user = if rng.below(25) == 0 { String::new() } else { format!("u{}", rng.below(40)) };
                let ts = if c > 0 && rng.below(3) == 0 { ts + rng.below(2 * HOUR as u64) as i64 } else { ts };
                let kind = rng.pick(&["view", "view", "cart", "purchase", "purchase", "Purchase"]);
                let product = rng.pick(&["p0", "p1", "p2", "p3", "p4", "p5", "p6", "p7", "px", ""]);
                let price = rng.pick(&["100", "0250", "999", "10000000", "0", "x", "10000001", ""]);
                copies.push(format!("e{n},{user},{ts},{},{kind},{product},{price}", first_ingest + 1000 * c));
            }
            if rng.below(2) == 0 {
                copies.reverse();
            }
            events.extend(copies);
        }
        let (first, second) = events.split_at(events.len() / 2);
        let mut clicks = Vec::new();
        for n in 0..800 {
            let ts = start - 8 * DAY + rng.below((16 * DAY) as u64) as i64;
            let user = format!("u{}", rng.below(40));
            let campaign = format!("c_{}", 1 + rng.below(6));
            clicks.push(format!("k{n},{user},{campaign},{ts}"));
            match rng.below(8) {
                0 => clicks.push(format!("k{n},{user},{campaign},{ts}")),
                1 => clicks.push(format!("j{n},{user},c_{},{ts}", 1 + rng.below(5))),
                _ => {}
            }
        }
        dataset(
            &format!("random{seed}"),
            "{\"window_start\": \"2026-10-01T00:00:00Z\", \"window_end\": \"2026-10-08T00:00:00Z\"}",
            &[
                ("products.csv", "product_id,category", strings(&["p0,k0", "p1,k1", "p2,k2", "p3,k3", "p4,k0", "p5,k1", "p6,k2"])),
                ("campaigns.csv", "campaign_id,channel", strings(&["c_1,search", "c_2,social", "c_3,search", "c_4,email", "c_5,social"])),
                ("links/a.csv", "user_id_a,user_id_b", strings(&["u1,u2", "u3,u2", "u10,U10", "u5,u5", ",u7", "u20,u21", "u21,u22"])),
                ("events/a.csv", "event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents", first.to_vec()),
                ("events/b.csv", "event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents", second.to_vec()),
                ("clicks/a.csv", "click_id,user_id,campaign_id,ts", clicks),
            ],
        )
    }

    #[test]
    fn agrees_with_the_packed_records_on_dirty_data() {
        for seed in 1..=4 {
            let dir = random_dataset(seed);
            let data = data(&dir);
            let (packed_users, packed_cells) = packed(&data);
            let (wide_users, wide_cells) = wide(&data);
            std::fs::remove_dir_all(&dir).unwrap();
            assert!(packed_users.len() > 100 && packed_cells.len() > 20, "seed {seed}: {} user rows, {} campaign rows", packed_users.len(), packed_cells.len());
            assert!(packed_users.iter().any(|row| row.5.is_some()) && packed_cells.iter().any(|row| row.6 > 0), "seed {seed}");
            assert_eq!(wide_users, packed_users, "seed {seed}");
            assert_eq!(wide_cells, packed_cells, "seed {seed}");
        }
    }
}
