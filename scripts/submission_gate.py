#!/usr/bin/env python3
"""Journaled submission gate: decide whether a commit may be submitted, and keep the record.

The gate never submits, clicks, logs in or opens a network connection. The team
submits on the track page in a signed-in browser. This tool decides beforehand
whether a commit may be submitted, and it records every preparation,
reservation, organizer receipt and official result in an append-only journal.

Usage (global options come before the command):

  submission_gate.py [--root DIR] [--journal FILE] COMMAND ...

  prepare --checks FILE [--target FILE]
  attempt COMMIT --server-remaining N --server-checked-at ISO [--target FILE]
  receipt ATTEMPT_ID BB-XXXXXXXX --evidence URL
  import-receipt BB-XXXXXXXX --submitted-at ISO --evidence URL [--commit SHA]
  result BB-XXXXXXXX scored|rejected|failed [--score SECONDS] --evidence URL
  status

Every command prints one JSON document on stdout. Success exits 0. Any refusal
or unexpected error exits 2 and prints {"error": ..., "ready": false}. Every
command refuses a --root that is not the top of a git work tree.

Journal: JSON lines at <root>/artifacts/submissions/events.jsonl unless
--journal names another file. Every command holds one exclusive lock on
<journal>.lock (msvcrt on Windows, fcntl elsewhere) for its whole run, and every
appended event is flushed and fsynced before the command reports success. State
is derived from the journal alone. A corrupt or inconsistent journal refuses
every command until someone repairs it by hand.

Runtime fingerprint: sha256 over Dockerfile, .dockerignore, Cargo.toml,
Cargo.lock and then every file under src/ sorted by path, each fed as
name + NUL + bytes + NUL. The bytes are the blobs git stores for the commit, so
line-ending conversion on checkout cannot change the fingerprint. prepare and
attempt hash HEAD after requiring a clean working tree; import-receipt hashes
the named historical commit. A clean working tree has nothing in git status,
untracked files included, no tracked file flagged assume-unchanged or
skip-worktree, and no ignored file under the runtime paths.

Checks file (prepare): a JSON list of objects with the keys name (a unique
string), argv (a non-empty list of strings), timeout_s (a positive number of
seconds, at most 86400) and, optionally, env (an object of strings laid over
the current environment) and cwd (a directory; a relative path resolves
against the repository root). Every check must exit 0. The record keeps each
check's name, argv, exit code, elapsed seconds and its combined stdout and
stderr after sensitive header lines and secret-looking environment values are
removed, bounded to 16384 characters by keeping the head and the tail around a
[TRUNCATED] marker. A check that fails, times out or cannot start revokes the
earlier prepare record of that commit.

Live target: prepare and attempt read the watcher snapshot (--target, default
<root>/artifacts/live-target.json) through watch_leaderboard.read_fresh and
refuse unless it is fresh.
"""
import argparse
from contextlib import contextmanager
from datetime import datetime, timezone
import errno
import hashlib
import json
import math
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import time
import traceback
from urllib.parse import unquote, urlsplit
import uuid

if os.name == 'nt':
    import msvcrt
else:
    import fcntl

ROOT = Path(__file__).resolve().parents[1]
JOURNAL = Path('artifacts', 'submissions', 'events.jsonl')
TARGET = Path('artifacts', 'live-target.json')
RUNTIME_FILES = ('Dockerfile', '.dockerignore', 'Cargo.toml', 'Cargo.lock')
RUNTIME_DIR = 'src'
REGULAR_MODES = (b'100644', b'100755')
TERMINAL = ('scored', 'rejected', 'failed')
INFLIGHT = ('submitting', 'scoring')
OUTPUT_LIMIT = 16384
TRUNCATION_MARKER = '\n[TRUNCATED]\n'
OBSERVATION_MAX_AGE_S = 90
WINDOW_S = 3600
HOURLY_LIMIT = 2
DEFAULT_PRACTICE_LIMIT = 2
LOCK_TIMEOUT_S = 60.0
KILL_WAIT_S = 10
MAX_CHECK_TIMEOUT_S = 86400
GIT_REDIRECTS = ('GIT_DIR', 'GIT_WORK_TREE', 'GIT_INDEX_FILE', 'GIT_OBJECT_DIRECTORY',
                 'GIT_ALTERNATE_OBJECT_DIRECTORIES', 'GIT_COMMON_DIR', 'GIT_NAMESPACE',
                 'GIT_PREFIX')
EVIDENCE_HOST = 'www.joinbuilders.org'
EVIDENCE_ROOT = '/buildathon'
RECEIPT = re.compile(r'BB-[A-Z0-9]{8}')
COMMIT_SHA = re.compile(r'[0-9a-f]{40}')
FINGERPRINT = re.compile(r'[0-9a-f]{64}')
CHECK_KEYS = ('name', 'argv', 'timeout_s', 'env', 'cwd')
CHECK_REQUIRED = ('name', 'argv', 'timeout_s')
RECEIPT_CHANGE = frozenset(('state', 'receipt', 'receipt_evidence', 'receipt_observed_at'))
RESULT_CHANGE = frozenset(('state', 'score', 'result_evidence', 'result_observed_at'))
NO_WINDOW = getattr(subprocess, 'CREATE_NO_WINDOW', 0)
LOCK_BUSY = frozenset(getattr(errno, name) for name in
                      ('EACCES', 'EAGAIN', 'EWOULDBLOCK', 'EDEADLK', 'EDEADLOCK')
                      if hasattr(errno, name))
SENSITIVE_HEADER = re.compile(
    r'(?im)^[^\r\n]*\b(?:proxy-authorization|authorization|set-cookie|cookie|x-api-key)'
    r'["\']?\s*[:=][^\r\n]*(?:\r?\n[ \t]+[^\r\n]*)*')
SECRET_ASSIGNMENT = re.compile(
    r'(?i)((?:password|passwd|token|api[_-]?key|secret|access[_-]?key)["\']?\s*[:=]\s*)\S+')
URL_CREDENTIALS = re.compile(r'(?i)\b([a-z][a-z0-9+.-]*://)[^\s/@]+@')
ENCODED_SEPARATOR = re.compile(r'%(?:2f|5c)', re.IGNORECASE)
SECRET_NAME = re.compile(
    r'(?i)TOKEN|PASSWORD|PASSWD|SECRET|API_?KEY|PRIVATE_?KEY|ACCESS_?KEY|CREDENTIAL|COOKIE')
SECRET_MIN_LENGTH = 8
REDACTED = '[REDACTED]'


class GateError(Exception):
    """A refused action: unsafe, inconsistent or malformed input or state."""


def parse_instant(value, label):
    """Parse an ISO 8601 time that carries a timezone and return it in UTC.

    A trailing Z means UTC. Text without a timezone, or a value that is not a
    string, raises GateError naming LABEL and the value.
    """
    if not isinstance(value, str) or not value:
        raise GateError(f'{label} must be an ISO 8601 time with a timezone, got {value!r}')
    text = value[:-1] + '+00:00' if value[-1] in 'Zz' else value
    try:
        parsed = datetime.fromisoformat(text)
    except ValueError as error:
        raise GateError(f'{label} is not an ISO 8601 time: {value!r}') from error
    if parsed.tzinfo is None or parsed.utcoffset() is None:
        raise GateError(f'{label} must include a timezone offset, got {value!r}')
    return parsed.astimezone(timezone.utc)


def require_commit(value, label):
    """Return VALUE when it is a full 40-character lowercase hex SHA; raise GateError otherwise."""
    if not isinstance(value, str) or not COMMIT_SHA.fullmatch(value):
        raise GateError(f'{label} must be a full 40-character lowercase commit SHA, got {value!r}')
    return value


def require_fingerprint(value, label):
    """Return VALUE when it is a 64-character sha256 hex digest; raise GateError otherwise."""
    if not isinstance(value, str) or not FINGERPRINT.fullmatch(value):
        raise GateError(f'{label} must be a 64-character sha256 hex digest, got {value!r}')
    return value


def require_receipt(value):
    """Return VALUE when it is an organizer receipt BB-XXXXXXXX; raise GateError otherwise."""
    if not isinstance(value, str) or not RECEIPT.fullmatch(value):
        raise GateError(
            f'receipt must be BB- followed by 8 uppercase letters or digits, got {value!r}')
    return value


def require_evidence(value):
    """Return VALUE when it is an https URL on the organizer site under /buildathon.

    The host must be exactly www.joinbuilders.org with no port or credentials,
    the path must be /buildathon or below it without dot segments, and the URL
    must carry no query, fragment, backslash, percent-encoded slash or
    backslash, whitespace, control or non-ASCII characters.
    """
    problem = None
    if (not isinstance(value, str) or not value.isascii()
            or any(c.isspace() or ord(c) < 32 or ord(c) == 127 for c in value)):
        problem = 'not a plain ASCII URL'
    elif '?' in value or '#' in value:
        problem = 'it has a query or a fragment'
    elif '\\' in value or ENCODED_SEPARATOR.search(value):
        problem = 'it has a backslash or a percent-encoded slash or backslash'
    else:
        parts = urlsplit(value)
        if parts.scheme != 'https':
            problem = 'the scheme is not https'
        elif parts.netloc != EVIDENCE_HOST:
            problem = f'the host is not exactly {EVIDENCE_HOST}'
        elif not (parts.path == EVIDENCE_ROOT or parts.path.startswith(EVIDENCE_ROOT + '/')):
            problem = f'the path is not under {EVIDENCE_ROOT}'
        elif any(unquote(segment) in ('.', '..') for segment in parts.path.split('/')):
            problem = 'the path has dot segments'
    if problem:
        raise GateError(f'--evidence must be an https URL on {EVIDENCE_HOST} under {EVIDENCE_ROOT} '
                        f'with no query or fragment ({problem}), got {value!r}')
    return value


def _finite_number(value):
    """Return True for an int or float, never a bool, whose value converts to a finite float."""
    if type(value) not in (int, float):
        return False
    try:
        return math.isfinite(value)
    except OverflowError:
        return False


def require_outcome(outcome, score):
    """Refuse an unknown outcome, a scored result without a finite score above 0, or a score on any other outcome."""
    if outcome not in TERMINAL:
        raise GateError(f'outcome must be one of {", ".join(TERMINAL)}, got {outcome!r}')
    if outcome == 'scored':
        if not _finite_number(score) or score <= 0:
            raise GateError(f'a scored result needs a finite --score above 0 seconds, got {score!r}')
    elif score is not None:
        raise GateError(f'a {outcome} result takes no --score, got {score!r}')


def redact(text, environ):
    """Remove sensitive header lines, secret-looking environment values and inline credentials.

    Header lines naming authorization, proxy-authorization, cookie, set-cookie
    or x-api-key are replaced whole, folded continuation lines included, also
    when the name is quoted as in JSON or a printed dict. The value of every
    environment variable whose name looks secret (8 characters or longer) is
    replaced wherever it appears, as are values written as password=, token:,
    "api_key": and similar, and the user or user:password part of URLs.
    """
    text = SENSITIVE_HEADER.sub(REDACTED, text)
    secrets = sorted({value for name, value in environ.items()
                      if SECRET_NAME.search(name) and len(value) >= SECRET_MIN_LENGTH},
                     key=len, reverse=True)
    for secret in secrets:
        text = text.replace(secret, REDACTED)
    text = SECRET_ASSIGNMENT.sub(lambda match: match.group(1) + REDACTED, text)
    return URL_CREDENTIALS.sub(lambda match: match.group(1) + REDACTED + '@', text)


def bound_output(text):
    """Return TEXT when it fits OUTPUT_LIMIT characters, else its head and tail around [TRUNCATED]."""
    if len(text) <= OUTPUT_LIMIT:
        return text
    keep = (OUTPUT_LIMIT - len(TRUNCATION_MARKER)) // 2
    return text[:keep] + TRUNCATION_MARKER + text[-keep:]


def _reject_constant(name):
    """Refuse NaN and Infinity while parsing JSON."""
    raise ValueError(f'non-finite number {name} is not allowed')


def _unique_keys(pairs):
    """Build a JSON object, refusing a key that appears twice."""
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f'duplicate key {key!r}')
        result[key] = value
    return result


def _strict_json(text):
    """Parse JSON text, refusing NaN, Infinity, repeated object keys and nesting too deep to parse."""
    try:
        return json.loads(text, parse_constant=_reject_constant, object_pairs_hook=_unique_keys)
    except RecursionError as error:
        raise ValueError('JSON nests too deeply to parse') from error


def _preview(value):
    """Return a short repr of VALUE for error messages."""
    text = repr(value)
    return text if len(text) <= 200 else text[:200] + '...'


def _require_check_records(value, label):
    """Raise ValueError naming LABEL unless VALUE is a list of check record objects."""
    if not isinstance(value, list) or not all(isinstance(item, dict) for item in value):
        raise ValueError(f'{label} must be a list of check records, got {_preview(value)}')


def _lock(descriptor):
    """Try once to take the exclusive lock; raise OSError when another process holds it."""
    if os.name == 'nt':
        os.lseek(descriptor, 0, os.SEEK_SET)
        msvcrt.locking(descriptor, msvcrt.LK_NBLCK, 1)
    else:
        fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)


def _unlock(descriptor):
    """Release the exclusive lock taken by _lock."""
    if os.name == 'nt':
        os.lseek(descriptor, 0, os.SEEK_SET)
        msvcrt.locking(descriptor, msvcrt.LK_UNLCK, 1)
    else:
        fcntl.flock(descriptor, fcntl.LOCK_UN)


@contextmanager
def journal_lock(path, timeout_s=LOCK_TIMEOUT_S):
    """Hold an exclusive lock on PATH (created when missing) for the duration of the block.

    Windows locks the first byte with msvcrt.locking; POSIX uses fcntl.flock.
    The lock is retried every 50 ms; after timeout_s seconds GateError names
    the lock file.
    """
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor = os.open(path, os.O_RDWR | os.O_CREAT | getattr(os, 'O_BINARY', 0), 0o600)
    try:
        deadline = time.monotonic() + timeout_s
        while True:
            try:
                _lock(descriptor)
                break
            except OSError as error:
                if error.errno not in LOCK_BUSY:
                    raise
                if time.monotonic() >= deadline:
                    raise GateError(f'journal lock {path} is held by another process '
                                    f'(waited {timeout_s:g} s)') from error
                time.sleep(0.05)
        try:
            yield
        finally:
            _unlock(descriptor)
    finally:
        os.close(descriptor)


def load_checks(path, root):
    """Read and validate a checks file; return (checks, sha256 hex digest of the file bytes).

    Each returned check holds name, argv, timeout_s, env (empty when absent)
    and cwd (a Path; the repository root when absent). A malformed file or
    entry raises GateError naming the entry and the bad value.
    """
    path = Path(path)
    try:
        raw = path.read_bytes()
    except OSError as error:
        raise GateError(f'cannot read checks file {path}: {error}') from error
    try:
        checks = _strict_json(raw.decode('utf-8'))
    except (UnicodeDecodeError, ValueError) as error:
        raise GateError(f'checks file {path} is not valid JSON: {error}') from error
    if not isinstance(checks, list) or not checks:
        raise GateError(f'checks file {path} must hold a non-empty JSON list of checks, '
                        f'got {_preview(checks)}')
    names, result = set(), []
    for index, check in enumerate(checks):
        where = f'checks file {path} entry {index}'
        if not isinstance(check, dict):
            raise GateError(f'{where} must be an object, got {_preview(check)}')
        unknown = sorted(set(check) - set(CHECK_KEYS))
        if unknown:
            raise GateError(f'{where} has unknown keys {unknown}')
        missing = [key for key in CHECK_REQUIRED if key not in check]
        if missing:
            raise GateError(f'{where} lacks the required keys {missing}')
        name, argv, timeout_s = check['name'], check['argv'], check['timeout_s']
        if not isinstance(name, str) or not name.strip():
            raise GateError(f'{where} name must be a non-empty string, got {name!r}')
        if name in names:
            raise GateError(f'{where} repeats the check name {name!r}')
        if (not isinstance(argv, list) or not argv
                or not all(isinstance(item, str) for item in argv) or not argv[0]):
            raise GateError(f'{where} argv must be a non-empty list of strings, got {_preview(argv)}')
        if not _finite_number(timeout_s) or not 0 < timeout_s <= MAX_CHECK_TIMEOUT_S:
            raise GateError(f'{where} timeout_s must be a positive number of seconds up to '
                            f'{MAX_CHECK_TIMEOUT_S}, got {_preview(timeout_s)}')
        env = check.get('env', {})
        if not isinstance(env, dict):
            raise GateError(f'{where} env must be an object of string values, got a '
                            f'{type(env).__name__}')
        bad = sorted(key for key, value in env.items()
                     if not key or '=' in key or not isinstance(value, str))
        if bad:
            raise GateError(f'{where} env must map names without "=" to string values; '
                            f'bad keys {bad}')
        if 'cwd' in check:
            cwd = check['cwd']
            if not isinstance(cwd, str) or not cwd:
                raise GateError(f'{where} cwd must be a non-empty string, got {cwd!r}')
            directory = Path(cwd) if Path(cwd).is_absolute() else Path(root) / cwd
        else:
            directory = Path(root)
        if not directory.is_dir():
            raise GateError(f'{where} cwd {directory} is not a directory')
        names.add(name)
        result.append(dict(name=name, argv=list(argv), timeout_s=timeout_s, env=dict(env),
                           cwd=directory))
    return result, hashlib.sha256(raw).hexdigest()


def kill_tree(process):
    """Kill PROCESS and its descendants: taskkill /T on Windows, the process group elsewhere."""
    if os.name == 'nt':
        try:
            subprocess.run(['taskkill', '/T', '/F', '/PID', str(process.pid)],
                           stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                           stderr=subprocess.DEVNULL, creationflags=NO_WINDOW, check=False)
        except OSError:
            pass
    else:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except (ProcessLookupError, PermissionError):
            pass
    try:
        process.kill()
    except OSError:
        pass


def run_check(check):
    """Run one validated check and return its record with redacted, bounded output.

    stdout and stderr are captured together and decoded as UTF-8 with
    replacement. On timeout the whole process tree is killed and the record
    says timed_out. A program that cannot start raises GateError.
    """
    environ = dict(os.environ)
    environ.update(check['env'])
    started = time.monotonic()
    try:
        process = subprocess.Popen(
            check['argv'], cwd=check['cwd'], env=environ, stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, encoding='utf-8',
            errors='replace', creationflags=NO_WINDOW, start_new_session=os.name != 'nt')
    except (OSError, ValueError) as error:
        raise GateError(f"check {check['name']!r} could not start {check['argv'][0]!r}: "
                        f'{error}') from error
    timed_out = False
    try:
        try:
            output, _ = process.communicate(timeout=check['timeout_s'])
        except subprocess.TimeoutExpired:
            timed_out = True
            kill_tree(process)
            try:
                output, _ = process.communicate(timeout=KILL_WAIT_S)
            except subprocess.TimeoutExpired:
                output = ('[output unavailable: a descendant process kept the output pipe '
                          'open after the timeout]')
    except BaseException:
        kill_tree(process)
        raise
    elapsed = time.monotonic() - started
    text = redact(output or '', environ)
    return dict(name=check['name'], argv=[redact(item, environ) for item in check['argv']],
                cwd=str(check['cwd']), env_keys=sorted(check['env']),
                timeout_s=check['timeout_s'], returncode=process.poll(), timed_out=timed_out,
                elapsed_s=round(elapsed, 3), output=bound_output(text),
                output_characters=len(text), output_truncated=len(text) > OUTPUT_LIMIT)


def read_live_target(path, now):
    """Return the watcher snapshot at PATH when watch_leaderboard.read_fresh accepts it at NOW.

    NOW is an aware datetime; read_fresh receives it as its clock. Raises
    GateError when the module cannot be imported or the snapshot is missing,
    malformed or not fresh.
    """
    try:
        from watch_leaderboard import read_fresh
    except ImportError as error:
        raise GateError('cannot import read_fresh from watch_leaderboard.py next to this '
                        f'script: {error}') from error
    try:
        return read_fresh(path, now=lambda: now)
    except (OSError, ValueError, KeyError, TypeError, RecursionError) as error:
        raise GateError(f'live target {path} is not fresh: {error}') from error


def practice_limit(target):
    """Return (limit, source) from the live target's practice_limit, or the default 2 when absent.

    A present value that is not a positive integer raises GateError.
    """
    if 'practice_limit' not in target:
        return DEFAULT_PRACTICE_LIMIT, 'default: the live target has no practice_limit'
    value = target['practice_limit']
    if type(value) is not int or value < 1:
        raise GateError(f'live target practice_limit must be a positive integer, got {value!r}')
    return value, 'live target'


def _check_summary(check):
    """Return the short form of a check record used by status."""
    return {key: check.get(key) for key in ('name', 'returncode', 'timed_out', 'elapsed_s')}


def _attempt_summary(row):
    """Return an attempt record without its live target snapshot."""
    return {key: value for key, value in row.items() if key != 'target'}


class SubmissionGate:
    """Decisions and the durable record for one repository checkout.

    root: the top of the git work tree of the scored repository.
    journal: JSON lines file; default <root>/artifacts/submissions/events.jsonl.
    now: callable returning the current timezone-aware datetime.
    target_reader: callable (path, now) returning the fresh live target as a
        dict or raising GateError; default read_live_target.
    lock_timeout_s: seconds to wait for the journal lock before refusing.
    """

    def __init__(self, root=ROOT, journal=None, now=None, target_reader=None,
                 lock_timeout_s=LOCK_TIMEOUT_S):
        self.root = Path(root).resolve()
        self.journal = Path(journal).resolve() if journal is not None else self.root / JOURNAL
        self.lock_path = self.journal.with_name(self.journal.name + '.lock')
        self._clock = now or (lambda: datetime.now(timezone.utc))
        self._target_reader = target_reader or read_live_target
        self.lock_timeout_s = lock_timeout_s

    def _now(self):
        """Return the clock's current time in UTC; refuse a clock without a timezone."""
        value = self._clock()
        if not isinstance(value, datetime) or value.tzinfo is None or value.utcoffset() is None:
            raise GateError(f'the clock returned {value!r}, not a timezone-aware datetime')
        return value.astimezone(timezone.utc)

    @contextmanager
    def _locked(self):
        """Refuse a root that is not the top of a git work tree, then hold the journal lock."""
        self._require_toplevel()
        with journal_lock(self.lock_path, self.lock_timeout_s):
            yield

    def _events(self):
        """Return [(line number, event)] from the journal; refuse a corrupt journal."""
        try:
            data = self.journal.read_bytes()
        except FileNotFoundError:
            return []
        except OSError as error:
            raise GateError(f'journal {self.journal} is unreadable: {error}') from error
        if not data:
            return []
        if not data.endswith(b'\n'):
            raise GateError(f'journal {self.journal} is corrupt: its last line is incomplete; '
                            'every command is refused until it is repaired by hand')
        events = []
        for number, line in enumerate(data[:-1].split(b'\n'), 1):
            try:
                event = _strict_json(line.decode('ascii'))
            except (UnicodeDecodeError, ValueError) as error:
                raise GateError(f'journal {self.journal} is corrupt at line {number}: {error}; '
                                'every command is refused until it is repaired by hand') from error
            if not isinstance(event, dict):
                raise GateError(f'journal {self.journal} is corrupt at line {number}: '
                                'not a JSON object; every command is refused until it is '
                                'repaired by hand')
            events.append((number, event))
        return events

    def _state(self, now):
        """Derive candidates, attempts, failures and limits from the journal alone."""
        candidates, attempts, receipts, failures = {}, {}, {}, []
        floor = None
        for number, event in self._events():
            try:
                floor = self._apply(event, candidates, attempts, receipts, failures, floor)
            except (GateError, KeyError, TypeError, ValueError, AttributeError) as error:
                raise GateError(f'journal {self.journal} is inconsistent at line {number}: '
                                f'{type(error).__name__}: {error}; every command is refused '
                                'until it is repaired by hand') from error
        rows = list(attempts.values())
        inflight = next((row for row in rows if row['state'] in INFLIGHT), None)
        recent = [row for row in rows
                  if (now - parse_instant(row['submitted_at'], 'submitted_at')).total_seconds()
                  <= WINDOW_S]
        scored = [(row['score'], parse_instant(row['submitted_at'], 'submitted_at'), index)
                  for index, row in enumerate(rows) if row['state'] == 'scored']
        best = rows[min(scored)[2]] if scored else None
        return dict(candidates=candidates, attempts=rows, by_id=attempts, by_receipt=receipts,
                    failures=failures, floor=floor, inflight=inflight, recent=recent, best=best)

    @staticmethod
    def _apply(event, candidates, attempts, receipts, failures, floor):
        """Apply one journal event to the derived state; raise on any inconsistency."""
        action = event['action']
        at = parse_instant(event['at'], 'at')
        if action == 'prepare':
            candidate = event['candidate']
            require_commit(candidate['commit'], 'prepared commit')
            require_fingerprint(candidate['fingerprint'], 'prepared fingerprint')
            _require_check_records(candidate['checks'], 'prepared checks')
            candidates[candidate['commit']] = candidate
        elif action == 'gate_failed':
            commit, revoked = event['commit'], event['revoked']
            if commit is not None:
                require_commit(commit, 'failed commit')
            if type(revoked) is not bool or (revoked and commit is None):
                raise ValueError(f'gate failure has revoked={revoked!r} for commit {commit!r}')
            _require_check_records(event['checks'], 'gate failure checks')
            if revoked:
                candidates.pop(commit, None)
            failures.append(event)
        elif action in ('attempt', 'import_receipt'):
            row = dict(event['attempt'])
            attempt_id = row['attempt_id']
            if not isinstance(attempt_id, str) or not attempt_id or attempt_id in attempts:
                raise ValueError(f'attempt id {attempt_id!r} is empty or repeated')
            busy = next((other for other in attempts.values() if other['state'] in INFLIGHT), None)
            if busy is not None:
                raise ValueError(f'{attempt_id} was recorded while {busy["attempt_id"]} '
                                 'was inflight')
            parse_instant(row['submitted_at'], 'submitted_at')
            if row['score'] is not None:
                raise ValueError(f'{attempt_id} starts with a score')
            if action == 'attempt':
                candidate = candidates.get(row['commit'])
                if row['state'] != 'submitting' or row['receipt'] is not None:
                    raise ValueError(f'{attempt_id} starts in state {row["state"]!r}')
                if candidate is None or candidate['fingerprint'] != row['fingerprint']:
                    raise ValueError(f'{attempt_id} has no matching prepare record')
            else:
                if row['state'] != 'scoring':
                    raise ValueError(f'{attempt_id} starts in state {row["state"]!r}')
                receipt = require_receipt(row['receipt'])
                if receipt in receipts:
                    raise ValueError(f'receipt {receipt} is recorded twice')
                require_evidence(row['receipt_evidence'])
                if row['commit'] is None:
                    if row['fingerprint'] is not None:
                        raise ValueError(f'{attempt_id} has a fingerprint without a commit')
                else:
                    require_commit(row['commit'], 'imported commit')
                    require_fingerprint(row['fingerprint'], 'imported fingerprint')
                receipts[receipt] = attempt_id
            attempts[attempt_id] = row
            floor = at if floor is None else max(floor, at)
        elif action == 'receipt':
            row = attempts[event['attempt_id']]
            change = event['change']
            if set(change) != RECEIPT_CHANGE:
                raise ValueError(f'receipt change has the keys {sorted(change)}')
            if row['state'] != 'submitting' or change['state'] != 'scoring':
                raise ValueError(f'receipt for {row["attempt_id"]} in state {row["state"]!r}')
            require_evidence(change['receipt_evidence'])
            receipt = require_receipt(change['receipt'])
            if receipt in receipts:
                raise ValueError(f'receipt {receipt} is recorded twice')
            row.update(change)
            receipts[receipt] = row['attempt_id']
        elif action == 'result':
            row = attempts[event['attempt_id']]
            change = event['change']
            if set(change) != RESULT_CHANGE:
                raise ValueError(f'result change has the keys {sorted(change)}')
            if row['state'] != 'scoring':
                raise ValueError(f'result for {row["attempt_id"]} in state {row["state"]!r}')
            require_outcome(change['state'], change['score'])
            require_evidence(change['result_evidence'])
            row.update(change)
            floor = at if floor is None else max(floor, at)
        else:
            raise ValueError(f'unknown action {action!r}')
        return floor

    def _append(self, action, at, **payload):
        """Append one event as an ASCII JSON line, then flush and fsync it."""
        event = dict(action=action, at=at.isoformat(), pid=os.getpid(), **payload)
        line = (json.dumps(event, allow_nan=False, sort_keys=True) + '\n').encode('ascii')
        self.journal.parent.mkdir(parents=True, exist_ok=True)
        created = not self.journal.exists()
        with open(self.journal, 'ab') as handle:
            handle.write(line)
            handle.flush()
            os.fsync(handle.fileno())
        if created and os.name != 'nt':
            descriptor = os.open(self.journal.parent, os.O_RDONLY)
            try:
                os.fsync(descriptor)
            finally:
                os.close(descriptor)
        return event

    def _git(self, *args, stdin=None):
        """Run git in the repository and return its stdout bytes; refuse on a non-zero exit.

        Variables that would point git at another repository or index (GIT_DIR,
        GIT_WORK_TREE, GIT_INDEX_FILE and similar) are removed, so git always
        works on the repository found from --root.
        """
        options = dict(input=stdin) if stdin is not None else dict(stdin=subprocess.DEVNULL)
        environ = {name: value for name, value in os.environ.items()
                   if name.upper() not in GIT_REDIRECTS}
        environ.update(GIT_TERMINAL_PROMPT='0', GIT_OPTIONAL_LOCKS='0')
        try:
            result = subprocess.run(['git', *args], cwd=self.root, capture_output=True,
                                    env=environ, creationflags=NO_WINDOW, **options)
        except OSError as error:
            raise GateError(f'cannot run git: {error}') from error
        if result.returncode != 0:
            message = result.stderr.decode('utf-8', 'replace').strip().splitlines()
            raise GateError(f'git {" ".join(args)} failed with exit code {result.returncode}: '
                            f'{message[0] if message else "no message"}')
        return result.stdout

    def _require_toplevel(self):
        """Refuse a --root that is not an existing directory at the top of its git work tree."""
        if not self.root.is_dir():
            raise GateError(f'--root {self.root} is not a directory')
        try:
            top = self._git('rev-parse', '--show-toplevel').decode('utf-8', 'replace').strip()
        except GateError as error:
            raise GateError(f'--root {self.root} is not inside a git work tree: {error}') from error
        if (os.path.normcase(os.path.realpath(top))
                != os.path.normcase(os.path.realpath(self.root))):
            raise GateError(f'--root {self.root} is not the top of its git work tree ({top})')

    def _require_existing_commit(self, commit, label):
        """Refuse a value that is not a full SHA of a commit in this repository."""
        require_commit(commit, label)
        kind = self._git('cat-file', '-t', commit).decode('ascii', 'replace').strip()
        if kind != 'commit':
            raise GateError(f'{label} {commit} is a {kind}, not a commit')
        return commit

    def fingerprint(self, commit):
        """Return the runtime fingerprint of COMMIT from the blobs git stores for it.

        Refuses a commit that lacks one of the fixed runtime files, has no
        files under src/, or holds a runtime entry that is not a regular file.
        """
        require_commit(commit, 'commit')
        listing = self._git('ls-tree', '-r', '-z', commit, '--', *RUNTIME_FILES, RUNTIME_DIR)
        entries = {}
        for record in listing.split(b'\0'):
            if not record:
                continue
            meta, _, raw_path = record.partition(b'\t')
            fields = meta.split(b' ')
            if len(fields) != 3 or not raw_path:
                raise GateError(f'git ls-tree returned an unexpected line {record!r}')
            try:
                name = raw_path.decode('utf-8')
            except UnicodeDecodeError as error:
                raise GateError(f'runtime path {raw_path!r} at {commit} is not UTF-8') from error
            entries[name] = fields
        missing = [name for name in RUNTIME_FILES if name not in entries]
        if missing:
            raise GateError(f'commit {commit} lacks the runtime files {missing}')
        sources = sorted((name for name in entries if name.startswith(RUNTIME_DIR + '/')),
                         key=lambda name: name.encode('utf-8'))
        if not sources:
            raise GateError(f'commit {commit} has no files under {RUNTIME_DIR}/')
        ordered = list(RUNTIME_FILES) + sources
        for name in ordered:
            mode, kind, _ = entries[name]
            if kind != b'blob' or mode not in REGULAR_MODES:
                raise GateError(f'runtime entry {name} at {commit} is a {kind.decode()} with mode '
                                f'{mode.decode()}; only regular files are accepted')
        digest = hashlib.sha256()
        for name, content in zip(ordered, self._blobs([entries[name][2] for name in ordered])):
            digest.update(name.encode('utf-8') + b'\0' + content + b'\0')
        return digest.hexdigest()

    def _blobs(self, oids):
        """Return the exact stored bytes of each blob id, read with one git cat-file --batch call."""
        data = self._git('cat-file', '--batch', stdin=b''.join(oid + b'\n' for oid in oids))
        contents, position = [], 0
        for oid in oids:
            end = data.find(b'\n', position)
            header = data[position:end].split(b' ') if end >= 0 else []
            if (len(header) != 3 or header[0] != oid or header[1] != b'blob'
                    or not header[2].isdigit()):
                raise GateError(f'git cat-file could not read blob {oid.decode()}: '
                                f'{data[position:end][:200]!r}')
            start = end + 1
            stop = start + int(header[2])
            if data[stop:stop + 1] != b'\n':
                raise GateError(f'git cat-file returned a short body for blob {oid.decode()}')
            contents.append(data[start:stop])
            position = stop + 1
        if position != len(data):
            raise GateError('git cat-file returned more data than requested')
        return contents

    def _identity(self):
        """Require a clean working tree; return (HEAD, fingerprint).

        Clean means git status lists nothing, untracked files included; no
        tracked file carries the assume-unchanged or skip-worktree flag, either
        of which hides working tree edits from git status; and no ignored file
        sits under a runtime path, where a local build sees it but a clone of
        the commit does not.
        """
        self._require_toplevel()
        status = self._git('status', '--porcelain', '--untracked-files=all')
        if status.strip():
            lines = status.decode('utf-8', 'replace').splitlines()
            shown = '; '.join(lines[:5]) + ('; ...' if len(lines) > 5 else '')
            raise GateError(f'the working tree is not clean (untracked files count): {shown}')
        hidden = [entry.decode('utf-8', 'replace')
                  for entry in self._git('ls-files', '-v', '-z').split(b'\0')
                  if entry and not entry.startswith(b'H ')]
        if hidden:
            shown = '; '.join(hidden[:5]) + ('; ...' if len(hidden) > 5 else '')
            raise GateError('the index hides working tree edits from git status (h marks '
                            f'assume-unchanged, S marks skip-worktree): {shown}')
        ignored = [entry.decode('utf-8', 'replace')
                   for entry in self._git('status', '--porcelain', '-z', '--ignored',
                                          '--untracked-files=all', '--', *RUNTIME_FILES,
                                          RUNTIME_DIR).split(b'\0')
                   if entry]
        if ignored:
            shown = '; '.join(ignored[:5]) + ('; ...' if len(ignored) > 5 else '')
            raise GateError('ignored files sit under the runtime paths, where a local build '
                            f'sees them but a clone of the commit does not: {shown}')
        head = self._git('rev-parse', '--verify', 'HEAD^{commit}').decode('ascii', 'replace')
        head = require_commit(head.strip(), 'HEAD')
        return head, self.fingerprint(head)

    def _read_target(self, path):
        """Return the fresh live target from PATH (default <root>/artifacts/live-target.json)."""
        path = Path(path) if path is not None else self.root / TARGET
        target = self._target_reader(path, self._now())
        if not isinstance(target, dict):
            raise GateError(f'live target {path} is not a JSON object: {_preview(target)}')
        try:
            json.dumps(target, allow_nan=False)
        except (TypeError, ValueError, RecursionError) as error:
            raise GateError(f'live target {path} is not strict JSON: {error}') from error
        return target

    def prepare(self, checks_path, target_path=None):
        """Verify clean HEAD with every check and record it as a submittable candidate.

        Records 'prepare' with the commit, fingerprint, check records and live
        target on success, or 'gate_failed' with the error and the checks that
        ran. A check that fails, times out or cannot start also revokes the
        earlier prepare record of that commit. Returns the candidate record.
        """
        with self._locked():
            self._state(self._now())
            ran, identity, revoked = [], None, False
            try:
                checks, checks_sha256 = load_checks(checks_path, self.root)
                identity = self._identity()
                self._read_target(target_path)
                for check in checks:
                    revoked = True
                    record = run_check(check)
                    ran.append(record)
                    if record['timed_out']:
                        raise GateError(f"check {check['name']!r} timed out after "
                                        f"{check['timeout_s']:g} s")
                    if record['returncode'] != 0:
                        raise GateError(f"check {check['name']!r} exited with code "
                                        f"{record['returncode']}")
                    revoked = False
                try:
                    after = self._identity()
                except GateError as error:
                    raise GateError(f'the repository changed during the checks: {error}') from error
                if after != identity:
                    raise GateError('the repository changed during the checks: HEAD and '
                                    f'fingerprint were {identity[0]} {identity[1]}, now '
                                    f'{after[0]} {after[1]}')
                target = self._read_target(target_path)
                now = self._now()
                candidate = dict(commit=identity[0], fingerprint=identity[1],
                                 prepared_at=now.isoformat(), checks_file=str(checks_path),
                                 checks_sha256=checks_sha256, checks=ran, target=target)
                self._append('prepare', now, candidate=candidate)
                return candidate
            except (GateError, OSError, subprocess.SubprocessError) as error:
                message = str(error) if isinstance(error, GateError) else f'prepare failed: {error}'
                message = redact(message, os.environ)
                self._append('gate_failed', self._now(),
                             commit=identity[0] if identity else None,
                             fingerprint=identity[1] if identity else None,
                             revoked=revoked and identity is not None, error=message, checks=ran)
                raise GateError(message) from error

    def _check_observation(self, observed, floor, now):
        """Refuse a server observation that is stale, in the future or not after FLOOR."""
        age = (now - observed).total_seconds()
        if age < 0:
            raise GateError(f'--server-checked-at {observed.isoformat()} is in the future '
                            f'(now {now.isoformat()})')
        if age > OBSERVATION_MAX_AGE_S:
            raise GateError(f'--server-checked-at {observed.isoformat()} is {age:.0f} s old; '
                            f'read the remaining count again (limit {OBSERVATION_MAX_AGE_S} s)')
        if floor is not None and observed <= floor:
            raise GateError(f'--server-checked-at {observed.isoformat()} is not after the last '
                            f'reservation or result at {floor.isoformat()}; read the '
                            'remaining count again')

    def attempt(self, commit, server_remaining, server_checked_at, target_path=None):
        """Reserve one submission of COMMIT before the team submits it; return the attempt.

        Refuses while another attempt is submitting or scoring; when
        server_remaining is not an integer from 1 to the track's practice
        limit; when the server observation is older than 90 s, in the future or
        not after the last reservation or result; when the rolling hour already
        holds the hourly limit of attempts; when COMMIT has no valid prepare
        record; when HEAD or the fingerprint no longer match it; and when the
        same commit or the same fingerprint already consumed an attempt.
        """
        require_commit(commit, 'COMMIT')
        if type(server_remaining) is not int:
            raise GateError(f'--server-remaining must be an integer, got {server_remaining!r}')
        observed = parse_instant(server_checked_at, '--server-checked-at')
        with self._locked():
            state = self._state(self._now())
            busy = state['inflight']
            if busy is not None:
                raise GateError(f"attempt {busy['attempt_id']} is still {busy['state']}; record "
                                'its receipt or result before reserving another')
            limit, _ = practice_limit(self._read_target(target_path))
            if not 1 <= server_remaining <= limit:
                raise GateError(f'--server-remaining must be from 1 to the practice limit '
                                f'{limit}, got {server_remaining}')
            self._check_observation(observed, state['floor'], self._now())
            cap = min(HOURLY_LIMIT, limit)
            if len(state['recent']) >= cap:
                raise GateError(f"{len(state['recent'])} attempts already exist in the rolling "
                                f'hour (limit {cap})')
            candidate = state['candidates'].get(commit)
            if candidate is None:
                raise GateError(f'commit {commit} has no prepare record (never prepared, or '
                                'revoked by a later failed prepare)')
            for row in state['attempts']:
                if row['commit'] == commit:
                    raise GateError(f"commit {commit} already consumed attempt "
                                    f"{row['attempt_id']}")
                if row['fingerprint'] == candidate['fingerprint']:
                    raise GateError(f"runtime fingerprint {candidate['fingerprint']} already "
                                    f"consumed attempt {row['attempt_id']} (commit "
                                    f"{row['commit']})")
            head, fingerprint = self._identity()
            if head != commit:
                raise GateError(f'HEAD is {head}, not the prepared commit {commit}')
            if fingerprint != candidate['fingerprint']:
                raise GateError(f'the runtime fingerprint is {fingerprint}, not the prepared '
                                f"{candidate['fingerprint']}")
            target = self._read_target(target_path)
            limit, source = practice_limit(target)
            if not 1 <= server_remaining <= limit:
                raise GateError(f'--server-remaining must be from 1 to the practice limit '
                                f'{limit}, got {server_remaining}')
            now = self._now()
            self._check_observation(observed, state['floor'], now)
            record = dict(attempt_id='A-' + uuid.uuid4().hex, state='submitting', manual=False,
                          commit=commit, fingerprint=candidate['fingerprint'],
                          submitted_at=now.isoformat(), server_remaining=server_remaining,
                          server_checked_at=observed.isoformat(), practice_limit=limit,
                          practice_limit_source=source, hourly_limit=min(HOURLY_LIMIT, limit),
                          target=target, receipt=None, score=None)
            self._append('attempt', now, attempt=record)
            return record

    def record_receipt(self, attempt_id, receipt, evidence):
        """Link an organizer receipt to a reserved attempt; the same pair again is a no-op.

        Refuses an unknown attempt, an attempt that already has another
        receipt, and a receipt already linked to another attempt.
        """
        require_receipt(receipt)
        require_evidence(evidence)
        if not isinstance(attempt_id, str) or not attempt_id:
            raise GateError(f'ATTEMPT_ID must be a non-empty string, got {attempt_id!r}')
        with self._locked():
            now = self._now()
            state = self._state(now)
            row = state['by_id'].get(attempt_id)
            if row is None:
                raise GateError(f'no attempt {attempt_id!r} in the journal')
            if row['receipt'] == receipt:
                return row
            if row['receipt'] is not None:
                raise GateError(f"attempt {attempt_id} already has receipt {row['receipt']}; "
                                f'refusing {receipt}')
            owner = state['by_receipt'].get(receipt)
            if owner is not None:
                raise GateError(f'receipt {receipt} already belongs to attempt {owner}')
            change = dict(state='scoring', receipt=receipt, receipt_evidence=evidence,
                          receipt_observed_at=now.isoformat())
            self._append('receipt', now, attempt_id=attempt_id, change=change)
            return dict(row, **change)

    def import_receipt(self, receipt, submitted_at, evidence, commit=None):
        """Record a submission made outside this tool; an identical repeat is a no-op.

        With commit, the fingerprint is computed from that commit's stored
        blobs. Refuses a submission time in the future, an unknown commit, a
        receipt already recorded with different details, and any import while
        another attempt is submitting or scoring.
        """
        require_receipt(receipt)
        require_evidence(evidence)
        submitted = parse_instant(submitted_at, '--submitted-at')
        if commit is not None:
            require_commit(commit, '--commit')
        with self._locked():
            now = self._now()
            if submitted > now:
                raise GateError(f'--submitted-at {submitted.isoformat()} is in the future '
                                f'(now {now.isoformat()})')
            state = self._state(now)
            fingerprint = None
            if commit is not None:
                self._require_toplevel()
                self._require_existing_commit(commit, '--commit')
                fingerprint = self.fingerprint(commit)
            owner = state['by_receipt'].get(receipt)
            if owner is not None:
                row = state['by_id'][owner]
                if (row.get('manual') is True and row['commit'] == commit
                        and row['fingerprint'] == fingerprint
                        and parse_instant(row['submitted_at'], 'submitted_at') == submitted
                        and row['receipt_evidence'] == evidence):
                    return row
                raise GateError(f'receipt {receipt} is already recorded on attempt {owner} with '
                                'different details; refusing to change it')
            busy = state['inflight']
            if busy is not None:
                raise GateError(f"attempt {busy['attempt_id']} is still {busy['state']}; record "
                                'its receipt or result before importing another submission')
            record = dict(attempt_id='M-' + receipt, state='scoring', manual=True, commit=commit,
                          fingerprint=fingerprint, submitted_at=submitted.isoformat(),
                          receipt=receipt, receipt_evidence=evidence,
                          receipt_observed_at=now.isoformat(), score=None)
            self._append('import_receipt', now, attempt=record)
            return record

    def record_result(self, receipt, outcome, score=None, evidence=None):
        """Record the official outcome of a receipt; identical input again is a no-op.

        scored needs a finite score above 0 seconds; rejected and failed take
        none. A recorded result never changes.
        """
        require_receipt(receipt)
        require_outcome(outcome, score)
        require_evidence(evidence)
        with self._locked():
            now = self._now()
            state = self._state(now)
            owner = state['by_receipt'].get(receipt)
            if owner is None:
                raise GateError(f'receipt {receipt} is not in the journal; record it with receipt '
                                'or import-receipt first')
            row = state['by_id'][owner]
            if row['state'] in TERMINAL:
                if (row['state'] == outcome and row['score'] == score
                        and row['result_evidence'] == evidence):
                    return row
                raise GateError(f"receipt {receipt} already has the result {row['state']} "
                                f"(score {row['score']!r}); results never change")
            change = dict(state=outcome, score=score, result_evidence=evidence,
                          result_observed_at=now.isoformat())
            self._append('result', now, attempt_id=owner, change=change)
            return dict(row, **change)

    def status(self):
        """Return candidates, attempts, inflight, local allowance, best official time and failures.

        best_official is the lowest scored time; equal times go to the earlier
        submission, so a slower or equal later result never replaces it.
        """
        with self._locked():
            now = self._now()
            state = self._state(now)
        consumed = {}
        for row in state['attempts']:
            for key in (row['commit'], row['fingerprint']):
                if key:
                    consumed.setdefault(key, row['attempt_id'])
        best = state['best']
        return dict(
            journal=str(self.journal), now=now.isoformat(),
            candidates=[dict(commit=row['commit'], fingerprint=row['fingerprint'],
                             prepared_at=row.get('prepared_at'),
                             consumed_by=consumed.get(row['commit'],
                                                      consumed.get(row['fingerprint'])),
                             checks=[_check_summary(check) for check in row.get('checks', [])])
                        for row in state['candidates'].values()],
            attempts=[_attempt_summary(row) for row in state['attempts']],
            inflight=_attempt_summary(state['inflight']) if state['inflight'] else None,
            hourly_limit=HOURLY_LIMIT, window_s=WINDOW_S,
            local_remaining=max(0, HOURLY_LIMIT - len(state['recent'])),
            observation_after=state['floor'].isoformat() if state['floor'] else None,
            best_official=(dict(receipt=best['receipt'], attempt_id=best['attempt_id'],
                                commit=best['commit'], fingerprint=best['fingerprint'],
                                score=best['score'], submitted_at=best['submitted_at'],
                                result_evidence=best.get('result_evidence'))
                           if best else None),
            gate_failures=[dict(at=event['at'], commit=event['commit'],
                                fingerprint=event.get('fingerprint'), revoked=event['revoked'],
                                error=event.get('error'),
                                checks=[_check_summary(check) for check in event.get('checks', [])])
                           for event in state['failures']])


class _Parser(argparse.ArgumentParser):
    """Argument parser that raises GateError instead of exiting on bad arguments."""

    def error(self, message):
        """Raise GateError with argparse's message."""
        raise GateError(f'{self.prog}: {message}')


def build_parser():
    """Return the command-line parser."""
    parser = _Parser(prog='submission_gate.py', description=__doc__.splitlines()[0],
                     formatter_class=argparse.RawDescriptionHelpFormatter,
                     epilog='\n'.join(__doc__.splitlines()[2:]))
    parser.add_argument('--root', type=Path, default=ROOT,
                        help='top of the git work tree (default: the parent of this script folder)')
    parser.add_argument('--journal', type=Path,
                        help='journal file (default: <root>/artifacts/submissions/events.jsonl)')
    commands = parser.add_subparsers(dest='command', required=True, metavar='COMMAND')
    prepare = commands.add_parser('prepare', help='run every check at clean HEAD and record it')
    prepare.add_argument('--checks', type=Path, required=True, help='JSON checks file')
    prepare.add_argument('--target', type=Path, help='live target file')
    attempt = commands.add_parser('attempt', help='reserve a submission before submitting')
    attempt.add_argument('commit')
    attempt.add_argument('--server-remaining', type=int, required=True)
    attempt.add_argument('--server-checked-at', required=True)
    attempt.add_argument('--target', type=Path, help='live target file')
    receipt = commands.add_parser('receipt', help='link the organizer receipt to an attempt')
    receipt.add_argument('attempt_id')
    receipt.add_argument('receipt')
    receipt.add_argument('--evidence', required=True)
    imported = commands.add_parser('import-receipt', help='record a submission made elsewhere')
    imported.add_argument('receipt')
    imported.add_argument('--submitted-at', required=True)
    imported.add_argument('--evidence', required=True)
    imported.add_argument('--commit', help='full 40-character SHA, when known')
    result = commands.add_parser('result', help='record the official outcome of a receipt')
    result.add_argument('receipt')
    result.add_argument('outcome')
    result.add_argument('--score', type=float)
    result.add_argument('--evidence', required=True)
    commands.add_parser('status', help='print the derived state')
    return parser


def main(argv=None):
    """Run one command; print one JSON document; return 0 on success and 2 on refusal.

    An unexpected exception is reported the same way as a refusal, with an
    error naming its type, and its traceback goes to stderr.
    """
    try:
        args = build_parser().parse_args(argv)
        gate = SubmissionGate(args.root, journal=args.journal)
        if args.command == 'prepare':
            value = gate.prepare(args.checks, args.target)
        elif args.command == 'attempt':
            value = gate.attempt(args.commit, args.server_remaining, args.server_checked_at,
                                 args.target)
        elif args.command == 'receipt':
            value = gate.record_receipt(args.attempt_id, args.receipt, args.evidence)
        elif args.command == 'import-receipt':
            value = gate.import_receipt(args.receipt, args.submitted_at, args.evidence,
                                        args.commit)
        elif args.command == 'result':
            value = gate.record_result(args.receipt, args.outcome, args.score, args.evidence)
        else:
            value = gate.status()
        text = json.dumps(value, indent=2, allow_nan=False, sort_keys=True)
    except (GateError, OSError) as error:
        print(json.dumps(dict(error=str(error), ready=False)))
        return 2
    except Exception as error:
        traceback.print_exc(file=sys.stderr)
        print(json.dumps(dict(error=f'unexpected {type(error).__name__}: {error}', ready=False)))
        return 2
    print(text)
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
