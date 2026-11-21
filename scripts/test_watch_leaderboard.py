#!/usr/bin/env python3
"""
Tests for watch_leaderboard: board validation, leader and target choice,
polling, recording, freshness checks and the command line. Every test injects
its fetcher, clock and sleep, and a module-level patch makes any real HTTP
request fail, so no test reaches the network.

Run from the repository root:
    python -m unittest discover -s scripts -p "test_watch_leaderboard.py" -v
"""
import contextlib
import copy
from datetime import datetime, timedelta, timezone
import http.client
import io
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

import watch_leaderboard as wl

NOW = datetime(2026, 11, 21, 1, 30, tzinfo=timezone.utc)
SCRIPT = Path(__file__).resolve().parent / "watch_leaderboard.py"

# A subset of the public standings board at 2026-11-21T01:29Z; each row is unchanged.
SAMPLE_ROWS = [
    {"at": "2026-11-21T01:02:43.854034+00:00", "rank": 1, "team": "BobBuildIt", "score": 1.17},
    {"at": "2026-11-21T00:37:22.08431+00:00", "rank": 2, "team": "last friday night", "score": 1.24},
    {"at": "2026-11-21T01:02:45.902204+00:00", "rank": 3, "team": "Team-Xero", "score": 1.35},
    {"at": "2026-11-21T01:03:36.550161+00:00", "rank": 5, "team": "prompt-gods", "score": 1.38},
    {"at": "2026-11-21T01:19:12.406135+00:00", "rank": 15, "team": "explorer-skp", "score": 1.58},
    {"at": "2026-11-21T00:49:04.664582+00:00", "rank": 16, "team": "yoookay", "score": 1.58},
    {"at": "2026-11-21T01:22:33.026538+00:00", "rank": 19, "team": "ImagineLosing", "score": 1.64},
    {"at": "2026-11-21T00:17:21.609914+00:00", "rank": 20, "team": "sparkling coffee", "score": 1.64},
    {"at": "2026-11-21T00:28:32.743531+00:00", "rank": 21, "team": "Wolverines", "score": 1.64},
]

_NO_NETWORK = mock.patch.object(wl.urllib.request, "urlopen",
                                side_effect=AssertionError("a test tried to open a network connection"))


def setUpModule():
    _NO_NETWORK.start()


def tearDownModule():
    _NO_NETWORK.stop()


def board(rows=None, **changes):
    """Returns a board shaped like the public standings response, with changes applied."""
    value = {
        "key": "shopping-sessions", "open": True, "unit": "s", "final": None, "closed": False,
        "metric": "Run time", "paused": False, "opens_at": "2026-11-20T23:00:00+00:00",
        "tie_rule": "Equal scores rank the earlier submission first.",
        "cutoff_at": "2026-11-22T16:00:00+00:00", "direction": "lower", "locked_at": None,
        "standings": copy.deepcopy(SAMPLE_ROWS if rows is None else rows),
        "team_count": 71, "final_count": None, "queue_depth": 0, "published_at": None,
        "active_workers": 14, "baseline_score": 172, "practice_limit": 2,
        "scorer_version": "shopping-sessions-4", "max_queue_depth": 1000, "submission_kind": "repo",
    }
    value.update(changes)
    return value


def ours(score, at="2026-11-21T01:25:00+00:00", rank=1):
    """Returns a standings row for our team."""
    return {"at": at, "rank": rank, "team": wl.TEAM, "score": score}


def rows_named(*teams):
    """Returns copies of the sample rows of the named teams."""
    return [copy.deepcopy(row) for row in SAMPLE_ROWS if row["team"] in teams]


def serve(value):
    """Returns a fetch callable that always answers with value as JSON text."""
    text = json.dumps(value)
    return lambda url: text


def sequence(*outcomes):
    """Returns (fetch, calls): fetch answers with each outcome in turn and raises the exception outcomes.

    A call past the last outcome raises KeyboardInterrupt, which ends a watcher loop.
    """
    calls = []

    def fetch(url):
        calls.append(url)
        if len(calls) > len(outcomes):
            raise KeyboardInterrupt
        outcome = outcomes[len(calls) - 1]
        if isinstance(outcome, BaseException):
            raise outcome
        return outcome
    return fetch, calls


class FakeTime:
    """A clock plus a sleep that advances it."""

    def __init__(self, start):
        self.now = start
        self.slept = 0.0

    def clock(self):
        return self.now

    def sleep(self, seconds):
        self.slept += seconds
        self.now += timedelta(seconds=seconds)


class Trickle:
    """A response body that arrives one byte per second of its own clock.

    read1 returns what one socket read returns here: a single byte. read keeps
    reading until it has the requested amount or the body ends, as a buffered
    reader does.
    """

    def __init__(self, size):
        self.left = size
        self.now = 0.0

    def __enter__(self):
        return self

    def __exit__(self, *details):
        return False

    def read1(self, amount):
        if not self.left or not amount:
            return b""
        self.left -= 1
        self.now += 1.0
        return b" "

    def read(self, amount):
        data = b""
        while len(data) < amount:
            chunk = self.read1(amount - len(data))
            if not chunk:
                break
            data += chunk
        return data


REJECTIONS = [
    ("key", lambda b: b.update(key="builders")),
    ("direction", lambda b: b.update(direction="higher")),
    ("metric", lambda b: b.update(metric="Placement points")),
    ("unit", lambda b: b.update(unit="ms")),
    ("unit", lambda b: b.pop("unit")),
    ("closed", lambda b: b.update(closed="false")),
    ("paused", lambda b: b.update(paused=None)),
    ("cutoff_at", lambda b: b.update(cutoff_at="2026-11-22T16:00:00")),
    ("cutoff_at", lambda b: b.pop("cutoff_at")),
    ("team_count", lambda b: b.update(team_count=True)),
    ("team_count", lambda b: b.update(team_count=-1)),
    ("practice_limit", lambda b: b.update(practice_limit="2")),
    ("scorer_version", lambda b: b.update(scorer_version=4)),
    ("baseline_score", lambda b: b.update(baseline_score=float("nan"))),
    ("baseline_score", lambda b: b.update(baseline_score=0)),
    ("standings", lambda b: b.update(standings={})),
    ("standings", lambda b: b.pop("standings")),
    ("standings", lambda b: b["standings"].extend([ours(1.2), ours(1.3)])),
    ("standings[1]", lambda b: b["standings"].insert(1, None)),
    ("standings[0].rank", lambda b: b["standings"][0].update(rank=True)),
    ("standings[0].rank", lambda b: b["standings"][0].update(rank=0)),
    ("standings[0].rank", lambda b: b["standings"][0].update(rank="1")),
    ("standings[0].rank", lambda b: b["standings"][0].update(rank=1.0)),
    ("standings[0].team", lambda b: b["standings"][0].update(team=None)),
    ("standings[0].team", lambda b: b["standings"][0].update(team=7)),
    ("standings[2].score", lambda b: b["standings"][2].update(score=float("nan"))),
    ("standings[2].score", lambda b: b["standings"][2].update(score=float("inf"))),
    ("standings[2].score", lambda b: b["standings"][2].update(score=0)),
    ("standings[2].score", lambda b: b["standings"][2].update(score=-1.5)),
    ("standings[2].score", lambda b: b["standings"][2].update(score=True)),
    ("standings[2].score", lambda b: b["standings"][2].update(score="1.35")),
    ("standings[2].score", lambda b: b["standings"][2].update(score=10 ** 400)),
    ("standings[2].score", lambda b: b["standings"][2].pop("score")),
    ("standings[3].at", lambda b: b["standings"][3].update(at="2026-11-21T01:03:36")),
    ("standings[3].at", lambda b: b["standings"][3].update(at="yesterday")),
    ("standings[3].at", lambda b: b["standings"][3].update(at=None)),
    ("standings[3].at", lambda b: b["standings"][3].update(at="2026-13-01T00:00:00+00:00")),
    ("standings[3].at", lambda b: b["standings"][3].update(at="2026-11-21 01:03:36+00:00")),
]


class SummarizeTests(unittest.TestCase):
    def test_sample_board_without_our_entry(self):
        result = wl.summarize(board())
        self.assertEqual((result["leader_team"], result["leader_score"], result["leader_rank"]),
                         ("BobBuildIt", 1.17, 1))
        self.assertEqual(result["leader_at"], "2026-11-21T01:02:43.854034+00:00")
        self.assertEqual((result["reference"], result["reference_score"]), ("leader", 1.17))
        self.assertEqual(result["target_score"], 1.111)
        for name in ("our_score", "our_rank", "our_at", "gap_to_leader_s", "gap_to_target_s"):
            self.assertIsNone(result[name], name)
        self.assertIs(result["beats_leader"], False)
        self.assertIs(result["target_met"], False)
        self.assertEqual((result["team_count"], result["practice_limit"]), (71, 2))
        self.assertEqual(result["cutoff_at"], "2026-11-22T16:00:00+00:00")
        self.assertEqual((result["closed"], result["paused"]), (False, False))
        self.assertEqual(result["scorer_version"], "shopping-sessions-4")
        self.assertEqual(result["baseline_score"], 172)
        self.assertEqual([(row["team"], row["score"], row["rank"]) for row in result["top5"]],
                         [("BobBuildIt", 1.17, 1), ("last friday night", 1.24, 2), ("Team-Xero", 1.35, 3),
                          ("prompt-gods", 1.38, 5), ("yoookay", 1.58, 16)])
        self.assertEqual((result["team"], result["margin"]), (wl.TEAM, 0.05))
        self.assertEqual((result["metric"], result["unit"], result["direction"]), ("Run time", "s", "lower"))

    def test_our_entry_against_leader_and_target(self):
        cases = [
            (1.05, True, True, -0.12, -0.061),
            (1.15, True, False, -0.02, 0.039),
            (1.30, False, False, 0.13, 0.189),
        ]
        for score, beats, met, to_leader, to_target in cases:
            with self.subTest(score=score):
                result = wl.summarize(board(SAMPLE_ROWS + [ours(score, rank=4)]))
                self.assertEqual(result["leader_team"], "BobBuildIt")
                self.assertEqual(result["target_score"], 1.111)
                self.assertEqual((result["our_score"], result["our_rank"]), (score, 4))
                self.assertEqual(result["our_at"], "2026-11-21T01:25:00+00:00")
                self.assertIs(result["beats_leader"], beats)
                self.assertIs(result["target_met"], met)
                self.assertEqual(result["gap_to_leader_s"], to_leader)
                self.assertEqual(result["gap_to_target_s"], to_target)

    def test_our_lead_is_never_the_leader(self):
        result = wl.summarize(board(SAMPLE_ROWS + [ours(0.9)]))
        self.assertEqual(result["leader_team"], "BobBuildIt")
        self.assertEqual(result["top5"][0]["team"], wl.TEAM)
        self.assertEqual(result["target_score"], 1.111)

    def test_margin_sets_the_target(self):
        for margin, target in ((0.1, 1.053), (0, 1.17), (0.05, 1.111)):
            with self.subTest(margin=margin):
                self.assertEqual(wl.summarize(board(), margin=margin)["target_score"], target)

    def test_equal_scores_rank_the_earlier_submission_first(self):
        result = wl.summarize(board(rows_named("ImagineLosing", "sparkling coffee", "Wolverines")))
        self.assertEqual((result["leader_team"], result["leader_rank"]), ("sparkling coffee", 20))
        self.assertEqual(result["target_score"], 1.558)
        result = wl.summarize(board(rows_named("explorer-skp", "yoookay")))
        self.assertEqual((result["leader_team"], result["leader_rank"]), ("yoookay", 16))

    def test_our_equal_score_beats_the_leader_only_when_earlier(self):
        rivals = rows_named("ImagineLosing", "sparkling coffee", "Wolverines")
        cases = [
            ("2026-11-21T00:17:21.609913+00:00", True),
            ("2026-11-21T01:17:21.609913+01:00", True),
            ("2026-11-21T00:17:21.609914+00:00", False),
            ("2026-11-21T00:17:21.609915Z", False),
        ]
        for at, wins in cases:
            with self.subTest(at=at):
                result = wl.summarize(board(rivals + [ours(1.64, at=at, rank=19)]), margin=0)
                self.assertEqual(result["leader_team"], "sparkling coffee")
                self.assertEqual(result["target_score"], 1.64)
                self.assertEqual(result["gap_to_leader_s"], 0.0)
                self.assertIs(result["beats_leader"], wins)
                self.assertIs(result["target_met"], wins)

    def test_no_competitors_uses_the_baseline(self):
        result = wl.summarize(board([]))
        self.assertEqual((result["reference"], result["reference_score"]), ("baseline", 172))
        self.assertEqual(result["target_score"], 163.4)
        for name in ("leader_team", "leader_score", "leader_at", "leader_rank", "our_score"):
            self.assertIsNone(result[name], name)
        self.assertEqual(result["top5"], [])
        cases = [(150, True, True, -22.0, -13.4), (170, True, False, -2.0, 6.6), (172, False, False, 0.0, 8.6)]
        for score, beats, met, to_reference, to_target in cases:
            with self.subTest(score=score):
                result = wl.summarize(board([ours(score)]))
                self.assertEqual(result["reference"], "baseline")
                self.assertIsNone(result["leader_team"])
                self.assertIs(result["beats_leader"], beats)
                self.assertIs(result["target_met"], met)
                self.assertEqual(result["gap_to_leader_s"], to_reference)
                self.assertEqual(result["gap_to_target_s"], to_target)

    def test_rejects_each_malformed_field(self):
        for field, mutate in REJECTIONS:
            value = board()
            mutate(value)
            with self.subTest(field=field), self.assertRaisesRegex(ValueError, "^" + re.escape(field + ":")):
                wl.summarize(value)

    def test_rejects_a_board_that_is_not_an_object(self):
        for value in ([board()], None, "board"):
            with self.subTest(value=type(value).__name__), self.assertRaisesRegex(ValueError, "^board:"):
                wl.summarize(value)

    def test_rejects_bad_team_and_margin(self):
        for margin in (-0.01, 1, 1.5, float("nan"), float("inf"), True, "0.05", None):
            with self.subTest(margin=margin), self.assertRaisesRegex(ValueError, "^margin:"):
                wl.summarize(board(), margin=margin)
        for team in ("", None, 7):
            with self.subTest(team=team), self.assertRaisesRegex(ValueError, "^team:"):
                wl.summarize(board(), team=team)


class ParseInstantTests(unittest.TestCase):
    def test_accepts_iso_timestamps_with_timezone(self):
        utc = timezone.utc
        cases = {
            "2026-11-21T00:37:22.08431+00:00": datetime(2026, 11, 21, 0, 37, 22, 84310, tzinfo=utc),
            "2026-11-21T01:02:43.854034+00:00": datetime(2026, 11, 21, 1, 2, 43, 854034, tzinfo=utc),
            "2026-11-21T01:02:43Z": datetime(2026, 11, 21, 1, 2, 43, tzinfo=utc),
            "2026-11-21T06:32:43.123456789+05:30": datetime(2026, 11, 21, 1, 2, 43, 123456, tzinfo=utc),
            "2026-11-20T20:02:43-05:00": datetime(2026, 11, 21, 1, 2, 43, tzinfo=utc),
        }
        for text, expected in cases.items():
            with self.subTest(text=text):
                parsed = wl.parse_instant(text, "when")
                self.assertEqual(parsed, expected)
                self.assertEqual(parsed.utcoffset(), timedelta(0))

    def test_rejects_anything_else(self):
        for value in ("2026-11-21T01:02:43", "2026-11-21 01:02:43+00:00", "2026-11-21T01:02+00:00",
                      "2026-02-30T00:00:00Z", "2026-11-21T01:02:43+24:00", " 2026-11-21T01:02:43Z",
                      "2026-11-21T01:02:43z", "", None, 1763686963):
            with self.subTest(value=value), self.assertRaisesRegex(ValueError, "^when:"):
                wl.parse_instant(value, "when")


class FetchBoardTests(unittest.TestCase):
    def opener(self, body):
        seen = {}

        def opener(request, timeout):
            seen["request"], seen["timeout"] = request, timeout
            return io.BytesIO(body)
        return opener, seen

    def test_sends_one_get_with_json_headers_and_decodes_utf8(self):
        opener, seen = self.opener(json.dumps(board()).encode("utf-8"))
        text = wl.fetch_board(opener=opener)
        self.assertEqual(json.loads(text)["key"], "shopping-sessions")
        request = seen["request"]
        self.assertEqual((request.full_url, request.get_method()), (wl.URL, "GET"))
        self.assertEqual(request.get_header("Accept"), "application/json")
        self.assertEqual(request.get_header("Cache-control"), "no-cache")
        self.assertEqual(seen["timeout"], 15)

    def test_refuses_a_body_over_the_size_cap(self):
        opener, _ = self.opener(b"x" * 10)
        self.assertEqual(wl.fetch_board(limit=10, opener=opener), "x" * 10)
        opener, _ = self.opener(b"x" * 11)
        with self.assertRaisesRegex(ValueError, "larger than 10 bytes"):
            wl.fetch_board(limit=10, opener=opener)
        self.assertEqual(wl.MAX_RESPONSE_BYTES, 2_000_000)

    def test_refuses_invalid_utf8(self):
        opener, _ = self.opener(b"{\xff}")
        with self.assertRaises(UnicodeDecodeError):
            wl.fetch_board(opener=opener)

    def test_bounds_the_whole_response_time(self):
        opener, _ = self.opener(b"{}")
        ticks = iter([0.0, 15.5])
        with self.assertRaisesRegex(TimeoutError, "longer than 15 s"):
            wl.fetch_board(opener=opener, clock=lambda: next(ticks))

    def test_stops_reading_a_trickling_body_at_the_deadline(self):
        body = Trickle(100)
        with self.assertRaisesRegex(TimeoutError, "longer than 15 s"):
            wl.fetch_board(opener=lambda request, timeout: body, clock=lambda: body.now)
        self.assertLessEqual(body.now, 16)


class PollTests(unittest.TestCase):
    def previous(self):
        state = wl.poll(None, serve(board(SAMPLE_ROWS + [ours(1.3, rank=4)])), lambda: NOW - timedelta(minutes=5))
        self.assertEqual(state["status"], "fresh")
        return state

    def test_fresh_poll_reports_the_summary_and_times(self):
        fetch, calls = sequence(json.dumps(board()))
        state = wl.poll(None, fetch, lambda: NOW)
        self.assertEqual(calls, [wl.URL])
        self.assertEqual(state["status"], "fresh")
        self.assertEqual(state["checked_at"], "2026-11-21T01:30:00+00:00")
        self.assertEqual(state["last_success_at"], state["checked_at"])
        self.assertIsNone(state["error"])
        self.assertEqual(state["source"], wl.URL)
        extra = ("status", "checked_at", "last_success_at", "source", "error")
        self.assertEqual({k: v for k, v in state.items() if k not in extra}, wl.summarize(board()))

    def test_any_failure_returns_the_previous_values_marked_stale(self):
        previous = self.previous()
        failures = [
            (OSError("network down"), "OSError: network down"),
            (TimeoutError("timed out"), "TimeoutError: timed out"),
            (http.client.IncompleteRead(b"partial"), "IncompleteRead"),
            (RuntimeError("unexpected"), "RuntimeError: unexpected"),
            ("<html>busy</html>", "JSONDecodeError"),
            ("[]", "board: expected a JSON object"),
            (json.dumps(board(direction="higher")), "direction: expected 'lower'"),
            (b"{}", "fetch: expected text"),
        ]
        for outcome, needle in failures:
            with self.subTest(needle=needle):
                state = wl.poll(previous, sequence(outcome)[0], lambda: NOW)
                self.assertEqual(state["status"], "stale")
                self.assertIn(needle, state["error"])
                self.assertEqual(state["checked_at"], "2026-11-21T01:30:00+00:00")
                self.assertEqual(state["last_success_at"], "2026-11-21T01:25:00+00:00")
                for name in ("our_score", "our_rank", "leader_team", "target_score"):
                    self.assertEqual(state[name], previous[name], name)

    def test_failure_without_previous_state_has_no_success_time(self):
        state = wl.poll(None, sequence(OSError("offline"))[0], lambda: NOW)
        self.assertEqual(state, {"team": wl.TEAM, "margin": 0.05, "source": wl.URL, "status": "stale",
                                 "checked_at": "2026-11-21T01:30:00+00:00", "error": "OSError: offline"})

    def test_bad_clock_returns_stale_without_fetching(self):
        previous = self.previous()
        state = wl.poll(previous, sequence()[0], lambda: datetime(2026, 11, 21, 1, 30))
        self.assertEqual(state["status"], "stale")
        self.assertIsNone(state["checked_at"])
        self.assertIn("clock:", state["error"])
        self.assertEqual(state["last_success_at"], previous["last_success_at"])


class RecordTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.folder = Path(directory.name)
        self.path = self.folder / "live-target.json"
        self.history = self.folder / "live-target.jsonl"

    def lines(self):
        return [json.loads(line) for line in self.history.read_text(encoding="utf-8").splitlines()]

    def test_history_grows_only_when_a_meaningful_field_changes(self):
        state = wl.poll(None, serve(board()), lambda: NOW)
        self.assertIs(wl.record(state, self.path), True)
        self.assertEqual(json.loads(self.path.read_text(encoding="utf-8")), state)
        self.assertEqual(self.lines(), [state])
        later = dict(state, checked_at="2026-11-21T01:31:00+00:00", last_success_at="2026-11-21T01:31:00+00:00",
                     top5=[], paused=True, error="not a tracked field")
        self.assertIs(wl.record(later, self.path), False)
        self.assertEqual(json.loads(self.path.read_text(encoding="utf-8")), later)
        self.assertEqual(len(self.lines()), 1)
        changes = [("leader_team", "Team-Xero"), ("leader_score", 1.1), ("target_score", 1.045),
                   ("our_score", 1.3), ("our_rank", 4), ("status", "stale"), ("closed", True)]
        current = later
        for count, (name, value) in enumerate(changes, start=2):
            current = dict(current, **{name: value})
            with self.subTest(field=name):
                self.assertIs(wl.record(current, self.path), True)
                self.assertEqual(len(self.lines()), count)
                self.assertEqual(self.lines()[-1], current)
        self.assertEqual(sorted(p.name for p in self.folder.iterdir()),
                         ["live-target.json", "live-target.json.lock", "live-target.jsonl"])

    def test_files_use_unix_newlines(self):
        wl.record(wl.poll(None, serve(board()), lambda: NOW), self.path)
        for path in (self.path, self.history):
            self.assertNotIn(b"\r", path.read_bytes(), path.name)

    def test_rejects_unserializable_state_and_keeps_the_old_file(self):
        state = wl.poll(None, serve(board()), lambda: NOW)
        wl.record(state, self.path)
        before = self.path.read_bytes()
        for bad in (dict(state, target_score=float("nan")), dict(state, checked_at=NOW), ["not", "a", "dict"]):
            with self.subTest(bad=type(bad).__name__), self.assertRaisesRegex(ValueError, "^state:"):
                wl.record(bad, self.path)
        self.assertEqual(self.path.read_bytes(), before)
        self.assertEqual(len(self.lines()), 1)

    def test_rejects_an_output_path_that_is_its_own_history(self):
        with self.assertRaisesRegex(ValueError, "^output:"):
            wl.record({"status": "fresh"}, self.folder / "state.jsonl")

    def test_unreadable_stored_state_counts_as_a_change(self):
        self.path.write_text("{partial", encoding="utf-8")
        state = wl.poll(None, serve(board()), lambda: NOW)
        self.assertIs(wl.record(state, self.path), True)
        self.assertEqual(json.loads(self.path.read_text(encoding="utf-8")), state)

    def test_failed_replace_keeps_the_old_file_and_no_temporary(self):
        state = wl.poll(None, serve(board()), lambda: NOW)
        wl.record(state, self.path)
        before = self.path.read_bytes()
        with mock.patch.object(wl.os, "replace", side_effect=OSError(28, "No space left on device")):
            with self.assertRaisesRegex(OSError, "No space left"):
                wl.record(dict(state, status="stale"), self.path)
        self.assertEqual(self.path.read_bytes(), before)
        self.assertEqual(sorted(p.name for p in self.folder.iterdir()),
                         ["live-target.json", "live-target.json.lock", "live-target.jsonl"])

    def test_partial_last_history_line_does_not_swallow_the_next_entry(self):
        self.history.write_bytes(b'{"partial')
        state = wl.poll(None, serve(board()), lambda: NOW)
        self.assertIs(wl.record(state, self.path), True)
        raw = self.history.read_bytes()
        self.assertTrue(raw.startswith(b'{"partial\n'), raw[:40])
        self.assertEqual(json.loads(raw.splitlines()[-1]), state)

    def test_rejects_an_output_path_that_cannot_hold_the_state(self):
        state = wl.poll(None, serve(board()), lambda: NOW)
        existing = self.folder / "existing"
        existing.mkdir()
        for bad in (existing, Path(""), self.folder / "state.JSONL"):
            with self.subTest(path=str(bad)), self.assertRaisesRegex(ValueError, "^output:"):
                wl.record(state, bad)
        self.assertEqual([p.name for p in self.folder.iterdir()], ["existing"])


class LockTests(unittest.TestCase):
    def test_second_holder_times_out_while_the_first_holds_the_lock(self):
        with tempfile.TemporaryDirectory() as folder:
            lock = Path(folder) / "state.lock"
            with wl._exclusive(lock):
                with self.assertRaisesRegex(TimeoutError, "still held"):
                    with wl._exclusive(lock, timeout_s=0.2):
                        self.fail("second holder entered while the lock was held")
            with wl._exclusive(lock, timeout_s=0.2):
                pass


class ReadFreshTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.path = Path(directory.name) / "live-target.json"
        self.state = wl.poll(None, serve(board()), lambda: NOW)
        wl.record(self.state, self.path)

    def read_at(self, seconds, **options):
        return wl.read_fresh(self.path, now=lambda: NOW + timedelta(seconds=seconds), **options)

    def test_freshness_boundaries(self):
        self.assertEqual(self.read_at(0), self.state)
        self.assertEqual(self.read_at(90), self.state)
        for seconds in (90.000001, 91):
            with self.subTest(seconds=seconds), self.assertRaisesRegex(ValueError, "s old, over the 90 s limit"):
                self.read_at(seconds)
        for seconds in (-0.000001, -1):
            with self.subTest(seconds=seconds), self.assertRaisesRegex(ValueError, "in the future"):
                self.read_at(seconds)

    def test_custom_max_age(self):
        self.assertEqual(self.read_at(30, max_age_s=30), self.state)
        with self.assertRaisesRegex(ValueError, "over the 30 s limit"):
            self.read_at(31, max_age_s=30)
        for bad in (-1, float("nan"), float("inf"), True, "90"):
            with self.subTest(bad=bad), self.assertRaisesRegex(ValueError, "^max_age_s:"):
                self.read_at(0, max_age_s=bad)

    def test_stale_state_is_refused_with_its_error(self):
        stale = wl.poll(self.state, sequence(OSError("offline"))[0], lambda: NOW + timedelta(seconds=5))
        wl.record(stale, self.path)
        with self.assertRaisesRegex(ValueError, "'stale', not 'fresh'.*offline"):
            self.read_at(5)

    def test_missing_corrupt_and_incomplete_files_are_refused(self):
        self.path.unlink()
        with self.assertRaisesRegex(ValueError, "no watcher state"):
            self.read_at(0)
        self.path.write_text("{partial", encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "not valid JSON"):
            self.read_at(0)
        self.path.write_text("[]", encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "not a JSON object"):
            self.read_at(0)
        for bad in ({"status": "fresh"}, {"status": "fresh", "last_success_at": "2026-11-21T01:30:00"}):
            self.path.write_text(json.dumps(bad), encoding="utf-8")
            with self.subTest(bad=bad), self.assertRaisesRegex(ValueError, "^last_success_at:"):
                self.read_at(0)

    def test_naive_clock_is_refused(self):
        with self.assertRaisesRegex(ValueError, "^clock:"):
            wl.read_fresh(self.path, now=lambda: datetime(2026, 11, 21, 1, 30))


class DescribeTests(unittest.TestCase):
    def test_line_without_our_entry(self):
        state = wl.poll(None, serve(board()), lambda: NOW)
        self.assertEqual(wl.describe(state), "fresh at 2026-11-21T01:30:00+00:00; leader 1.17 s BobBuildIt; "
                                             "target 1.111 s; ours: no entry; gap n/a")

    def test_line_with_our_entry(self):
        state = wl.poll(None, serve(board(SAMPLE_ROWS + [ours(1.3, rank=4)])), lambda: NOW)
        self.assertEqual(wl.describe(state), "fresh at 2026-11-21T01:30:00+00:00; leader 1.17 s BobBuildIt; "
                                             "target 1.111 s; ours 1.3 s rank 4; "
                                             "gap +0.13 s to leader, +0.189 s to target")

    def test_line_with_the_baseline_as_reference(self):
        state = wl.poll(None, serve(board([ours(150)])), lambda: NOW)
        self.assertEqual(wl.describe(state), "fresh at 2026-11-21T01:30:00+00:00; no other team, baseline 172 s; "
                                             "target 163.4 s; ours 150 s rank 1; "
                                             "gap -22 s to baseline, -13.4 s to target")

    def test_stale_line_without_earlier_values(self):
        state = wl.poll(None, sequence(OSError("offline"))[0], lambda: NOW)
        self.assertEqual(wl.describe(state), "stale at 2026-11-21T01:30:00+00:00; leader unknown; target unknown; "
                                             "ours unknown; gap n/a; error: OSError: offline")

    def test_line_escapes_control_and_non_ascii_characters(self):
        rows = [dict(SAMPLE_ROWS[0], team="spoof\nfresh\x1b[2J\x7f\r\xe9")]
        state = wl.poll(None, serve(board(rows)), lambda: NOW)
        self.assertEqual(wl.describe(state), "fresh at 2026-11-21T01:30:00+00:00; "
                                             "leader 1.17 s spoof\\nfresh\\x1b[2J\\x7f\\r\\xe9; "
                                             "target 1.111 s; ours: no entry; gap n/a")


class MainTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.folder = Path(directory.name)
        self.output = self.folder / "live-target.json"
        self.history = self.folder / "live-target.jsonl"
        self.board = json.dumps(board())

    def run_main(self, args, fetch, now=lambda: NOW, sleep=None):
        def no_sleep(seconds):
            raise AssertionError(f"unexpected sleep of {seconds} s")
        stdout, stderr = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            code = wl.main(args + ["--output", str(self.output)], fetch=fetch, now=now, sleep=sleep or no_sleep)
        return code, stdout.getvalue().splitlines(), stderr.getvalue()

    def stored(self):
        return json.loads(self.output.read_text(encoding="utf-8"))

    def history_lines(self):
        return [json.loads(line) for line in self.history.read_text(encoding="utf-8").splitlines()]

    def test_once_records_and_prints_one_line(self):
        fetch, calls = sequence(self.board)
        code, lines, _ = self.run_main(["--once"], fetch)
        self.assertEqual(code, 0)
        self.assertEqual(calls, [wl.URL])
        self.assertEqual(self.stored()["status"], "fresh")
        self.assertEqual(lines, [wl.describe(self.stored())])
        self.assertEqual(len(self.history_lines()), 1)
        code, lines, _ = self.run_main(["--once"], sequence(self.board)[0])
        self.assertEqual((code, len(lines), len(self.history_lines())), (0, 1, 1))

    def test_once_during_an_outage_keeps_the_last_values(self):
        wl.record(wl.poll(None, serve(board(SAMPLE_ROWS + [ours(1.3, rank=4)])),
                          lambda: NOW - timedelta(minutes=5)), self.output)
        code, lines, _ = self.run_main(["--once"], sequence(OSError("offline"))[0])
        self.assertEqual(code, 1)
        state = self.stored()
        self.assertEqual(state["status"], "stale")
        self.assertEqual((state["our_score"], state["target_score"]), (1.3, 1.111))
        self.assertEqual(state["last_success_at"], "2026-11-21T01:25:00+00:00")
        self.assertEqual(state["checked_at"], "2026-11-21T01:30:00+00:00")
        self.assertEqual(lines, ["stale at 2026-11-21T01:30:00+00:00; leader 1.17 s BobBuildIt; target 1.111 s; "
                                 "ours 1.3 s rank 4; gap +0.13 s to leader, +0.189 s to target; "
                                 "error: OSError: offline"])
        self.assertEqual([h["status"] for h in self.history_lines()], ["fresh", "stale"])

    def test_once_ignores_state_written_for_another_margin_or_team(self):
        for args in (["--margin", "0.1"], ["--team", "Team-Xero"]):
            with self.subTest(args=args):
                wl.record(wl.poll(None, serve(board(SAMPLE_ROWS + [ours(1.3, rank=4)])),
                                  lambda: NOW - timedelta(minutes=5)), self.output)
                code, lines, _ = self.run_main(["--once"] + args, sequence(OSError("offline"))[0])
                self.assertEqual(code, 1)
                self.assertNotIn("our_score", self.stored())
                self.assertNotIn("last_success_at", self.stored())
                self.assertIn("ours unknown", lines[0])

    def test_once_warns_about_a_corrupt_state_file(self):
        self.output.write_text("{partial", encoding="utf-8")
        code, _, errors = self.run_main(["--once"], sequence(OSError("offline"))[0])
        self.assertEqual(code, 1)
        self.assertIn("warning: ignoring unreadable state", errors)
        self.assertEqual(self.stored()["status"], "stale")

    def test_loop_records_changes_only_and_stops_when_the_board_closes(self):
        faster = board()
        faster["standings"][0]["score"] = 1.1
        closed = copy.deepcopy(faster)
        closed["closed"] = True
        fetch, calls = sequence(self.board, self.board, json.dumps(faster), json.dumps(closed))
        fake = FakeTime(NOW)
        code, lines, _ = self.run_main(["--interval", "10"], fetch, fake.clock, fake.sleep)
        self.assertEqual(code, 0)
        self.assertEqual(len(calls), 4)
        self.assertEqual(fake.slept, 30)
        self.assertEqual([(h["leader_score"], h["target_score"], h["closed"]) for h in self.history_lines()],
                         [(1.17, 1.111, False), (1.1, 1.045, False), (1.1, 1.045, True)])
        self.assertEqual(len(lines), 4)
        self.assertTrue(lines[1].startswith("fresh at 2026-11-21T01:30:20+00:00; leader 1.1 s BobBuildIt; "
                                            "target 1.045 s"), lines[1])
        self.assertEqual(lines[3], "board closed; watcher stopped")

    def test_loop_stops_once_the_cutoff_passes(self):
        fetch, calls = sequence(self.board, self.board, self.board)
        fake = FakeTime(datetime(2026, 11, 22, 15, 59, 45, tzinfo=timezone.utc))
        code, lines, _ = self.run_main(["--interval", "10"], fetch, fake.clock, fake.sleep)
        self.assertEqual(code, 0)
        self.assertEqual(len(calls), 3)
        self.assertEqual(lines[-1], "cutoff 2026-11-22T16:00:00+00:00 passed; watcher stopped")
        self.assertEqual(len(self.history_lines()), 1)

    def test_stop_signal_ends_the_loop_and_restores_handlers(self):
        original = signal.getsignal(signal.SIGINT)

        def sleep(seconds):
            signal.getsignal(signal.SIGINT)(signal.SIGINT, None)
        fetch, calls = sequence(self.board, self.board)
        code, lines, _ = self.run_main(["--interval", "10"], fetch, sleep=sleep)
        self.assertEqual(code, 130)
        self.assertEqual(len(calls), 1)
        self.assertEqual(lines[-1], "interrupted; watcher stopped")
        self.assertIs(signal.getsignal(signal.SIGINT), original)

    def test_keyboard_interrupt_ends_the_loop(self):
        def sleep(seconds):
            raise KeyboardInterrupt
        code, lines, _ = self.run_main(["--interval", "10"], sequence(self.board)[0], sleep=sleep)
        self.assertEqual(code, 130)
        self.assertEqual(lines[-1], "interrupted; watcher stopped")

    def test_default_output_is_the_repository_artifacts_folder(self):
        self.assertEqual(wl.DEFAULT_OUTPUT, SCRIPT.parents[1] / "artifacts" / "live-target.json")

    def test_command_line_rejects_bad_arguments(self):
        environment = dict(os.environ, HTTP_PROXY="http://127.0.0.1:9", HTTPS_PROXY="http://127.0.0.1:9",
                           NO_PROXY="")
        target = self.folder / "state.json"
        existing = self.folder / "existing"
        existing.mkdir()
        cases = [
            (["--interval", "5"], "--interval"),
            (["--interval", "nan"], "--interval"),
            (["--margin", "1"], "--margin"),
            (["--margin", "-0.1"], "--margin"),
            (["--team", ""], "--team"),
            (["--output", str(self.folder / "state.jsonl")], "--output"),
            (["--output", str(self.folder / "state.JSONL")], "--output"),
            (["--output", str(existing)], "--output"),
            (["--output", ""], "--output"),
            (["--bogus"], "unrecognized arguments"),
        ]
        for extra, needle in cases:
            with self.subTest(args=extra):
                result = subprocess.run([sys.executable, str(SCRIPT), "--once", "--output", str(target), *extra],
                                        capture_output=True, text=True, errors="replace", env=environment,
                                        cwd=str(self.folder), timeout=60)
                self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
                self.assertIn(needle, result.stderr)
                self.assertFalse(target.exists())
        self.assertEqual([p.name for p in self.folder.iterdir()], ["existing"])


if __name__ == "__main__":
    unittest.main()
