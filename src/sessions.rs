use crate::clicks::Click;
use crate::events::Event;
use crate::ids::Key;
use crate::lookups::Products;
use crate::timestamp::{self, MINUTE, WEEK};

const GAP: i64 = 30 * MINUTE;

pub struct UserRow {
    pub person: Key,
    /// Days since the epoch.
    pub day: i32,
    pub sessions: i64,
    pub events: i64,
    pub revenue: i64,
    pub top: Option<u16>,
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

    fn at(&mut self, campaign: u16, day: usize) -> &mut Cell {
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
    /// Days since the epoch of the window's first day.
    pub base_day: i32,
    /// Days the window touches.
    pub days: usize,
    pub products: &'a Products,
}

#[derive(Clone, Copy)]
struct Session {
    start: i64,
    day: usize,
}

#[derive(Default)]
struct DayAgg {
    sessions: i64,
    events: i64,
    revenue: i64,
    cats: Vec<(u16, i64)>,
}

/// Scratch space reused across persons.
pub struct Scratch {
    sessions: Vec<Session>,
    day_agg: Vec<DayAgg>,
    used_days: Vec<usize>,
    day_lo: Vec<usize>,
    by_campaign: Vec<(u16, i64)>,
}

impl Scratch {
    pub fn new(days: usize) -> Scratch {
        Scratch {
            sessions: Vec::new(),
            day_agg: (0..days).map(|_| DayAgg::default()).collect(),
            used_days: Vec::new(),
            day_lo: vec![0; days + 1],
            by_campaign: Vec::new(),
        }
    }
}

/// Sessionizes, credits, assists and aggregates one bucket of persons.
pub fn run_bucket(events: &mut [Event], clicks: &mut [Click], rules: &Rules, scratch: &mut Scratch, rows: &mut Vec<UserRow>, accum: &mut Accum) {
    events.sort_unstable_by(|a, b| (a.person, a.ts).cmp(&(b.person, b.ts)));
    clicks.sort_unstable_by(|a, b| a.person.cmp(&b.person).then(a.ts.cmp(&b.ts)).then_with(|| a.id.cmp_bytes(b.id)));
    let mut ci = 0;
    let mut i = 0;
    while i < events.len() {
        let person = events[i].person;
        let mut j = i;
        while j < events.len() && events[j].person == person {
            j += 1;
        }
        while ci < clicks.len() && clicks[ci].person < person {
            ci += 1;
        }
        let mut cj = ci;
        while cj < clicks.len() && clicks[cj].person == person {
            cj += 1;
        }
        person_rows(&events[i..j], &clicks[ci..cj], rules, scratch, rows, accum);
        ci = cj;
        i = j;
    }
}

fn credit(clicks: &[Click], start: i64) -> u16 {
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

fn person_rows(events: &[Event], clicks: &[Click], rules: &Rules, scratch: &mut Scratch, rows: &mut Vec<UserRow>, accum: &mut Accum) {
    let person = events[0].person;
    scratch.sessions.clear();
    let mut k = 0;
    while k < events.len() {
        let start = events[k].ts;
        let day = (timestamp::day_of(start) - rules.base_day) as usize;
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
            if event.purchase {
                let price = i64::from(event.price);
                revenue += price;
                match agg.cats.iter_mut().find(|(cat, _)| *cat == event.cat) {
                    Some(slot) => slot.1 += price,
                    None => agg.cats.push((event.cat, price)),
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
        let mut top: Option<u16> = None;
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
