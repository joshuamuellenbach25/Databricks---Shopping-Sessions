#!/usr/bin/env python3
"""
Watches the public Shopping Sessions standings and keeps a live run-time target.

Each poll reads the track's public standings JSON, validates it and takes the
fastest other team as the leader; on equal scores the earlier submission ranks
first. The target is the leader's score less a margin. With no other team on
the board, the published baseline score stands in for the leader.

The state goes to a JSON file through an atomic replace. When the leader,
target, our score or rank, status or closed flag changes, the state is also
appended to a sibling .jsonl history and one readable line goes to stdout. A
failed poll keeps the last known values and marks them stale with the error.

Usage:
    python scripts/watch_leaderboard.py --once
    python scripts/watch_leaderboard.py --interval 30 --margin 0.05

Without --once the watcher polls until the board closes, the cutoff passes or
it is interrupted. Exit status: 0 after a fresh --once poll or a normal stop,
1 after a stale --once poll, 2 for bad arguments, 130 when interrupted.

Other tools call read_fresh(path) to get the state only while it is fresh.
"""
import argparse
import contextlib
from datetime import datetime, timedelta, timezone
import errno
import json
import math
import os
from pathlib import Path
import re
import signal
import sys
import tempfile
import threading
import time
import urllib.request

if os.name == "nt":
    import msvcrt
else:
    import fcntl

URL = "https://www.joinbuilders.org/buildathon/standings/shopping-sessions"
TEAM = "BackwardsDeployeds"
DEFAULT_MARGIN = 0.05
DEFAULT_INTERVAL_S = 30.0
MIN_INTERVAL_S = 10.0
FETCH_TIMEOUT_S = 15.0
MAX_RESPONSE_BYTES = 2_000_000
FRESH_MAX_AGE_S = 90
LOCK_TIMEOUT_S = 10.0
TOP_COUNT = 5
BOARD_IDENTITY = (("key", "shopping-sessions"), ("direction", "lower"), ("metric", "Run time"), ("unit", "s"))
CHANGE_FIELDS = ("leader_team", "leader_score", "target_score", "our_score", "our_rank", "status", "closed")
DEFAULT_OUTPUT = Path(__file__).resolve().parents[1] / "artifacts" / "live-target.json"

_INSTANT = re.compile("([0-9]{4})-([0-9]{2})-([0-9]{2})T([0-9]{2}):([0-9]{2}):([0-9]{2})"
                      "(?:[.]([0-9]+))?(Z|[+-][0-9]{2}:[0-9]{2})")
_LOCK_BUSY = {errno.EACCES, errno.EAGAIN, getattr(errno, "EWOULDBLOCK", errno.EAGAIN),
              getattr(errno, "EDEADLK", errno.EAGAIN)}


def _show(value, limit=80):
    """Returns an ASCII repr of value, shortened to at most limit characters."""
    text = ascii(value)
    return text if len(text) <= limit else text[:limit - 3] + "..."


def _ascii(text):
    """Returns text with each character outside printable ASCII, control characters included, as a backslash escape."""
    return "".join(char if " " <= char <= "~" else ascii(char)[1:-1] for char in text)


def _describe_error(error):
    """Returns 'Type: message' for error, cut to 500 characters."""
    text = f"{type(error).__name__}: {error}"
    return text if len(text) <= 500 else text[:497] + "..."


def _positive_number(value):
    """True for an int or float that is finite as a float and above zero; False for bool and other types."""
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return False
    try:
        number = float(value)
    except OverflowError:
        return False
    return math.isfinite(number) and number > 0


def parse_instant(value, field):
    """Parses an ISO 8601 timestamp that carries a timezone into an aware UTC datetime.

    Accepts YYYY-MM-DDTHH:MM:SS with optional fractional seconds (digits past
    microseconds are dropped) and a Z or +HH:MM / -HH:MM offset. Raises
    ValueError starting with field for any other value.
    """
    match = _INSTANT.fullmatch(value) if isinstance(value, str) else None
    if match is None:
        raise ValueError(f"{field}: expected an ISO timestamp with timezone, got {_show(value)}")
    year, month, day, hour, minute, second, fraction, zone = match.groups()
    offset = timedelta(0)
    if zone != "Z":
        hours, minutes = int(zone[1:3]), int(zone[4:6])
        if hours > 23 or minutes > 59:
            raise ValueError(f"{field}: invalid timezone offset in {_show(value)}")
        offset = timedelta(hours=hours, minutes=minutes) * (1 if zone[0] == "+" else -1)
    micro = int((fraction or "").ljust(6, "0")[:6])
    try:
        parsed = datetime(int(year), int(month), int(day), int(hour), int(minute), int(second), micro,
                          tzinfo=timezone(offset))
        return parsed.astimezone(timezone.utc)
    except (ValueError, OverflowError) as error:
        raise ValueError(f"{field}: invalid timestamp {_show(value)}: {error}") from None


def _system_now():
    """Returns the system clock as an aware UTC datetime."""
    return datetime.now(timezone.utc)


def _clock(now=None):
    """Calls now (default: the system clock) and returns its timezone-aware result in UTC.

    Raises ValueError when the clock returns anything but an aware datetime.
    """
    value = (now or _system_now)()
    if not isinstance(value, datetime) or value.utcoffset() is None:
        raise ValueError(f"clock: expected a timezone-aware datetime, got {_show(value)}")
    return value.astimezone(timezone.utc)


def _require(board, name):
    """Returns board[name]; raises ValueError starting with name when the field is absent."""
    if name not in board:
        raise ValueError(f"{name}: missing from the board")
    return board[name]


def _check_row(index, row):
    """Validates standings[index] and returns its 'at' as an aware UTC datetime."""
    where = f"standings[{index}]"
    if not isinstance(row, dict):
        raise ValueError(f"{where}: expected an object, got {_show(row)}")
    rank = row.get("rank")
    if type(rank) is not int or rank < 1:
        raise ValueError(f"{where}.rank: expected an integer of at least 1, got {_show(rank)}")
    if not isinstance(row.get("team"), str):
        raise ValueError(f"{where}.team: expected a string, got {_show(row.get('team'))}")
    if not _positive_number(row.get("score")):
        raise ValueError(f"{where}.score: expected a finite number above 0, got {_show(row.get('score'))}")
    return parse_instant(row.get("at"), f"{where}.at")


def _gap(ours, other):
    """Returns ours minus other rounded to 3 decimals, or None when ours is None."""
    if ours is None:
        return None
    return round(ours - other, 3) + 0.0


def summarize(board, team=TEAM, margin=DEFAULT_MARGIN):
    """Validates a standings board and derives the leader, the target and our position.

    The board must have key 'shopping-sessions', direction 'lower', metric
    'Run time' and unit 's'. Every standings row needs an integer rank of at
    least 1, a string team, a finite score above 0 and an ISO 'at' with a
    timezone. Any other value raises ValueError starting with the field name.

    The leader is the best other team: lowest score, and on equal scores the
    earlier 'at'. The reference is the leader's score, or baseline_score when
    no other team is on the board. target_score is the reference times
    (1 - margin), rounded to 3 decimals. The gaps are our score minus the
    reference and minus the target, None without our entry. beats_leader means
    a lower score than the reference, or an equal score submitted before the
    leader's. target_met means beats_leader and a score at or below the target.
    top5 lists the five best rows in the same order as the leader choice.
    """
    if not isinstance(team, str) or not team:
        raise ValueError(f"team: expected a non-empty string, got {_show(team)}")
    if isinstance(margin, bool) or not isinstance(margin, (int, float)) or not 0 <= margin < 1:
        raise ValueError(f"margin: expected a number from 0 up to but not including 1, got {_show(margin)}")
    if not isinstance(board, dict):
        raise ValueError(f"board: expected a JSON object, got {type(board).__name__}")
    for name, expected in BOARD_IDENTITY:
        value = _require(board, name)
        if value != expected:
            raise ValueError(f"{name}: expected {expected!r}, got {_show(value)}")
    flags = {}
    for name in ("closed", "paused"):
        flags[name] = _require(board, name)
        if not isinstance(flags[name], bool):
            raise ValueError(f"{name}: expected true or false, got {_show(flags[name])}")
    cutoff_at = _require(board, "cutoff_at")
    parse_instant(cutoff_at, "cutoff_at")
    counts = {}
    for name in ("team_count", "practice_limit"):
        counts[name] = _require(board, name)
        if type(counts[name]) is not int or counts[name] < 0:
            raise ValueError(f"{name}: expected a non-negative integer, got {_show(counts[name])}")
    scorer_version = _require(board, "scorer_version")
    if not isinstance(scorer_version, str) or not scorer_version:
        raise ValueError(f"scorer_version: expected a non-empty string, got {_show(scorer_version)}")
    baseline = _require(board, "baseline_score")
    if not _positive_number(baseline):
        raise ValueError(f"baseline_score: expected a finite number above 0, got {_show(baseline)}")
    rows = _require(board, "standings")
    if not isinstance(rows, list):
        raise ValueError(f"standings: expected a list, got {type(rows).__name__}")

    entries = []
    for index, row in enumerate(rows):
        at = _check_row(index, row)
        entries.append((row["score"], at, row["rank"], index, row))
    entries.sort(key=lambda entry: entry[:4])
    ours = [entry for entry in entries if entry[4]["team"] == team]
    if len(ours) > 1:
        raise ValueError(f"standings: team {_show(team)} appears {len(ours)} times")
    others = [entry for entry in entries if entry[4]["team"] != team]
    own = ours[0] if ours else None
    leader = others[0] if others else None
    reference = leader[0] if leader else baseline
    target = float(round(reference * (1 - margin), 3))
    our_score = own[0] if own else None
    if own is None:
        beats = False
    elif leader is None:
        beats = our_score < reference
    else:
        beats = our_score < leader[0] or (our_score == leader[0] and own[1] < leader[1])
    return {
        "team": team,
        "margin": margin,
        "metric": "Run time",
        "unit": "s",
        "direction": "lower",
        "leader_team": leader[4]["team"] if leader else None,
        "leader_score": leader[0] if leader else None,
        "leader_at": leader[4]["at"] if leader else None,
        "leader_rank": leader[2] if leader else None,
        "reference": "leader" if leader else "baseline",
        "reference_score": reference,
        "target_score": target,
        "our_score": our_score,
        "our_rank": own[2] if own else None,
        "our_at": own[4]["at"] if own else None,
        "gap_to_leader_s": _gap(our_score, reference),
        "gap_to_target_s": _gap(our_score, target),
        "beats_leader": beats,
        "target_met": own is not None and our_score <= target and beats,
        "team_count": counts["team_count"],
        "practice_limit": counts["practice_limit"],
        "cutoff_at": cutoff_at,
        "closed": flags["closed"],
        "paused": flags["paused"],
        "scorer_version": scorer_version,
        "baseline_score": baseline,
        "top5": [{"rank": entry[2], "team": entry[4]["team"], "score": entry[0], "at": entry[4]["at"]}
                 for entry in entries[:TOP_COUNT]],
    }


def fetch_board(url=URL, timeout=FETCH_TIMEOUT_S, limit=MAX_RESPONSE_BYTES, opener=None, clock=time.monotonic):
    """Sends one GET to url and returns the body decoded as UTF-8.

    Sends Accept: application/json and Cache-Control: no-cache and refuses a
    body larger than limit bytes. Each socket operation waits at most timeout
    seconds. The body is read one socket read at a time, and reading stops with
    TimeoutError once timeout seconds have passed since the request began, so a
    body that trickles in ends the call at most one socket wait after that
    point. opener defaults to urllib.request.urlopen and must return a response
    with read1. Raises OSError (TimeoutError included) or ValueError
    (UnicodeDecodeError included) on failure.
    """
    request = urllib.request.Request(url, headers={"Accept": "application/json", "Cache-Control": "no-cache"})
    deadline = clock() + timeout
    chunks, size = [], 0
    with (opener or urllib.request.urlopen)(request, timeout=timeout) as response:
        while True:
            if clock() > deadline:
                raise TimeoutError(f"response took longer than {timeout:g} s")
            chunk = response.read1(65536)
            if not chunk:
                break
            size += len(chunk)
            if size > limit:
                raise ValueError(f"response is larger than {limit} bytes")
            chunks.append(chunk)
    return b"".join(chunks).decode("utf-8")


def poll(previous=None, fetch=None, now=None, team=TEAM, margin=DEFAULT_MARGIN, url=URL):
    """Reads the board once and returns a fresh state, or the previous state marked stale.

    fetch(url) returns the response body as text (default: fetch_board). now()
    returns an aware datetime (default: the system clock). A fresh state is the
    summarize() result plus status 'fresh', checked_at, last_success_at,
    source and error None. On any failure the result is previous's values with
    status 'stale', a new checked_at and the error text, while last_success_at
    keeps its old value or stays absent. Never raises an Exception.
    """
    base = dict(previous) if isinstance(previous, dict) else {}
    for name, value in (("team", team), ("margin", margin), ("source", url)):
        base.setdefault(name, value)
    try:
        checked_at = _clock(now).isoformat()
    except Exception as error:
        return dict(base, status="stale", checked_at=None, error=_describe_error(error))
    try:
        text = (fetch or fetch_board)(url)
        if not isinstance(text, str):
            raise TypeError(f"fetch: expected text, got {type(text).__name__}")
        summary = summarize(json.loads(text), team, margin)
    except Exception as error:
        return dict(base, status="stale", checked_at=checked_at, error=_describe_error(error))
    return dict(summary, status="fresh", checked_at=checked_at, last_success_at=checked_at, source=url, error=None)


def _try_lock(descriptor):
    """Takes an exclusive lock on byte 0 of descriptor without waiting; raises OSError when it is held."""
    if os.name == "nt":
        os.lseek(descriptor, 0, os.SEEK_SET)
        msvcrt.locking(descriptor, msvcrt.LK_NBLCK, 1)
    else:
        fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)


def _unlock(descriptor):
    """Releases the lock taken by _try_lock."""
    if os.name == "nt":
        os.lseek(descriptor, 0, os.SEEK_SET)
        msvcrt.locking(descriptor, msvcrt.LK_UNLCK, 1)
    else:
        fcntl.flock(descriptor, fcntl.LOCK_UN)


@contextlib.contextmanager
def _exclusive(lock_path, timeout_s=LOCK_TIMEOUT_S):
    """Holds an exclusive cross-process lock on lock_path for the body of the with block.

    Raises TimeoutError naming the path when another holder keeps the lock for
    more than timeout_s seconds.
    """
    descriptor = os.open(str(lock_path), os.O_RDWR | os.O_CREAT, 0o644)
    try:
        deadline = time.monotonic() + timeout_s
        while True:
            try:
                _try_lock(descriptor)
                break
            except OSError as error:
                if error.errno not in _LOCK_BUSY:
                    raise
                if time.monotonic() >= deadline:
                    raise TimeoutError(f"lock {lock_path} still held after {timeout_s:g} s") from error
                time.sleep(0.05)
        try:
            yield
        finally:
            _unlock(descriptor)
    finally:
        os.close(descriptor)


def _read_text(path, attempts=40, delay=0.05):
    """Reads a UTF-8 file, retrying while a concurrent replace briefly denies access on Windows."""
    for attempt in range(attempts):
        try:
            with open(path, encoding="utf-8") as handle:
                return handle.read()
        except PermissionError:
            if attempt == attempts - 1:
                raise
            time.sleep(delay)


def _replace(source, target, attempts=40, delay=0.05):
    """Runs os.replace, retrying while another process holds target open on Windows."""
    for attempt in range(attempts):
        try:
            os.replace(source, target)
            return
        except PermissionError:
            if attempt == attempts - 1:
                raise
            time.sleep(delay)


def _write_atomic(path, text):
    """Writes text to path through a fsynced temporary file in the same folder and os.replace."""
    descriptor, temporary = tempfile.mkstemp(prefix=path.name + ".", suffix=".tmp", dir=str(path.parent))
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8", newline="\n") as handle:
            handle.write(text)
            handle.flush()
            os.fsync(handle.fileno())
        _replace(temporary, path)
    except BaseException:
        with contextlib.suppress(FileNotFoundError):
            os.unlink(temporary)
        raise


def _stored(path):
    """Returns the JSON object stored at path, or {} when the file is missing, unreadable or not an object."""
    try:
        value = json.loads(_read_text(path))
    except (OSError, ValueError):
        return {}
    return value if isinstance(value, dict) else {}


def _output_problem(path):
    """Returns why path cannot hold the watcher state, or None when it can.

    The path must name a file, must not be an existing directory and must not
    end in .jsonl in any letter case, since the history file takes that suffix.
    """
    if path.name in ("", ".", ".."):
        return f"{_show(str(path))} names no file"
    if path.suffix.lower() == ".jsonl":
        return f"{_show(str(path))} ends in .jsonl, the suffix of the history file"
    if path.is_dir():
        return f"{_show(str(path))} is a directory"
    return None


def record(state, path):
    """Writes state to path atomically and appends it to the sibling history when it changed.

    The history file is path with the suffix .jsonl. It gets one line when any
    of CHANGE_FIELDS differs from the state stored at path before this call; a
    missing or unreadable stored state counts as a change. When the history
    ends in a line without its newline, a newline goes first so the new line
    stays whole. A lock file next to path serializes writers. Returns True when
    the history grew. Raises ValueError before writing anything when
    _output_problem rejects path or when state is not a dict of JSON values
    without NaN or infinity.
    """
    path = Path(path)
    problem = _output_problem(path)
    if problem:
        raise ValueError(f"output: {problem}")
    history = path.with_suffix(".jsonl")
    if not isinstance(state, dict):
        raise ValueError(f"state: expected a dict, got {type(state).__name__}")
    try:
        document = json.dumps(state, indent=2, sort_keys=True, allow_nan=False) + "\n"
        line = json.dumps(state, sort_keys=True, allow_nan=False) + "\n"
    except (TypeError, ValueError) as error:
        raise ValueError(f"state: not serializable as strict JSON: {error}") from None
    path.parent.mkdir(parents=True, exist_ok=True)
    with _exclusive(path.with_name(path.name + ".lock")):
        old = _stored(path)
        changed = any(old.get(name) != state.get(name) for name in CHANGE_FIELDS)
        if changed:
            with open(history, "a+b") as handle:
                if handle.seek(0, os.SEEK_END) > 0:
                    handle.seek(-1, os.SEEK_END)
                    if handle.read(1) != b"\n":
                        line = "\n" + line
                handle.write(line.encode("utf-8"))
                handle.flush()
                os.fsync(handle.fileno())
        _write_atomic(path, document)
    return changed


def read_fresh(path, max_age_s=FRESH_MAX_AGE_S, now=None):
    """Returns the watcher state stored at path while it is fresh.

    The state must have status 'fresh' and a last_success_at that is at most
    max_age_s seconds before now() and not after it. Otherwise raises
    ValueError saying why, including for a missing or unreadable file.
    """
    if isinstance(max_age_s, bool) or not isinstance(max_age_s, (int, float)) or not 0 <= max_age_s < math.inf:
        raise ValueError(f"max_age_s: expected a finite number of at least 0, got {_show(max_age_s)}")
    path = Path(path)
    try:
        text = _read_text(path)
    except FileNotFoundError:
        raise ValueError(f"no watcher state at {path}") from None
    except OSError as error:
        raise ValueError(f"cannot read watcher state at {path}: {error}") from None
    try:
        state = json.loads(text)
    except ValueError as error:
        raise ValueError(f"watcher state at {path} is not valid JSON: {error}") from None
    if not isinstance(state, dict):
        raise ValueError(f"watcher state at {path} is not a JSON object")
    if state.get("status") != "fresh":
        raise ValueError(f"watcher state is {_show(state.get('status'))}, not 'fresh'; "
                         f"last error: {_show(state.get('error'), 300)}")
    last = parse_instant(state.get("last_success_at"), "last_success_at")
    age = (_clock(now) - last).total_seconds()
    if age < 0:
        raise ValueError(f"last_success_at {state['last_success_at']} is {_seconds(-age)} s in the future")
    if age > max_age_s:
        raise ValueError(f"last_success_at {state['last_success_at']} is {_seconds(age)} s old, "
                         f"over the {_seconds(max_age_s)} s limit")
    return state


def _seconds(value):
    """Formats a duration in seconds with up to 6 decimals and no trailing zeros."""
    return f"{value:.6f}".rstrip("0").rstrip(".") or "0"


def _number(value):
    """Formats a number with at most 3 decimals and no trailing zeros."""
    text = f"{value:.3f}".rstrip("0").rstrip(".")
    return "0" if text in ("", "-0") else text


def _signed(value):
    """Formats a number like _number with a leading + when it is above zero."""
    return ("+" if value > 0 else "") + _number(value)


def describe(state):
    """Returns one ASCII line: status and check time, leader, target, our entry and gaps."""
    parts = [f"{state.get('status', 'unknown')} at {state.get('checked_at') or 'unknown time'}"]
    if state.get("leader_score") is not None:
        parts.append(f"leader {_number(state['leader_score'])} s {state.get('leader_team')}")
    elif state.get("reference") == "baseline":
        parts.append(f"no other team, baseline {_number(state['reference_score'])} s")
    else:
        parts.append("leader unknown")
    target = state.get("target_score")
    parts.append(f"target {_number(target)} s" if target is not None else "target unknown")
    if state.get("our_score") is None:
        parts.append("ours: no entry" if "our_score" in state else "ours unknown")
        parts.append("gap n/a")
    else:
        parts.append(f"ours {_number(state['our_score'])} s rank {state.get('our_rank')}")
        parts.append(f"gap {_signed(state['gap_to_leader_s'])} s to {state.get('reference', 'leader')}, "
                     f"{_signed(state['gap_to_target_s'])} s to target")
    if state.get("status") != "fresh" and state.get("error"):
        parts.append(f"error: {state['error']}")
    return _ascii("; ".join(parts))


def _restore(path, team, margin):
    """Returns the state stored at path when it was written for this team, margin and source, else None.

    An unreadable or corrupt file produces a warning on stderr and None.
    """
    try:
        state = json.loads(_read_text(path))
    except FileNotFoundError:
        return None
    except (OSError, ValueError) as error:
        print(_ascii(f"warning: ignoring unreadable state {path}: {error}"), file=sys.stderr, flush=True)
        return None
    if (isinstance(state, dict) and state.get("team") == team and state.get("margin") == margin
            and state.get("source") == URL):
        return state
    return None


def _past_cutoff(state, now):
    """True when state carries a cutoff_at that the clock has reached."""
    cutoff = state.get("cutoff_at")
    return cutoff is not None and _clock(now) >= parse_instant(cutoff, "cutoff_at")


def _wait(seconds, stop, sleep):
    """Sleeps for seconds in steps of at most 1 s; returns True as soon as stop is set."""
    remaining = seconds
    while remaining > 0:
        if stop.is_set():
            return True
        step = min(1.0, remaining)
        sleep(step)
        remaining -= step
    return stop.is_set()


def _install_stop_handlers(stop):
    """Routes SIGINT, SIGTERM and SIGBREAK to stop.set() and returns a function that restores the old handlers.

    Installs nothing outside the main thread, where Python cannot set handlers.
    """
    saved = []
    if threading.current_thread() is threading.main_thread():
        for name in ("SIGINT", "SIGTERM", "SIGBREAK"):
            number = getattr(signal, name, None)
            if number is None:
                continue
            saved.append((number, signal.getsignal(number)))
            signal.signal(number, lambda signum, frame: stop.set())

    def restore():
        for number, handler in saved:
            signal.signal(number, handler if handler is not None else signal.SIG_DFL)
    return restore


def _parse_args(argv):
    """Parses and checks the command line; exits with status 2 and a message naming the bad option."""
    parser = argparse.ArgumentParser(
        description="Watch the public Shopping Sessions standings and keep a live run-time target.")
    parser.add_argument("--once", action="store_true", help="poll once, record, print the line and exit")
    parser.add_argument("--interval", type=float, default=DEFAULT_INTERVAL_S,
                        help=f"seconds between polls, at least {MIN_INTERVAL_S:g} (default {DEFAULT_INTERVAL_S:g})")
    parser.add_argument("--margin", type=float, default=DEFAULT_MARGIN,
                        help=f"fraction below the leader's score for the target, 0 or more and below 1 "
                             f"(default {DEFAULT_MARGIN:g})")
    parser.add_argument("--team", default=TEAM, help=f"exact team name on the board (default {TEAM})")
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT,
                        help="state file; the history goes next to it with the suffix .jsonl "
                             "(default artifacts/live-target.json in the repository)")
    args = parser.parse_args(argv)
    if not math.isfinite(args.interval) or args.interval < MIN_INTERVAL_S:
        parser.error(f"--interval: expected at least {MIN_INTERVAL_S:g} seconds, got {args.interval:g}")
    if not 0 <= args.margin < 1:
        parser.error(f"--margin: expected a number from 0 up to but not including 1, got {args.margin:g}")
    if not args.team:
        parser.error("--team: expected a non-empty team name")
    problem = _output_problem(args.output)
    if problem:
        parser.error(f"--output: {problem}")
    return args


def main(argv=None, fetch=None, now=None, sleep=time.sleep):
    """Runs the command line with an injectable fetch, clock and sleep; returns the exit status."""
    args = _parse_args(argv)
    stop = threading.Event()
    restore_handlers = _install_stop_handlers(stop)
    try:
        previous = _restore(args.output, args.team, args.margin)
        while True:
            state = poll(previous, fetch, now, args.team, args.margin)
            if record(state, args.output) or args.once:
                print(describe(state), flush=True)
            if args.once:
                return 0 if state.get("status") == "fresh" else 1
            previous = state
            if state.get("closed") is True:
                print("board closed; watcher stopped", flush=True)
                return 0
            if _past_cutoff(state, now):
                print(_ascii(f"cutoff {state['cutoff_at']} passed; watcher stopped"), flush=True)
                return 0
            if _wait(args.interval, stop, sleep):
                print("interrupted; watcher stopped", flush=True)
                return 130
    except KeyboardInterrupt:
        print("interrupted; watcher stopped", flush=True)
        return 130
    finally:
        restore_handlers()


if __name__ == "__main__":
    sys.exit(main())
