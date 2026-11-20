# Shopping Sessions

The language of the Shopping Sessions solver: messy click-stream CSV files in, two exact daily tables out. The rules that define every term are in the track's rules.md; this file names the concepts so code, tests and notes use one word each.

## Language

### Input

**Line**:
One `\n`-terminated record of a CSV file (or the last record without `\n`). Either well-formed, with the exact field count and quotes only around whole fields, or malformed and dropped.
_Avoid_: row (ambiguous with output rows), record

**Field**:
One comma-separated value of a line, with its optional surrounding quotes removed. Spaces and `\r` are part of the value. An empty field is null.
_Avoid_: column (reserved for output tables), cell

**Event**:
A line of `events/*.csv`: a view, cart or purchase by a user ID at a timestamp, ingested at an ingest time.
_Avoid_: hit, action, interaction

**Click**:
A line of `clicks/*.csv`: a user ID clicked a campaign at a timestamp.
_Avoid_: touch, impression

**Link**:
A line of `links/*.csv` naming two user IDs of one person.
_Avoid_: edge, identity link, alias pair

**Window**:
The dataset's half-open time range `[window_start, window_end)` from manifest.json. Events must start inside it; clicks may start up to 7 days before it.
_Avoid_: period, date range

### Identity

**User ID**:
The exact bytes of a `user_id` field. Compared byte for byte, case-sensitive, untrimmed.
_Avoid_: user, account, cookie, device ID

**Person**:
The set of user IDs joined by links, transitively. A user ID in no link is a person of one ID.
_Avoid_: user, customer, entity, cluster

**Name**:
The byte-wise smallest user ID of a person. The `user_id` written in user_daily.
_Avoid_: canonical ID, root, representative

### Surviving data

**Copy**:
One of several events sharing an event_id. The copy with the smallest ingest time is kept; the others are discarded before validation.
_Avoid_: duplicate (used only for the act of removing copies: dedup)

**Valid event**:
A kept copy that passes every check in rules.md "Events" step 3: user ID present, timestamp valid and in the window, type exactly view, cart or purchase, product present, and a purchase price of 1 to 10,000,000 cents.
_Avoid_: good event, clean event

**Late event**:
A valid event ingested more than 24 hours after its timestamp. Dropped.

**Surviving event**:
A valid event that is not late. Only surviving events form sessions.

**Valid click**:
A deduped click with a user ID, a timestamp in the click window, and a campaign present in campaigns.csv.

### Sessions

**Session**:
A maximal run of one person's surviving events, in timestamp order, with no gap over 30 minutes between distinct timestamps. Starts at its first event's timestamp.
_Avoid_: visit, episode

**Session day**:
The UTC date of a session's start. Every event and purchase of the session counts on that day.
_Avoid_: date, event day

**Category**:
The product's category from products.csv, or `unknown` for a product not in the file.

**Top category**:
The category with the highest summed purchase price in a person's sessions on one day; ties go to the byte-wise smallest name; null without purchases.

### Attribution

**Credit**:
The one click a session is attributed to: the person's latest valid click at or before the session start and within 7 days of it; on equal timestamps, the smallest click_id byte-wise. Without one, the session is credited to `organic`.
_Avoid_: attribution, last-touch, conversion

**Assist**:
A campaign assists a session when the person has at least one valid click for it in the 7 days up to and including the session start. The crediting campaign assists too. `organic` never assists.
_Avoid_: influence, touchpoint, multi-touch

**Organic**:
The campaign and channel, both spelled `organic`, of a session with no credit. Never in campaigns.csv.

### Output

**user_daily**:
One row per person name and session day: sessions, events, revenue in cents, top category.

**campaign_daily**:
One row per campaign (or organic) and day with at least one credited or assisted session: credited sessions, purchasing sessions, revenue, assisted sessions.
