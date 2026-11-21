#!/usr/bin/env python3
"""
A plain reading of rules.md, written for clarity rather than speed, used to
cross-check the solver on generated data.

Usage: python reference.py DATA_DIR OUT_DIR
Writes OUT_DIR/user_daily.csv and OUT_DIR/campaign_daily.csv in the format
of the fixtures' expected tables (day as YYYY-MM-DD, empty top_category for
null). Bytes are read and written as latin-1 so comparisons stay byte-wise.
"""
import glob
import json
import os
import re
import sys
from datetime import date

DAY = 86_400_000
WEEK = 7 * DAY
GAP = 30 * 60 * 1000
EPOCH = date(1970, 1, 1).toordinal()

F1 = re.compile(r"^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})Z$", re.ASCII)
F2 = re.compile(r"^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})([+-])(\d{2}):(\d{2})$", re.ASCII)
F3 = re.compile(r"^[0-9]+$", re.ASCII)
PRICE = re.compile(r"^[0-9]+$", re.ASCII)


def split_line(line, count):
    parts = line.split(",")
    if len(parts) != count:
        return None
    fields = []
    for field in parts:
        if field.startswith('"'):
            if len(field) < 2 or not field.endswith('"') or '"' in field[1:-1]:
                return None
            fields.append(field[1:-1])
        else:
            if '"' in field:
                return None
            fields.append(field)
    return fields


def read_lines(folder):
    for path in sorted(glob.glob(os.path.join(folder, "*.csv"))):
        with open(path, "rb") as handle:
            text = handle.read().decode("latin-1")
        lines = text.split("\n")
        if text.endswith("\n"):
            lines.pop()
        yield from lines[1:]


def parse_ts(text):
    if F3.match(text):
        value = int(text)
        return value if value <= 2**63 - 1 else None
    match = F1.match(text) or F2.match(text)
    if not match:
        return None
    year, month, day, hour, minute, second = (int(x) for x in match.groups()[:6])
    if hour > 23 or minute > 59 or second > 59:
        return None
    try:
        days = date(year, month, day).toordinal() - EPOCH
    except ValueError:
        return None
    ms = ((days * 24 + hour) * 60 + minute) * 60_000 + second * 1000
    if len(match.groups()) == 9:
        sign, oh, om = match.group(7), int(match.group(8)), int(match.group(9))
        if oh > 23 or om > 59:
            return None
        offset = (oh * 60 + om) * 60_000
        ms = ms + offset if sign == "-" else ms - offset
    return ms


def day_text(ms):
    return date.fromordinal(EPOCH + ms // DAY).isoformat()


def main():
    data, out = sys.argv[1], sys.argv[2]
    with open(os.path.join(data, "manifest.json"), encoding="utf-8") as handle:
        manifest = json.load(handle)
    window_start = parse_ts(manifest["window_start"])
    window_end = parse_ts(manifest["window_end"])
    if window_start is None or window_end is None:
        sys.exit("manifest.json: bad window")

    products = {}
    for line in read_lines_file(os.path.join(data, "products.csv")):
        fields = split_line(line, 2)
        if fields:
            products[fields[0]] = fields[1]
    campaigns = {}
    for line in read_lines_file(os.path.join(data, "campaigns.csv")):
        fields = split_line(line, 2)
        if fields:
            campaigns[fields[0]] = fields[1]

    parent = {}

    def find(x):
        parent.setdefault(x, x)
        while parent[x] != x:
            parent[x] = parent[parent[x]]
            x = parent[x]
        return x

    for line in read_lines(os.path.join(data, "links")):
        fields = split_line(line, 2)
        if fields is None or fields[0] == "" or fields[1] == "":
            continue
        a, b = find(fields[0]), find(fields[1])
        if a != b:
            parent[a] = b
    members = {}
    for key in list(parent):
        members.setdefault(find(key), []).append(key)
    name = {}
    for ids in members.values():
        smallest = min(ids)
        for key in ids:
            name[key] = smallest

    def person(user_id):
        return name.get(user_id, user_id)

    kept = {}
    for line in read_lines(os.path.join(data, "events")):
        fields = split_line(line, 7)
        if fields is None or fields[0] == "":
            continue
        ingest = parse_ts(fields[3])
        if ingest is None:
            ingest = float("inf")
        if fields[0] not in kept or ingest < kept[fields[0]][0]:
            kept[fields[0]] = (ingest, fields)
    events_by_person = {}
    for ingest, fields in kept.values():
        _, user, ts_text, _, kind, product, price_text = fields
        ts = parse_ts(ts_text)
        if user == "" or ts is None or not (window_start <= ts < window_end):
            continue
        if kind not in ("view", "cart", "purchase") or product == "":
            continue
        price = 0
        if kind == "purchase":
            if not PRICE.match(price_text):
                continue
            price = int(price_text)
            if not 1 <= price <= 10_000_000:
                continue
        if ingest == float("inf") or ingest - ts > DAY:
            continue
        events_by_person.setdefault(person(user), []).append((ts, kind, product, price))

    seen_clicks = set()
    clicks_by_person = {}
    for line in read_lines(os.path.join(data, "clicks")):
        fields = split_line(line, 4)
        if fields is None or fields[0] == "":
            continue
        click_id, user, campaign, ts_text = fields
        if click_id in seen_clicks:
            continue
        seen_clicks.add(click_id)
        ts = parse_ts(ts_text)
        if user == "" or ts is None or not (window_start - WEEK <= ts < window_end):
            continue
        if campaign not in campaigns:
            continue
        clicks_by_person.setdefault(person(user), []).append((ts, click_id, campaign))

    user_daily = {}
    campaign_daily = {}
    for who, events in events_by_person.items():
        events.sort(key=lambda e: e[0])
        clicks = clicks_by_person.get(who, [])
        sessions = []
        current = []
        for event in events:
            if current and event[0] - current[-1][0] > GAP:
                sessions.append(current)
                current = []
            current.append(event)
        sessions.append(current)
        for session in sessions:
            start = session[0][0]
            day = day_text(start)
            revenue = sum(e[3] for e in session if e[1] == "purchase")
            row = user_daily.setdefault((who, day), {"sessions": 0, "events": 0, "revenue": 0, "cats": {}})
            row["sessions"] += 1
            row["events"] += len(session)
            row["revenue"] += revenue
            for event in session:
                if event[1] == "purchase":
                    category = products.get(event[2], "unknown")
                    row["cats"][category] = row["cats"].get(category, 0) + event[3]
            candidates = [c for c in clicks if c[0] <= start and start - c[0] <= WEEK]
            if candidates:
                latest = max(c[0] for c in candidates)
                best = min((c for c in candidates if c[0] == latest), key=lambda c: c[1])
                campaign = best[2]
            else:
                campaign = "organic"
            cell = campaign_daily.setdefault((campaign, day), {"sessions": 0, "purchasing": 0, "revenue": 0, "assisted": 0})
            cell["sessions"] += 1
            if revenue > 0:
                cell["purchasing"] += 1
                cell["revenue"] += revenue
            for assisting in {c[2] for c in candidates}:
                campaign_daily.setdefault((assisting, day), {"sessions": 0, "purchasing": 0, "revenue": 0, "assisted": 0})["assisted"] += 1

    os.makedirs(out, exist_ok=True)
    with open(os.path.join(out, "user_daily.csv"), "wb") as handle:
        handle.write(b"user_id,day,sessions,events,revenue_cents,top_category\n")
        for (who, day), row in sorted(user_daily.items()):
            top = ""
            if row["cats"]:
                best_sum = max(row["cats"].values())
                top = min(c for c, s in row["cats"].items() if s == best_sum)
            line = f"{who},{day},{row['sessions']},{row['events']},{row['revenue']},{top}\n"
            handle.write(line.encode("latin-1"))
    with open(os.path.join(out, "campaign_daily.csv"), "wb") as handle:
        handle.write(b"campaign_id,channel,day,sessions,purchasing_sessions,revenue_cents,assisted_sessions\n")
        for (campaign, day), cell in sorted(campaign_daily.items()):
            channel = campaigns.get(campaign, "organic")
            line = f"{campaign},{channel},{day},{cell['sessions']},{cell['purchasing']},{cell['revenue']},{cell['assisted']}\n"
            handle.write(line.encode("latin-1"))


def read_lines_file(path):
    with open(path, "rb") as handle:
        text = handle.read().decode("latin-1")
    lines = text.split("\n")
    if text.endswith("\n"):
        lines.pop()
    return lines[1:]


if __name__ == "__main__":
    main()
