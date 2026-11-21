use crate::clicks::Click;
use crate::events::{Event, Sale};
use crate::ids::Key;
use crate::lookups::Products;
use crate::timestamp;

/// Times here are i32 milliseconds from the window start. `events::check_fits`
/// refuses a window whose span plus 7 days exceeds i32, so a click time plus
/// `WEEK`, or a session start minus a click time, stays within i32.
const GAP: i32 = (30 * timestamp::MINUTE) as i32;
const WEEK: i32 = timestamp::WEEK as i32;

/// Refuses more surviving events than the counts here hold. No session,
/// event or assist count can exceed the surviving events; user_daily writes
/// its counts as int32 and each thread counts per (campaign, day) in u32.
pub fn check_counts(events: usize) -> Result<(), String> {
    if events > i32::MAX as usize {
        return Err(format!("{events} surviving events; session and event counts are stored as int32, which holds at most {}", i32::MAX));
    }
    Ok(())
}

/// One thread's user_daily rows, column by column as the writer takes them.
#[derive(Default)]
pub struct UserRows {
    /// Each person with rows and how many rows it has, in row order: a
    /// person's rows are consecutive.
    pub persons: Vec<(Key, u32)>,
    /// Days since the epoch.
    pub days: Vec<i32>,
    pub sessions: Vec<i32>,
    pub events: Vec<i32>,
    pub revenue: Vec<i64>,
    /// 1 for a row with a top category, 0 for a row without one.
    pub has_top: Vec<i16>,
    /// The top category of each row that has one, in row order.
    pub tops: Vec<u16>,
}

impl UserRows {
    pub fn with_capacity(rows: usize) -> UserRows {
        UserRows {
            persons: Vec::with_capacity(rows / 2),
            days: Vec::with_capacity(rows),
            sessions: Vec::with_capacity(rows),
            events: Vec::with_capacity(rows),
            revenue: Vec::with_capacity(rows),
            has_top: Vec::with_capacity(rows),
            tops: Vec::with_capacity(rows / 2),
        }
    }

    pub fn len(&self) -> usize {
        self.days.len()
    }

    pub fn is_empty(&self) -> bool {
        self.days.is_empty()
    }

    pub fn clear(&mut self) {
        self.persons.clear();
        self.days.clear();
        self.sessions.clear();
        self.events.clear();
        self.revenue.clear();
        self.has_top.clear();
        self.tops.clear();
    }
}

/// One thread's totals per (campaign, day), each at `campaign * days + day`;
/// campaign 0 is `organic`. `check_counts` keeps every count within u32.
pub struct Counts {
    days: usize,
    sessions: Vec<u32>,
    purchasing: Vec<u32>,
    assisted: Vec<u32>,
    revenue: Vec<i64>,
}

impl Counts {
    pub fn new(campaigns: usize, days: usize) -> Counts {
        let cells = campaigns * days;
        Counts { days, sessions: vec![0; cells], purchasing: vec![0; cells], assisted: vec![0; cells], revenue: vec![0; cells] }
    }

    #[inline]
    fn cell(&self, campaign: u16, day: usize) -> usize {
        usize::from(campaign) * self.days + day
    }
}

#[derive(Clone, Copy, Default)]
pub struct Cell {
    pub sessions: i64,
    pub purchasing: i64,
    pub revenue: i64,
    pub assisted: i64,
}

/// Per (campaign, day) totals of every thread; campaign 0 is `organic`.
pub struct Accum {
    pub days: usize,
    pub cells: Vec<Cell>,
}

impl Accum {
    pub fn new(campaigns: usize, days: usize) -> Accum {
        Accum { days, cells: vec![Cell::default(); campaigns * days] }
    }

    pub fn merge(&mut self, counts: &Counts) {
        for (i, cell) in self.cells.iter_mut().enumerate() {
            cell.sessions += i64::from(counts.sessions[i]);
            cell.purchasing += i64::from(counts.purchasing[i]);
            cell.revenue += counts.revenue[i];
            cell.assisted += i64::from(counts.assisted[i]);
        }
    }
}

pub struct Rules<'a> {
    /// The window start in epoch milliseconds, the zero of every stored time.
    pub origin: i64,
    /// Days since the epoch of the window's first day.
    pub base_day: i32,
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
    events: u32,
    revenue: i64,
}

/// The events of one person inside one `GAP`-long stretch of time. Any two
/// events of a slot are less than `GAP` apart, so a slot never splits
/// across sessions, and a session break can only fall between occupied
/// slots: two that are not neighbours, or neighbours whose facing times are
/// more than `GAP` apart.
#[derive(Clone, Copy)]
struct Slot {
    first: u32,
    last: u32,
    events: u32,
}

impl Slot {
    const EMPTY: Slot = Slot { first: u32::MAX, last: 0, events: 0 };
}

#[derive(Default)]
struct DayAgg {
    sessions: i64,
    events: i64,
    revenue: i64,
    cats: Vec<(u16, i64)>,
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
    /// Every slot of the window, the time from the window start divided by
    /// `GAP`; all empty between persons.
    slots: Vec<Slot>,
    /// One bit per slot, set for the slots the current person occupies, so
    /// they are read back in time order without a sort.
    occupied: Vec<u64>,
    sessions: Vec<Session>,
    day_agg: Vec<DayAgg>,
    used_days: Vec<usize>,
    day_lo: Vec<usize>,
    /// Per campaign, the end of the sessions its clicks assist so far; 0
    /// for a campaign none of the person's clicks has assisted yet.
    covered: Vec<u32>,
    /// Per campaign, the set of a person's sessions its clicks assist, for
    /// a person with at most 64 sessions.
    assist_bits: Vec<u64>,
    /// For such a person, the set of its sessions on each of its days, and
    /// per session the place of its day among those.
    day_bits: Vec<u64>,
    day_rank: Vec<u8>,
    /// The campaigns whose `covered` or `assist_bits` is not 0.
    touched: Vec<u16>,
}

/// One bucket's events and clicks laid out person by person.
#[derive(Default)]
struct GroupScratch {
    persons: Persons,
    /// Person number of each event, in reading order.
    event_tag: Vec<u32>,
    /// Start of each person's event times, purchases and clicks, plus the total.
    event_off: Vec<u32>,
    purchase_off: Vec<u32>,
    click_off: Vec<u32>,
    fill: Vec<u32>,
    purchase_fill: Vec<u32>,
    /// The time of every event, never negative.
    times: Vec<u32>,
    /// Each purchase as `pack` gives it.
    purchases: Vec<u64>,
    /// Person number of each click, in reading order; `NO_GROUP` for the
    /// click of a person without events.
    click_tag: Vec<u32>,
    /// The clicks of persons with events, person by person, each person's
    /// in (ts, click_id) order.
    clicks: Vec<Click>,
}

const NO_GROUP: u32 = u32::MAX;

impl Scratch {
    pub fn new(campaigns: usize, days: usize) -> Scratch {
        // An event time is under `days` whole days from the window start,
        // since the window starts and ends inside those days.
        let slots = days * (timestamp::DAY as usize).div_ceil(GAP as usize);
        Scratch {
            sort_seconds: 0.0,
            person: PersonScratch {
                slots: vec![Slot::EMPTY; slots],
                occupied: vec![0; slots.div_ceil(64)],
                sessions: Vec::new(),
                day_agg: (0..days).map(|_| DayAgg::default()).collect(),
                used_days: Vec::new(),
                day_lo: vec![0; days + 1],
                covered: vec![0; campaigns],
                assist_bits: vec![0; campaigns],
                day_bits: Vec::new(),
                day_rank: Vec::new(),
                touched: Vec::new(),
            },
            group: GroupScratch::default(),
        }
    }
}

/// The persons of one bucket numbered in order of first appearance, in an
/// open-addressing table kept at most half full. A position holds its
/// person's number plus one, so 0 marks an empty position whatever the key.
#[derive(Default)]
struct Persons {
    keys: Vec<u64>,
    numbers: Vec<u32>,
    shift: u32,
    /// The person and the table position of each number.
    list: Vec<Key>,
    positions: Vec<u32>,
}

/// Multiplier of the table's hash. `Key::bucket` uses another, since every
/// key of a bucket shares the top bits of that one's product.
const PERSON_MIX: u64 = 0xFF51_AFD7_ED55_8CCD;
const MIN_POSITIONS: usize = 64;

impl Persons {
    /// Empties the table, keeping its size.
    fn clear(&mut self) {
        if self.numbers.is_empty() {
            self.resize(MIN_POSITIONS);
        }
        for &position in &self.positions {
            self.numbers[position as usize] = 0;
        }
        self.list.clear();
        self.positions.clear();
    }

    fn resize(&mut self, positions: usize) {
        self.keys = vec![0; positions];
        self.numbers = vec![0; positions];
        self.shift = 64 - positions.trailing_zeros();
    }

    /// Where the search for `key` starts.
    #[inline(always)]
    fn home(&self, key: Key) -> usize {
        (key.0.wrapping_mul(PERSON_MIX) >> self.shift) as usize
    }

    /// The number of `key`, giving it the next number when it is new.
    #[inline(always)]
    fn number(&mut self, key: Key) -> usize {
        let mask = self.numbers.len() - 1;
        let mut i = self.home(key);
        loop {
            let n = self.numbers[i];
            if n == 0 {
                return self.insert(i, key);
            }
            if self.keys[i] == key.0 {
                return n as usize - 1;
            }
            i = (i + 1) & mask;
        }
    }

    /// The number of `key`, or None when it is not in the table.
    #[inline(always)]
    fn find(&self, key: Key) -> Option<usize> {
        let mask = self.numbers.len() - 1;
        let mut i = self.home(key);
        loop {
            let n = self.numbers[i];
            if n == 0 {
                return None;
            }
            if self.keys[i] == key.0 {
                return Some(n as usize - 1);
            }
            i = (i + 1) & mask;
        }
    }

    /// Numbers the new person `key` at the empty `position`, doubling the
    /// table first when the person would fill more than half of it.
    fn insert(&mut self, mut position: usize, key: Key) -> usize {
        if 2 * (self.list.len() + 1) > self.numbers.len() {
            self.resize(2 * self.numbers.len());
            for n in 0..self.list.len() {
                let at = self.free_position(self.list[n]);
                self.keys[at] = self.list[n].0;
                self.numbers[at] = n as u32 + 1;
                self.positions[n] = at as u32;
            }
            position = self.free_position(key);
        }
        self.keys[position] = key.0;
        self.list.push(key);
        self.positions.push(position as u32);
        self.numbers[position] = self.list.len() as u32;
        self.list.len() - 1
    }

    fn free_position(&self, key: Key) -> usize {
        let mask = self.numbers.len() - 1;
        let mut i = self.home(key);
        while self.numbers[i] != 0 {
            i = (i + 1) & mask;
        }
        i
    }
}

/// A purchase as one word: its time, never negative, in the high 32 bits
/// and its sale in the low 32.
#[inline(always)]
fn pack(event: &Event) -> u64 {
    (u64::from(event.ts as u32) << 32) | u64::from(event.sale.to_bits())
}

#[inline(always)]
fn time(word: u64) -> i32 {
    (word >> 32) as i32
}

#[inline(always)]
fn sale(word: u64) -> Sale {
    Sale::from_bits(word as u32)
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
pub fn run_bucket(event_parts: &[Vec<Event>], click_parts: &[Vec<Click>], rules: &Rules, scratch: &mut Scratch, rows: &mut UserRows, counts: &mut Counts) {
    let started = std::time::Instant::now();
    let Scratch { sort_seconds, person, group: g } = scratch;
    g.persons.clear();
    g.event_tag.clear();
    g.event_off.clear();
    g.purchase_off.clear();
    for part in event_parts {
        for event in part {
            let tag = g.persons.number(event.person);
            if tag == g.event_off.len() {
                g.event_off.push(0);
                g.purchase_off.push(0);
            }
            g.event_off[tag] += 1;
            g.purchase_off[tag] += u32::from(event.sale.is_purchase());
            g.event_tag.push(tag as u32);
        }
    }
    let groups = g.event_off.len();
    if groups == 0 {
        return;
    }
    let total = offsets(&mut g.event_off);
    let bought = offsets(&mut g.purchase_off);
    if g.times.len() < total {
        g.times.resize(total, 0);
    }
    if g.purchases.len() < bought {
        g.purchases.resize(bought, 0);
    }
    g.fill.clear();
    g.fill.extend_from_slice(&g.event_off[..groups]);
    g.purchase_fill.clear();
    g.purchase_fill.extend_from_slice(&g.purchase_off[..groups]);
    let mut tags = g.event_tag.as_slice();
    for part in event_parts {
        let (part_tags, rest) = tags.split_at(part.len());
        tags = rest;
        for (event, &tag) in part.iter().zip(part_tags) {
            debug_assert!(event.ts >= 0);
            let at = &mut g.fill[tag as usize];
            g.times[*at as usize] = event.ts as u32;
            *at += 1;
            if event.sale.is_purchase() {
                let at = &mut g.purchase_fill[tag as usize];
                g.purchases[*at as usize] = pack(event);
                *at += 1;
            }
        }
    }

    // The clicks of persons with events, spread person by person. A person
    // with few clicks gets each one inserted in (ts, click_id) order as it
    // lands; one with many has its clicks sorted afterwards.
    g.click_tag.clear();
    g.click_off.clear();
    g.click_off.resize(groups, 0);
    let mut first_click = None;
    for part in click_parts {
        for click in part {
            let tag = match g.persons.find(click.person) {
                Some(tag) => {
                    g.click_off[tag] += 1;
                    first_click.get_or_insert(*click);
                    tag as u32
                }
                None => NO_GROUP,
            };
            g.click_tag.push(tag);
        }
    }
    let kept = offsets(&mut g.click_off);
    if let Some(first_click) = first_click {
        if g.clicks.len() < kept {
            g.clicks.resize(kept, first_click);
        }
        g.fill.clear();
        g.fill.extend_from_slice(&g.click_off[..groups]);
        let mut tags = g.click_tag.as_slice();
        for part in click_parts {
            let (part_tags, rest) = tags.split_at(part.len());
            tags = rest;
            for (click, &tag) in part.iter().zip(part_tags) {
                if tag == NO_GROUP {
                    continue;
                }
                let (first, end) = (g.click_off[tag as usize] as usize, g.click_off[tag as usize + 1] as usize);
                let fill = &mut g.fill[tag as usize];
                let mut at = *fill as usize;
                *fill += 1;
                if end - first <= INSERTED_CLICKS {
                    while at > first && click_order(&g.clicks[at - 1], click).is_gt() {
                        g.clicks[at] = g.clicks[at - 1];
                        at -= 1;
                    }
                }
                g.clicks[at] = *click;
            }
        }
    }
    *sort_seconds += started.elapsed().as_secs_f64();

    for t in 0..groups {
        let times = &g.times[g.event_off[t] as usize..g.event_off[t + 1] as usize];
        let purchases = &g.purchases[g.purchase_off[t] as usize..g.purchase_off[t + 1] as usize];
        let clicks = &mut g.clicks[g.click_off[t] as usize..g.click_off[t + 1] as usize];
        if clicks.len() > INSERTED_CLICKS {
            clicks.sort_unstable_by(click_order);
        }
        person_rows(g.persons.list[t], times, purchases, clicks, rules, person, rows, counts);
    }
}

/// A person's clicks are inserted in order while being spread when there
/// are this many or fewer, and sorted afterwards when there are more.
const INSERTED_CLICKS: usize = 32;

/// Clicks by time, then by click_id compared byte by byte.
#[inline(always)]
fn click_order(a: &Click, b: &Click) -> std::cmp::Ordering {
    a.ts.cmp(&b.ts).then_with(|| a.id.cmp_bytes(b.id))
}

/// A click's newly assisted sessions are counted session by session when
/// there are this many or fewer, else day by day.
const SHORT_RUN: usize = 16;

/// The slot holding the event time `t`.
#[inline(always)]
fn slot_of(t: u32) -> usize {
    (t / GAP as u32) as usize
}

/// Bits `lo` up to but not including `hi` of a word, for `lo < hi <= 64`.
#[inline(always)]
fn range_bits(lo: usize, hi: usize) -> u64 {
    (u64::MAX >> (64 - hi)) & (u64::MAX << lo)
}

/// One person's sessions, credits, assists and user_daily rows, from its
/// event times and its purchases, in any order, and its clicks in time
/// order with the smallest click_id first at each shared time.
#[allow(clippy::too_many_arguments)]
fn person_rows(person: Key, times: &[u32], purchases: &[u64], clicks: &[Click], rules: &Rules, scratch: &mut PersonScratch, rows: &mut UserRows, counts: &mut Counts) {
    // Sessions from slots: the first and last time and the event count of
    // each occupied slot, then the occupied slots in order, a new session
    // starting at each break. An empty slot is `Slot::EMPTY`, so filling
    // one needs no test.
    for &t in times {
        let s = slot_of(t);
        let slot = &mut scratch.slots[s];
        slot.first = slot.first.min(t);
        slot.last = slot.last.max(t);
        slot.events += 1;
        scratch.occupied[s / 64] |= 1 << (s % 64);
    }
    scratch.sessions.clear();
    let mut previous: Option<(usize, u32)> = None;
    for w in 0..scratch.occupied.len() {
        let mut bits = std::mem::take(&mut scratch.occupied[w]);
        while bits != 0 {
            let s = w * 64 + bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let slot = std::mem::replace(&mut scratch.slots[s], Slot::EMPTY);
            let joins = matches!(previous, Some((p, last)) if s == p + 1 && slot.first - last <= GAP as u32);
            if !joins {
                let start = slot.first as i32;
                scratch.sessions.push(Session { start, day: rules.day(start), events: 0, revenue: 0 });
            }
            scratch.sessions.last_mut().unwrap().events += slot.events;
            previous = Some((s, slot.last));
        }
    }
    // A purchase belongs to the last session starting at or before it.
    for &p in purchases {
        let sale = sale(p);
        let price = i64::from(sale.price());
        let at = scratch.sessions.partition_point(|s| s.start <= time(p)) - 1;
        let session = &mut scratch.sessions[at];
        session.revenue += price;
        let cats = &mut scratch.day_agg[session.day].cats;
        match cats.iter_mut().find(|(c, _)| *c == sale.cat()) {
            Some(entry) => entry.1 += price,
            None => cats.push((sale.cat(), price)),
        }
    }

    // Clicks before `seen` are at or before the current session start, which
    // only grows; `run` is the first of them at the latest of their times,
    // so the one with the smallest click_id at that time.
    let mut seen = 0;
    let mut run = 0;
    for session in &scratch.sessions {
        let (start, day) = (session.start, session.day);
        let agg = &mut scratch.day_agg[day];
        if agg.sessions == 0 {
            scratch.used_days.push(day);
        }
        agg.sessions += 1;
        agg.events += i64::from(session.events);
        agg.revenue += session.revenue;
        while seen < clicks.len() && clicks[seen].ts <= start {
            if seen == 0 || clicks[seen].ts != clicks[seen - 1].ts {
                run = seen;
            }
            seen += 1;
        }
        let campaign = if seen > 0 && start - clicks[seen - 1].ts <= WEEK { clicks[run].campaign } else { 0 };
        let cell = counts.cell(campaign, day);
        counts.sessions[cell] += 1;
        if session.revenue > 0 {
            counts.purchasing[cell] += 1;
            counts.revenue[cell] += session.revenue;
        }
    }

    // A click assists the sessions that start in [ts, ts + WEEK], a range
    // [lo, hi) of them that only moves forward as the clicks come in time
    // order. A campaign assists a session once.
    let sessions = &scratch.sessions;
    if !clicks.is_empty() && sessions.len() <= 64 {
        // Each campaign's assisted sessions as one bit set, the union of its
        // clicks' ranges, then counted day by day.
        let (mut lo, mut hi) = (0, 0);
        for click in clicks {
            let ts = click.ts;
            while lo < sessions.len() && sessions[lo].start < ts {
                lo += 1;
            }
            while hi < sessions.len() && sessions[hi].start - ts <= WEEK {
                hi += 1;
            }
            if lo < hi {
                let bits = &mut scratch.assist_bits[usize::from(click.campaign)];
                if *bits == 0 {
                    scratch.touched.push(click.campaign);
                }
                *bits |= range_bits(lo, hi);
            }
        }
        // The sessions of each of the person's days, in `used_days` order,
        // and the place in that order of each session's day.
        scratch.day_bits.clear();
        scratch.day_rank.clear();
        for (i, session) in sessions.iter().enumerate() {
            if i == 0 || session.day != sessions[i - 1].day {
                scratch.day_bits.push(0);
            }
            *scratch.day_bits.last_mut().unwrap() |= 1 << i;
            scratch.day_rank.push(scratch.day_bits.len() as u8 - 1);
        }
        // Each campaign's sessions, day by day over the days it assists.
        for &campaign in &scratch.touched {
            let mut bits = std::mem::take(&mut scratch.assist_bits[usize::from(campaign)]);
            while bits != 0 {
                let rank = usize::from(scratch.day_rank[bits.trailing_zeros() as usize]);
                let on_day = bits & scratch.day_bits[rank];
                let cell = counts.cell(campaign, scratch.used_days[rank]);
                counts.assisted[cell] += on_day.count_ones();
                bits &= !on_day;
            }
        }
        scratch.touched.clear();
    } else if !clicks.is_empty() {
        // Too many sessions for a bit set: `covered` holds the end of each
        // campaign's ranges so far, and only a range's part past it counts.
        let (mut lo, mut hi) = (0, 0);
        // Filled when a long range first needs it.
        let mut day_lo_filled = false;
        for click in clicks {
            let ts = click.ts;
            while lo < sessions.len() && sessions[lo].start < ts {
                lo += 1;
            }
            while hi < sessions.len() && sessions[hi].start - ts <= WEEK {
                hi += 1;
            }
            let campaign = click.campaign;
            let covered = &mut scratch.covered[usize::from(campaign)];
            let from = lo.max(*covered as usize);
            if from >= hi {
                continue;
            }
            if *covered == 0 {
                scratch.touched.push(campaign);
            }
            *covered = hi as u32;
            if hi - from <= SHORT_RUN {
                let mut d = sessions[from].day;
                let mut n = 0;
                for s in &sessions[from..hi] {
                    if s.day != d {
                        let cell = counts.cell(campaign, d);
                        counts.assisted[cell] += n;
                        d = s.day;
                        n = 0;
                    }
                    n += 1;
                }
                let cell = counts.cell(campaign, d);
                counts.assisted[cell] += n;
            } else {
                if !day_lo_filled {
                    // Sessions sort by start, so by day: day_lo[d] is the
                    // first session on day d or later.
                    let (first_day, last_day) = (sessions[0].day, sessions[sessions.len() - 1].day);
                    let mut i = 0;
                    for d in first_day..=last_day + 1 {
                        while i < sessions.len() && sessions[i].day < d {
                            i += 1;
                        }
                        scratch.day_lo[d] = i;
                    }
                    day_lo_filled = true;
                }
                for d in sessions[from].day..=sessions[hi - 1].day {
                    let n = hi.min(scratch.day_lo[d + 1]).saturating_sub(from.max(scratch.day_lo[d]));
                    if n > 0 {
                        let cell = counts.cell(campaign, d);
                        counts.assisted[cell] += n as u32;
                    }
                }
            }
        }
        for &campaign in &scratch.touched {
            scratch.covered[usize::from(campaign)] = 0;
        }
        scratch.touched.clear();
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
        // `check_counts` keeps both counts within i32.
        rows.days.push(rules.base_day + day as i32);
        rows.sessions.push(agg.sessions as i32);
        rows.events.push(agg.events as i32);
        rows.revenue.push(agg.revenue);
        match top {
            Some(cat) => {
                rows.has_top.push(1);
                rows.tops.push(cat);
            }
            None => rows.has_top.push(0),
        }
        agg.sessions = 0;
        agg.events = 0;
        agg.revenue = 0;
        agg.cats.clear();
    }
    rows.persons.push((person, scratch.used_days.len() as u32));
    scratch.used_days.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Sale;
    use crate::timestamp::{DAY, HOUR, MINUTE};
    use hashbrown::{HashMap, HashSet};

    type Row = (Vec<u8>, i32, i64, i64, i64, Option<u16>);

    fn user_rows(rows: &UserRows) -> Vec<Row> {
        let mut out = Vec::new();
        let mut at = 0;
        let mut top = 0;
        for &(person, count) in &rows.persons {
            for _ in 0..count {
                let cat = (rows.has_top[at] == 1).then(|| {
                    top += 1;
                    rows.tops[top - 1]
                });
                out.push((person.to_vec(), rows.days[at], i64::from(rows.sessions[at]), i64::from(rows.events[at]), rows.revenue[at], cat));
                at += 1;
            }
        }
        assert_eq!((at, top), (rows.len(), rows.tops.len()));
        out.sort();
        out
    }

    fn cells(counts: &Counts, campaigns: usize, days: usize) -> Vec<(i64, i64, i64, i64)> {
        let mut accum = Accum::new(campaigns, days);
        accum.merge(counts);
        accum.cells.iter().map(|c| (c.sessions, c.purchasing, c.revenue, c.assisted)).collect()
    }

    #[test]
    fn days_and_credit_count_from_a_window_that_starts_at_noon() {
        let origin = timestamp::parse(b"2026-10-01T12:00:00Z").unwrap();
        let base_day = timestamp::day_of(origin);
        let days = (timestamp::day_of(origin + 3 * DAY - 1) - base_day + 1) as usize;
        let products = Products { map: HashMap::new(), categories: vec![b"shoes".to_vec(), b"unknown".to_vec()], unknown: 1, rank: vec![0, 1] };
        let rules = Rules { origin, base_day, products: &products };
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
        let mut scratch = Scratch::new(2, days);
        let mut rows = UserRows::default();
        let mut counts = Counts::new(2, days);
        run_bucket(&[events], &[clicks], &rules, &mut scratch, &mut rows, &mut counts);
        let got: Vec<(i32, i64, i64, i64, Option<u16>)> = user_rows(&rows).into_iter().map(|r| (r.1 - base_day, r.2, r.3, r.4, r.5)).collect();
        assert_eq!(got, vec![(0, 1, 2, 700, Some(0)), (2, 1, 1, 0, None)]);
        let cells = cells(&counts, 2, days);
        assert_eq!(cells[days], (1, 1, 700, 1));
        assert_eq!(cells[2], (1, 0, 0, 0));
        assert_eq!(cells.iter().map(|c| c.0).sum::<i64>(), 2);
        assert_eq!(cells.iter().map(|c| c.3).sum::<i64>(), 1);
    }

    #[test]
    fn sessions_break_only_where_a_gap_passes_thirty_minutes_across_slots() {
        let origin = timestamp::parse(b"2026-10-01T00:00:00Z").unwrap();
        let base_day = timestamp::day_of(origin);
        let days = 2;
        let products = Products { map: HashMap::new(), categories: vec![b"unknown".to_vec()], unknown: 0, rank: vec![0] };
        let rules = Rules { origin, base_day, products: &products };
        let u1 = Key::from_bytes(b"u1");
        let at = |ts: i32| Event { person: u1, ts, sale: Sale::NONE };
        // Slots 0 and 1, exactly GAP apart: one session. Slot 3: a break, the
        // slots not being neighbours. Slot 4, a neighbour more than GAP after
        // slot 3's time: a break. Slot 5, exactly GAP after slot 4, twice: joins.
        let events = vec![at(6 * GAP - 1), at(GAP - 1), at(3 * GAP), at(2 * GAP - 1), at(5 * GAP - 1), at(6 * GAP - 1)];
        let mut scratch = Scratch::new(1, days);
        let mut rows = UserRows::default();
        let mut counts = Counts::new(1, days);
        run_bucket(&[events], &[], &rules, &mut scratch, &mut rows, &mut counts);
        assert_eq!(user_rows(&rows), vec![(b"u1".to_vec(), base_day, 3, 6, 0, None)]);
        assert_eq!(cells(&counts, 1, days)[0], (3, 0, 0, 0));
    }

    #[test]
    fn check_counts_refuses_more_events_than_int32_holds() {
        assert!(check_counts(i32::MAX as usize).is_ok());
        assert!(check_counts(i32::MAX as usize + 1).is_err());
    }

    #[test]
    fn credit_takes_the_smallest_click_id_at_the_latest_time_for_few_and_many_clicks() {
        let origin = timestamp::parse(b"2026-10-01T00:00:00Z").unwrap();
        let base_day = timestamp::day_of(origin);
        let days = 2;
        let products = Products { map: HashMap::new(), categories: vec![b"unknown".to_vec()], unknown: 0, rank: vec![0] };
        let rules = Rules { origin, base_day, products: &products };
        let start = (10 * HOUR) as i32;
        let (few, many) = (Key::from_bytes(b"u1"), Key::from_bytes(b"u2"));
        let events = vec![Event { person: few, ts: start, sale: Sale::NONE }, Event { person: many, ts: start, sale: Sale::NONE }];
        let click = |person: Key, id: &str, ts: i32, campaign: u16| Click { person, id: Key::from_bytes(id.as_bytes()), ts, campaign };
        // Long IDs compare through their bytes: "k-2" sorts before "k1" and "kz".
        let mut clicks = Vec::new();
        for person in [few, many] {
            clicks.extend([click(person, "kz", start - 10, 1), click(person, "k1", start - 10, 4), click(person, "k-2", start - 10, 3), click(person, "k0", start - 20, 2)]);
        }
        // Enough older clicks that the second person's are sorted, not inserted.
        clicks.extend((0..2 * INSERTED_CLICKS as i32).map(|i| click(many, &format!("m{i}"), start - 1000 - i, 2)));
        clicks.reverse();
        let mut scratch = Scratch::new(5, days);
        let mut rows = UserRows::default();
        let mut counts = Counts::new(5, days);
        run_bucket(&[events], &[clicks[..30].to_vec(), clicks[30..].to_vec()], &rules, &mut scratch, &mut rows, &mut counts);
        let cells = cells(&counts, 5, days);
        let credited: Vec<i64> = (0..5).map(|c| cells[c * days].0).collect();
        assert_eq!(credited, vec![0, 0, 0, 2, 0]);
        let assisted: Vec<i64> = (0..5).map(|c| cells[c * days].3).collect();
        assert_eq!(assisted, vec![0, 2, 2, 2, 2]);
    }

    #[test]
    fn persons_number_in_order_of_first_appearance_through_growth() {
        let mut persons = Persons::default();
        for round in 0..3 {
            persons.clear();
            let keys: Vec<Key> = (0..1000 + 300 * round).map(|i| Key::from_bytes(format!("p{}", i * 7 + round).as_bytes())).collect();
            for (n, &key) in keys.iter().enumerate() {
                assert_eq!(persons.number(key), n);
                assert_eq!(persons.number(key), n);
            }
            for (n, &key) in keys.iter().enumerate() {
                assert_eq!(persons.find(key), Some(n));
                assert_eq!(persons.list[n], key);
            }
            assert_eq!(persons.find(Key::from_bytes(b"absent")), None);
            assert_eq!(persons.find(Key(0)), None);
            assert_eq!(persons.number(Key(0)), keys.len());
            assert_eq!(persons.find(Key(0)), Some(keys.len()));
        }
    }

    /// Persons with every rule's edge: equal times, gaps of exactly 30
    /// minutes and one millisecond more, sessions crossing midnight, ties in
    /// the top category, clicks at the session start, exactly a week before
    /// it and a millisecond more, distinct clicks at one millisecond, copies
    /// of a click, long click IDs, clicks of persons without events, and one
    /// person with enough events for the radix sort. Every time stays inside
    /// a 14-day window, clicks reaching 7 days before it.
    fn dataset(seed: u64, base: i32) -> (Vec<Event>, Vec<Click>) {
        let mut x = seed;
        let mut next = move |n: u64| {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (x >> 33) % n
        };
        let end = (14 * DAY) as i32;
        let mut events = Vec::new();
        let mut clicks = Vec::new();
        let mut click_no = 0;
        for p in 0..60u64 {
            let person = Key::from_bytes(format!("u{p}").as_bytes());
            let whale = p == 0;
            let sessions = if whale { 90 } else { 1 + next(6) };
            let mut t = base + next(2 * DAY as u64) as i32;
            let mut starts = Vec::new();
            for _ in 0..sessions {
                starts.push(t);
                let n = if whale { 1 + next(20) } else { 1 + next(8) };
                for _ in 0..n {
                    let sale = if next(4) == 0 { Sale::new(1 + next(3) as u32 * 100, next(3) as u16) } else { Sale::NONE };
                    events.push(Event { person, ts: t, sale });
                    t += match next(8) {
                        0 => 0,
                        1 => GAP,
                        _ => next(if whale { 5 * MINUTE as u64 } else { GAP as u64 }) as i32,
                    };
                }
                t += GAP + 1 + if next(3) == 0 { 0 } else { next(if whale { HOUR as u64 / 2 } else { DAY as u64 / 2 }) as i32 };
            }
            for _ in 0..if whale { 40 + next(20) } else { next(12) } {
                let start = starts[next(starts.len() as u64) as usize];
                let ts = match next(6) {
                    0 => start,
                    1 => start - WEEK,
                    2 => start - WEEK - 1,
                    3 => start + 1,
                    _ => start - next(2 * WEEK as u64) as i32,
                };
                click_no += 1;
                let id = if next(4) == 0 { format!("k-{click_no}") } else { format!("k{click_no}") };
                let click = Click { person, id: Key::from_bytes(id.as_bytes()), ts, campaign: 1 + next(4) as u16 };
                clicks.push(click);
                if next(5) == 0 {
                    clicks.push(click);
                }
            }
        }
        for c in 0..5 {
            clicks.push(Click { person: Key::from_bytes(format!("v{c}").as_bytes()), id: Key::from_bytes(b"kv"), ts: base, campaign: 1 });
        }
        events.retain(|e| e.ts < end);
        clicks.retain(|c| c.ts >= -WEEK && c.ts < end);
        assert!(events.iter().filter(|e| e.person == Key::from_bytes(b"u0")).count() >= 512);
        assert!(clicks.iter().filter(|c| c.person == Key::from_bytes(b"u0")).count() > INSERTED_CLICKS);
        // Shuffle so persons interleave as dedup leaves them.
        for i in (1..events.len()).rev() {
            events.swap(i, next(i as u64 + 1) as usize);
        }
        for i in (1..clicks.len()).rev() {
            clicks.swap(i, next(i as u64 + 1) as usize);
        }
        (events, clicks)
    }

    /// The rules applied one session and one click at a time.
    fn reference(events: &[Event], clicks: &[Click], rules: &Rules, campaigns: usize, days: usize) -> (Vec<Row>, Vec<(i64, i64, i64, i64)>) {
        let mut cells = vec![(0i64, 0i64, 0i64, 0i64); campaigns * days];
        let mut by_person: HashMap<Key, Vec<i32>> = HashMap::new();
        let mut sales: HashMap<(Key, i32), Vec<Sale>> = HashMap::new();
        for e in events {
            by_person.entry(e.person).or_default().push(e.ts);
            sales.entry((e.person, e.ts)).or_default().push(e.sale);
        }
        let mut rows = Vec::new();
        for (&person, times) in &mut by_person {
            times.sort_unstable();
            times.dedup();
            let mine: Vec<&Click> = clicks.iter().filter(|c| c.person == person).collect();
            let mut per_day: HashMap<usize, (i64, i64, i64, HashMap<u16, i64>)> = HashMap::new();
            let mut i = 0;
            while i < times.len() {
                let start = times[i];
                let mut j = i + 1;
                while j < times.len() && times[j] - times[j - 1] <= GAP {
                    j += 1;
                }
                let day = rules.day(start);
                let (mut n, mut revenue, mut cats) = (0i64, 0i64, HashMap::new());
                for t in &times[i..j] {
                    for sale in &sales[&(person, *t)] {
                        n += 1;
                        if sale.is_purchase() {
                            revenue += i64::from(sale.price());
                            *cats.entry(sale.cat()).or_insert(0) += i64::from(sale.price());
                        }
                    }
                }
                let entry = per_day.entry(day).or_default();
                entry.0 += 1;
                entry.1 += n;
                entry.2 += revenue;
                for (cat, sum) in cats {
                    *entry.3.entry(cat).or_insert(0) += sum;
                }
                // The latest qualifying click, and of those the smallest click_id.
                let mut best: Option<&Click> = None;
                for &c in mine.iter().filter(|c| c.ts <= start && start - c.ts <= WEEK) {
                    if best.is_none_or(|b| c.ts > b.ts || (c.ts == b.ts && c.id.to_vec() < b.id.to_vec())) {
                        best = Some(c);
                    }
                }
                let credited = best.map_or(0, |c| c.campaign as usize);
                let cell = &mut cells[credited * days + day];
                cell.0 += 1;
                if revenue > 0 {
                    cell.1 += 1;
                    cell.2 += revenue;
                }
                let assisting: HashSet<u16> = mine.iter().filter(|c| c.ts <= start && start - c.ts <= WEEK).map(|c| c.campaign).collect();
                for campaign in assisting {
                    cells[campaign as usize * days + day].3 += 1;
                }
                i = j;
            }
            for (day, (sessions, n, revenue, cats)) in per_day {
                let mut cats: Vec<(u16, i64)> = cats.into_iter().collect();
                cats.sort_by_key(|&(cat, sum)| (std::cmp::Reverse(sum), rules.products.rank[cat as usize]));
                rows.push((person.to_vec(), rules.base_day + day as i32, sessions, n, revenue, cats.first().map(|c| c.0)));
            }
        }
        rows.sort();
        (rows, cells)
    }

    #[test]
    fn matches_the_rules_applied_one_session_at_a_time() {
        let origin = timestamp::parse(b"2026-10-01T00:00:00Z").unwrap();
        let base_day = timestamp::day_of(origin);
        let days = 14;
        let products = Products { map: HashMap::new(), categories: vec![b"b".to_vec(), b"a".to_vec(), b"unknown".to_vec()], unknown: 2, rank: vec![1, 0, 2] };
        let rules = Rules { origin, base_day, products: &products };
        let campaigns = 5;
        let mut ties = 0;
        for seed in 1..=12 {
            let (events, clicks) = dataset(seed, (seed as i32 % 3) * DAY as i32 + 7 * MINUTE as i32);
            ties += clicks.iter().enumerate().filter(|&(i, a)| clicks[..i].iter().any(|b| b.person == a.person && b.ts == a.ts && b.id != a.id)).count();
            let (want_rows, want_cells) = reference(&events, &clicks, &rules, campaigns, days);
            assert!(want_cells.iter().any(|c| c.3 > 0) && want_cells.iter().any(|c| c.1 > 0), "seed {seed}");
            // One person has too many sessions for the bit-set assists.
            assert!(want_rows.iter().filter(|r| r.0 == b"u0").map(|r| r.2).sum::<i64>() > 64, "seed {seed}");
            // The bucket as dedup and the click scan leave it: several parts each.
            let event_parts: Vec<Vec<Event>> = events.chunks(events.len() / 3 + 1).map(<[Event]>::to_vec).collect();
            let click_parts: Vec<Vec<Click>> = clicks.chunks(clicks.len() / 4 + 1).map(<[Click]>::to_vec).collect();
            let mut scratch = Scratch::new(campaigns, days);
            let mut rows = UserRows::default();
            let mut counts = Counts::new(campaigns, days);
            // Twice through one scratch, as a thread runs bucket after bucket.
            run_bucket(&event_parts, &click_parts, &rules, &mut scratch, &mut rows, &mut counts);
            assert_eq!(user_rows(&rows), want_rows, "seed {seed}");
            assert_eq!(cells(&counts, campaigns, days), want_cells, "seed {seed}");
            let mut again = UserRows::default();
            let mut counts_again = Counts::new(campaigns, days);
            run_bucket(&event_parts, &click_parts, &rules, &mut scratch, &mut again, &mut counts_again);
            assert_eq!(user_rows(&again), want_rows, "seed {seed}, second run");
            assert_eq!(cells(&counts_again, campaigns, days), want_cells, "seed {seed}, second run");
        }
        assert!(ties > 0, "no person has two click IDs at one time");
    }
}
