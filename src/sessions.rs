use hashbrown::HashMap;

use crate::clicks::Click;
use crate::events::Event;
use crate::ids::Key;
use crate::lookups::Products;
use crate::timestamp;

const GAP: i32 = (30 * timestamp::MINUTE) as i32;
const WEEK: i32 = timestamp::WEEK as i32;

pub struct UserRow {
    pub person: Key,
    /// Days since the epoch.
    pub day: i32,
    pub sessions: i64,
    pub events: i64,
    pub revenue: i64,
    pub top: Option<u32>,
}

#[derive(Clone, Copy, Default)]
pub struct Cell {
    pub sessions: i64,
    pub purchasing: i64,
    pub revenue: i64,
    pub assisted: i64,
}

/// Per (campaign, day) totals; campaign 0 is `organic`.
pub struct Accum {
    pub days: usize,
    pub cells: Vec<Cell>,
}

impl Accum {
    pub fn new(campaigns: usize, days: usize) -> Accum {
        Accum { days, cells: vec![Cell::default(); campaigns * days] }
    }

    pub fn at(&mut self, campaign: u32, day: usize) -> &mut Cell {
        &mut self.cells[campaign as usize * self.days + day]
    }

    pub fn merge(&mut self, other: &Accum) {
        for (mine, theirs) in self.cells.iter_mut().zip(&other.cells) {
            mine.sessions += theirs.sessions;
            mine.purchasing += theirs.purchasing;
            mine.revenue += theirs.revenue;
            mine.assisted += theirs.assisted;
        }
    }
}

pub struct Rules<'a> {
    /// The window start in epoch milliseconds, the zero of every stored time.
    pub origin: i64,
    /// Days since the epoch of the window's first day.
    pub base_day: i32,
    /// Days the window touches.
    pub days: usize,
    pub products: &'a Products,
}

impl Rules<'_> {
    /// Position, counted from the window's first day, of the UTC day holding
    /// the stored time `ts`.
    #[inline]
    fn day(&self, ts: i32) -> usize {
        (timestamp::day_of(self.origin + i64::from(ts)) - self.base_day) as usize
    }
}

#[derive(Clone, Copy)]
struct Session {
    start: i32,
    day: usize,
}

#[derive(Default)]
struct DayAgg {
    sessions: i64,
    events: i64,
    revenue: i64,
    cats: Vec<(u32, i64)>,
}

/// Scratch space reused across buckets.
pub struct Scratch {
    /// Seconds this thread spent grouping events and clicks by person.
    pub sort_seconds: f64,
    person: PersonScratch,
    group: GroupScratch,
}

/// Scratch space reused across persons.
struct PersonScratch {
    sessions: Vec<Session>,
    day_agg: Vec<DayAgg>,
    used_days: Vec<usize>,
    day_lo: Vec<usize>,
    by_campaign: Vec<(u32, i32)>,
}

/// One bucket's events and clicks laid out person by person.
#[derive(Default)]
struct GroupScratch {
    /// Person to its group number, in order of first appearance.
    index: HashMap<Key, u32>,
    /// Group number of each event and click, in reading order; clicks of a
    /// person without events get `NO_GROUP`.
    event_tag: Vec<u32>,
    click_tag: Vec<u32>,
    /// Start of each group in `events` and `clicks`, plus the total.
    event_off: Vec<u32>,
    click_off: Vec<u32>,
    fill: Vec<u32>,
    events: Vec<Event>,
    clicks: Vec<Click>,
}

const NO_GROUP: u32 = u32::MAX;

impl Scratch {
    pub fn new(days: usize) -> Scratch {
        Scratch {
            sort_seconds: 0.0,
            person: PersonScratch {
                sessions: Vec::new(),
                day_agg: (0..days).map(|_| DayAgg::default()).collect(),
                used_days: Vec::new(),
                day_lo: vec![0; days + 1],
                by_campaign: Vec::new(),
            },
            group: GroupScratch::default(),
        }
    }
}

/// Turns per-group counts into start offsets followed by the total.
fn offsets(counts: &mut Vec<u32>) -> usize {
    let mut total = 0u32;
    for count in counts.iter_mut() {
        let n = *count;
        *count = total;
        total += n;
    }
    counts.push(total);
    total as usize
}

/// Sessionizes, credits, assists and aggregates one bucket of persons, given
/// as parts that together hold every event and click of those persons.
pub fn run_bucket(event_parts: &[Vec<Event>], click_parts: &[Vec<Click>], rules: &Rules, scratch: &mut Scratch, rows: &mut Vec<UserRow>, accum: &mut Accum) {
    let started = std::time::Instant::now();
    let Scratch { sort_seconds, person, group: g } = scratch;
    let Some(&first_event) = event_parts.iter().flatten().next() else {
        return;
    };
    g.index.clear();
    g.event_tag.clear();
    g.event_off.clear();
    for event in event_parts.iter().flatten() {
        let next = g.event_off.len() as u32;
        let tag = *g.index.entry(event.person).or_insert(next);
        if tag == next {
            g.event_off.push(0);
        }
        g.event_off[tag as usize] += 1;
        g.event_tag.push(tag);
    }
    let groups = g.event_off.len();
    let total = offsets(&mut g.event_off);
    if g.events.len() < total {
        g.events.resize(total, first_event);
    }
    g.fill.clear();
    g.fill.extend_from_slice(&g.event_off[..groups]);
    for (event, &tag) in event_parts.iter().flatten().zip(&g.event_tag) {
        let slot = &mut g.fill[tag as usize];
        g.events[*slot as usize] = *event;
        *slot += 1;
    }

    g.click_tag.clear();
    g.click_off.clear();
    g.click_off.resize(groups, 0);
    let mut first_click = None;
    for click in click_parts.iter().flatten() {
        let tag = g.index.get(&click.person).copied().unwrap_or(NO_GROUP);
        if tag != NO_GROUP {
            g.click_off[tag as usize] += 1;
            first_click.get_or_insert(*click);
        }
        g.click_tag.push(tag);
    }
    let kept = offsets(&mut g.click_off);
    if let Some(first_click) = first_click {
        if g.clicks.len() < kept {
            g.clicks.resize(kept, first_click);
        }
        g.fill.clear();
        g.fill.extend_from_slice(&g.click_off[..groups]);
        for (click, &tag) in click_parts.iter().flatten().zip(&g.click_tag) {
            if tag != NO_GROUP {
                let slot = &mut g.fill[tag as usize];
                g.clicks[*slot as usize] = *click;
                *slot += 1;
            }
        }
    }
    *sort_seconds += started.elapsed().as_secs_f64();

    for t in 0..groups {
        let events = &mut g.events[g.event_off[t] as usize..g.event_off[t + 1] as usize];
        events.sort_unstable_by_key(|e| e.ts);
        let clicks = &mut g.clicks[g.click_off[t] as usize..g.click_off[t + 1] as usize];
        clicks.sort_unstable_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.id.cmp_bytes(b.id)));
        person_rows(events, clicks, rules, person, rows, accum);
    }
}

fn credit(clicks: &[Click], start: i32) -> u32 {
    let i = clicks.partition_point(|c| c.ts <= start);
    if i == 0 {
        return 0;
    }
    let t = clicks[i - 1].ts;
    if start - t > WEEK {
        return 0;
    }
    let j = clicks.partition_point(|c| c.ts < t);
    clicks[j].campaign
}

fn person_rows(events: &[Event], clicks: &[Click], rules: &Rules, scratch: &mut PersonScratch, rows: &mut Vec<UserRow>, accum: &mut Accum) {
    let person = events[0].person;
    scratch.sessions.clear();
    let mut k = 0;
    while k < events.len() {
        let start = events[k].ts;
        let day = rules.day(start);
        let agg = &mut scratch.day_agg[day];
        if agg.sessions == 0 {
            scratch.used_days.push(day);
        }
        let mut prev = start;
        let mut m = k;
        let mut revenue = 0i64;
        while m < events.len() && events[m].ts - prev <= GAP {
            let event = &events[m];
            prev = event.ts;
            if event.sale.is_purchase() {
                let price = i64::from(event.sale.price());
                let cat = event.sale.cat();
                revenue += price;
                match agg.cats.iter_mut().find(|(c, _)| *c == cat) {
                    Some(slot) => slot.1 += price,
                    None => agg.cats.push((cat, price)),
                }
            }
            m += 1;
        }
        agg.sessions += 1;
        agg.events += (m - k) as i64;
        agg.revenue += revenue;
        let campaign = credit(clicks, start);
        let cell = accum.at(campaign, day);
        cell.sessions += 1;
        if revenue > 0 {
            cell.purchasing += 1;
            cell.revenue += revenue;
        }
        scratch.sessions.push(Session { start, day });
        k = m;
    }

    if !clicks.is_empty() {
        let sessions = &scratch.sessions;
        for d in 0..=rules.days {
            scratch.day_lo[d] = sessions.partition_point(|s| s.day < d);
        }
        scratch.by_campaign.clear();
        scratch.by_campaign.extend(clicks.iter().map(|c| (c.campaign, c.ts)));
        scratch.by_campaign.sort_unstable();
        let by_campaign = &scratch.by_campaign;
        let mut a = 0;
        while a < by_campaign.len() {
            let campaign = by_campaign[a].0;
            let mut lo_t = by_campaign[a].1;
            let mut hi_t = lo_t + WEEK;
            a += 1;
            loop {
                if a < by_campaign.len() && by_campaign[a].0 == campaign && by_campaign[a].1 <= hi_t {
                    hi_t = by_campaign[a].1 + WEEK;
                    a += 1;
                    continue;
                }
                let lo = sessions.partition_point(|s| s.start < lo_t);
                let hi = sessions.partition_point(|s| s.start <= hi_t);
                if lo < hi {
                    for d in sessions[lo].day..=sessions[hi - 1].day {
                        let n = hi.min(scratch.day_lo[d + 1]).saturating_sub(lo.max(scratch.day_lo[d]));
                        if n > 0 {
                            accum.at(campaign, d).assisted += n as i64;
                        }
                    }
                }
                if a < by_campaign.len() && by_campaign[a].0 == campaign {
                    lo_t = by_campaign[a].1;
                    hi_t = lo_t + WEEK;
                    a += 1;
                } else {
                    break;
                }
            }
        }
    }

    for &day in &scratch.used_days {
        let agg = &mut scratch.day_agg[day];
        let mut top: Option<u32> = None;
        let mut best = 0i64;
        for &(cat, sum) in &agg.cats {
            let better = match top {
                None => true,
                Some(current) => sum > best || (sum == best && rules.products.rank[cat as usize] < rules.products.rank[current as usize]),
            };
            if better {
                top = Some(cat);
                best = sum;
            }
        }
        rows.push(UserRow {
            person,
            day: rules.base_day + day as i32,
            sessions: agg.sessions,
            events: agg.events,
            revenue: agg.revenue,
            top,
        });
        agg.sessions = 0;
        agg.events = 0;
        agg.revenue = 0;
        agg.cats.clear();
    }
    scratch.used_days.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Sale;
    use crate::timestamp::{DAY, HOUR};

    #[test]
    fn days_and_credit_count_from_a_window_that_starts_at_noon() {
        let origin = timestamp::parse(b"2026-10-01T12:00:00Z").unwrap();
        let base_day = timestamp::day_of(origin);
        let days = (timestamp::day_of(origin + 3 * DAY - 1) - base_day + 1) as usize;
        let products = Products { map: HashMap::new(), categories: vec![b"shoes".to_vec(), b"unknown".to_vec()], unknown: 1, rank: vec![0, 1] };
        let rules = Rules { origin, base_day, days, products: &products };
        let u1 = Key::from_bytes(b"u1");
        let late_evening = (11 * HOUR + 59 * timestamp::MINUTE) as i32;
        let events = vec![
            // 23:59 on the first day and a purchase at 00:10 the next: one session of the first day.
            Event { person: u1, ts: late_evening, sale: Sale::NONE },
            Event { person: u1, ts: late_evening + 11 * timestamp::MINUTE as i32, sale: Sale::new(700, 0) },
            // Midnight starting the third day.
            Event { person: u1, ts: (36 * HOUR) as i32, sale: Sale::NONE },
        ];
        // Exactly 7 days before the first session, so before the window: it
        // credits and assists that session and is too old for the second.
        let clicks = vec![Click { person: u1, id: Key::from_bytes(b"k1"), ts: late_evening - WEEK, campaign: 1 }];
        assert!(clicks[0].ts < 0);
        let mut scratch = Scratch::new(days);
        let mut rows = Vec::new();
        let mut accum = Accum::new(2, days);
        run_bucket(&[events], &[clicks], &rules, &mut scratch, &mut rows, &mut accum);
        let mut got: Vec<(i32, i64, i64, i64, Option<u32>)> = rows.iter().map(|r| (r.day - base_day, r.sessions, r.events, r.revenue, r.top)).collect();
        got.sort();
        assert_eq!(got, vec![(0, 1, 2, 700, Some(0)), (2, 1, 1, 0, None)]);
        let cell = |campaign: usize, day: usize| {
            let c = accum.cells[campaign * days + day];
            (c.sessions, c.purchasing, c.revenue, c.assisted)
        };
        assert_eq!(cell(1, 0), (1, 1, 700, 1));
        assert_eq!(cell(0, 2), (1, 0, 0, 0));
        assert_eq!(accum.cells.iter().map(|c| c.sessions).sum::<i64>(), 2);
        assert_eq!(accum.cells.iter().map(|c| c.assisted).sum::<i64>(), 1);
    }

    #[test]
    fn campaigns_past_65535_keep_their_own_cells() {
        let origin = timestamp::parse(b"2026-10-01T00:00:00Z").unwrap();
        let base_day = timestamp::day_of(origin);
        let products = Products { map: HashMap::new(), categories: vec![b"unknown".to_vec()], unknown: 0, rank: vec![0] };
        let rules = Rules { origin, base_day, days: 1, products: &products };
        let u = Key::from_bytes(b"u");
        let events = vec![Event { person: u, ts: 0, sale: Sale::NONE }];
        let clicks = vec![Click { person: u, id: Key::from_bytes(b"k"), ts: 0, campaign: 65_536 }];
        let mut accum = Accum::new(65_537, 1);
        run_bucket(&[events], &[clicks], &rules, &mut Scratch::new(1), &mut Vec::new(), &mut accum);
        let cell = |campaign: usize| {
            let c = accum.cells[campaign];
            (c.sessions, c.purchasing, c.revenue, c.assisted)
        };
        assert_eq!(cell(65_536), (1, 0, 0, 1));
        assert_eq!(cell(0), (0, 0, 0, 0));
    }
}
