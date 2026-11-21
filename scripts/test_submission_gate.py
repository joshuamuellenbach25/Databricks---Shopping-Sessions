"""Tests for submission_gate.py.

Each test builds a throwaway git repository that holds the runtime files, runs
real check processes (the current interpreter with a one-line script), drives
the gate with an injected clock and reads a fake live target file through
watch_leaderboard.read_fresh. Nothing touches the network.
"""
import contextlib
from datetime import datetime, timedelta, timezone
import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

import submission_gate as gate_module
from submission_gate import GateError, SubmissionGate

EVIDENCE = 'https://www.joinbuilders.org/buildathon/tracks/shopping-sessions'
START = datetime(2026, 11, 21, 1, 0, tzinfo=timezone.utc)
SCRIPT = Path(gate_module.__file__).resolve()
REMOVE = object()


def python(code, *args):
    """Return argv that runs CODE with the current interpreter."""
    return [sys.executable, '-c', code, *args]


PASS = python('print("check passed")')
FAIL = python('import sys; print("check failed"); sys.exit(1)')


class GateTest(unittest.TestCase):
    """Throwaway repository, fake live target, checks file and injected clock for each test."""

    def setUp(self):
        environment = mock.patch.dict(os.environ)
        environment.start()
        self.addCleanup(environment.stop)
        for name in gate_module.GIT_REDIRECTS:
            os.environ.pop(name, None)
        temp = tempfile.TemporaryDirectory(ignore_cleanup_errors=True)
        self.addCleanup(temp.cleanup)
        self.base = Path(temp.name)
        self.root = self.base / 'repo'
        self.root.mkdir()
        self.now = START
        self.runtime_edits = 0
        self.git('init', '-q')
        for key, value in (('user.email', 'gate-tests@example.invalid'),
                           ('user.name', 'Gate Tests'), ('core.autocrlf', 'false'),
                           ('commit.gpgsign', 'false')):
            self.git('config', key, value)
        self.files = {
            '.gitignore': b'artifacts/\n',
            'Dockerfile': b'FROM scratch\n',
            '.dockerignore': b'target/\n',
            'Cargo.toml': b'[package]\nname = "solver"\n',
            'Cargo.lock': b'version = 3\n',
            'src/main.rs': b'fn main() {}\n',
            'src/a-b.rs': b'pub fn a() {}\n',
            'src/scan/mod.rs': b'pub fn scan() {}\n',
            'README.md': b'notes\n',
        }
        for name, content in self.files.items():
            self.write(name, content)
        self.commit()
        self.target = self.root / 'artifacts' / 'live-target.json'
        self.journal = self.root / 'artifacts' / 'submissions' / 'events.jsonl'
        self.write_target()
        self.checks = self.write_checks([dict(name='unit', argv=PASS, timeout_s=60)], 'checks.json')

    def git(self, *args):
        """Run git in the throwaway repository and return its stripped stdout."""
        result = subprocess.run(['git', *args], cwd=self.root, capture_output=True, text=True,
                                encoding='utf-8', errors='replace')
        if result.returncode != 0:
            raise AssertionError(f'git {args} failed: {result.stderr}')
        return result.stdout.strip()

    def write(self, name, content):
        """Write exact bytes to a repository file, creating folders."""
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(content)

    def commit(self, message='fixture'):
        """Commit every change and return the new HEAD."""
        self.git('add', '-A')
        self.git('commit', '-q', '--allow-empty', '-m', message)
        return self.head()

    def head(self):
        """Return the full HEAD SHA."""
        return self.git('rev-parse', 'HEAD')

    def change_runtime(self, name='src/main.rs'):
        """Append a distinct line to a runtime file, commit it and return the new HEAD."""
        self.runtime_edits += 1
        with open(self.root / name, 'ab') as handle:
            handle.write(f'// revision {self.runtime_edits}\n'.encode('ascii'))
        return self.commit(f'runtime revision {self.runtime_edits}')

    def write_target(self, **changes):
        """Write a fresh watcher state at the current clock; REMOVE drops a key."""
        state = dict(status='fresh', checked_at=self.now.isoformat(),
                     last_success_at=self.now.isoformat(),
                     source='https://www.joinbuilders.org/buildathon/standings/shopping-sessions',
                     team='BackwardsDeployeds', metric='Run time', unit='s', direction='lower',
                     leader_team='Leader', leader_score=1.17, target_score=1.111,
                     our_score=None, practice_limit=2, closed=False, paused=False, error=None)
        state.update(changes)
        state = {key: value for key, value in state.items() if value is not REMOVE}
        self.target.parent.mkdir(parents=True, exist_ok=True)
        self.target.write_text(json.dumps(state), encoding='utf-8')

    def write_checks(self, checks, name='extra-checks.json'):
        """Write a checks file outside the repository and return its path."""
        path = self.base / name
        path.write_text(json.dumps(checks), encoding='utf-8')
        return path

    def gate(self):
        """Return a gate on the throwaway repository driven by the test clock."""
        return SubmissionGate(self.root, now=lambda: self.now, lock_timeout_s=5)

    def prepare(self, checks=None):
        """Refresh the live target and run prepare with the given or default checks."""
        self.write_target()
        path = self.checks if checks is None else self.write_checks(checks)
        return self.gate().prepare(path, self.target)

    def reserve(self, commit, remaining=2):
        """Advance the clock, refresh the target and reserve COMMIT with a fresh observation."""
        self.now += timedelta(seconds=1)
        self.write_target()
        return self.gate().attempt(commit, remaining, self.now.isoformat(), self.target)

    def resolve(self, attempt, receipt, seconds=1.5, outcome='scored'):
        """Link RECEIPT to ATTEMPT and record its official outcome."""
        gate = self.gate()
        self.now += timedelta(seconds=1)
        gate.record_receipt(attempt['attempt_id'], receipt, EVIDENCE)
        self.now += timedelta(seconds=1)
        return gate.record_result(receipt, outcome, seconds if outcome == 'scored' else None,
                                  EVIDENCE)

    def status(self):
        """Return the gate status at the test clock."""
        return self.gate().status()

    def journal_bytes(self):
        """Return the journal bytes, or b'' before the first event."""
        return self.journal.read_bytes() if self.journal.exists() else b''

    def assertRefused(self, call, *fragments):
        """Assert that CALL raises GateError whose message holds every fragment."""
        with self.assertRaises(GateError) as caught:
            call()
        for fragment in fragments:
            self.assertIn(fragment, str(caught.exception))
        return caught.exception


class FingerprintTests(GateTest):
    """The runtime fingerprint covers exactly the stored runtime bytes."""

    def test_fingerprint_follows_the_defined_byte_layout(self):
        ordered = ['Dockerfile', '.dockerignore', 'Cargo.toml', 'Cargo.lock',
                   'src/a-b.rs', 'src/main.rs', 'src/scan/mod.rs']
        digest = hashlib.sha256()
        for name in ordered:
            digest.update(name.encode('ascii') + b'\0' + self.files[name] + b'\0')
        self.assertEqual(self.gate().fingerprint(self.head()), digest.hexdigest())

    def test_fingerprint_reads_stored_blobs_not_checked_out_line_endings(self):
        stored = self.gate().fingerprint(self.head())
        self.git('config', 'core.autocrlf', 'true')
        for name in ('Dockerfile', 'src/main.rs'):
            (self.root / name).unlink()
        self.git('checkout', '--', 'Dockerfile', 'src/main.rs')
        self.assertIn(b'\r\n', (self.root / 'src' / 'main.rs').read_bytes())
        self.assertEqual(self.git('status', '--porcelain', '--untracked-files=all'), '')
        self.assertEqual(self.prepare()['fingerprint'], stored)

    def test_fingerprint_changes_with_runtime_files_only(self):
        gate = self.gate()
        base = gate.fingerprint(self.head())
        self.write('README.md', b'more notes\n')
        self.write('scripts/tool.py', b'print(1)\n')
        self.assertEqual(gate.fingerprint(self.commit('docs and tools')), base)
        seen = {base}
        for name in ('Dockerfile', '.dockerignore', 'Cargo.toml', 'Cargo.lock', 'src/scan/mod.rs'):
            with self.subTest(name=name):
                fingerprint = gate.fingerprint(self.change_runtime(name))
                self.assertNotIn(fingerprint, seen)
                seen.add(fingerprint)
        self.write('src/extra.rs', b'')
        self.assertNotIn(gate.fingerprint(self.commit('new source file')), seen)

    def test_missing_runtime_file_is_refused(self):
        self.git('rm', '-q', '.dockerignore')
        self.commit('drop the docker ignore file')
        self.assertRefused(self.prepare, '.dockerignore')
        self.assertEqual(self.status()['candidates'], [])


class PrepareTests(GateTest):
    """prepare runs every check at clean HEAD and records the outcome."""

    def test_prepare_records_checks_identity_and_target(self):
        checks = [dict(name='unit', argv=PASS, timeout_s=60),
                  dict(name='second', argv=python('import sys; print("second ok", file=sys.stderr)'),
                       timeout_s=60, env={'EXTRA_FLAG': '1'}, cwd='src')]
        candidate = self.prepare(checks)
        head = self.head()
        self.assertEqual(candidate['commit'], head)
        self.assertEqual(candidate['fingerprint'], self.gate().fingerprint(head))
        first, second = candidate['checks']
        self.assertEqual((first['name'], first['argv'], first['returncode']), ('unit', PASS, 0))
        self.assertIn('check passed', first['output'])
        self.assertIsInstance(first['elapsed_s'], float)
        self.assertIn('second ok', second['output'])
        self.assertEqual(Path(second['cwd']).resolve(), (self.root / 'src').resolve())
        self.assertEqual(second['env_keys'], ['EXTRA_FLAG'])
        self.assertEqual(candidate['target']['status'], 'fresh')
        status = self.status()
        self.assertEqual([row['commit'] for row in status['candidates']], [head])
        self.assertIsNone(status['best_official'])
        self.assertEqual(status['local_remaining'], 2)
        data = self.journal_bytes()
        data.decode('ascii')
        self.assertTrue(data.endswith(b'\n'))
        self.assertEqual([json.loads(line)['action'] for line in data.splitlines()], ['prepare'])
        self.assertTrue(self.gate().lock_path.is_file())

    def test_modified_tracked_file_is_refused(self):
        self.write('src/main.rs', b'fn main() { edited(); }\n')
        self.assertRefused(self.prepare, 'not clean', 'src/main.rs')
        self.assertEqual(self.status()['candidates'], [])

    def test_untracked_file_is_refused(self):
        self.write('notes.txt', b'scratch\n')
        self.assertRefused(self.prepare, 'not clean', 'notes.txt')
        self.assertEqual(self.status()['candidates'], [])

    def test_root_must_be_the_top_of_the_work_tree(self):
        gate = SubmissionGate(self.root / 'src', journal=self.journal, now=lambda: self.now)
        self.assertRefused(lambda: gate.prepare(self.checks, self.target), 'not the top')

    def test_index_flags_that_hide_edits_are_refused(self):
        candidate = self.prepare()
        for flag in ('assume-unchanged', 'skip-worktree'):
            with self.subTest(flag=flag):
                self.git('update-index', '--' + flag, 'src/main.rs')
                self.write('src/main.rs', b'fn main() { hidden(); }\n')
                self.assertEqual(self.git('status', '--porcelain', '--untracked-files=all'), '')
                self.assertRefused(self.prepare, 'src/main.rs', flag)
                self.assertRefused(lambda: self.reserve(candidate['commit']), 'src/main.rs', flag)
                self.git('update-index', '--no-' + flag, 'src/main.rs')
                self.git('checkout', '--', 'src/main.rs')
        self.assertEqual(self.status()['attempts'], [])
        self.assertEqual(self.reserve(candidate['commit'])['state'], 'submitting')

    def test_ignored_files_under_runtime_paths_are_refused(self):
        self.write('src/artifacts/generated.rs', b'pub fn generated() {}\n')
        self.assertEqual(self.git('status', '--porcelain', '--untracked-files=all'), '')
        self.assertRefused(self.prepare, 'ignored', 'src/artifacts/generated.rs')
        self.assertEqual(self.status()['candidates'], [])
        (self.root / 'src' / 'artifacts' / 'generated.rs').unlink()
        self.assertEqual(self.prepare()['commit'], self.head())

    def test_failed_check_records_gate_failure_and_no_candidate(self):
        checks = [dict(name='unit', argv=PASS, timeout_s=60),
                  dict(name='broken', argv=FAIL, timeout_s=60),
                  dict(name='never', argv=PASS, timeout_s=60)]
        self.assertRefused(lambda: self.prepare(checks), "'broken'", 'code 1')
        status = self.status()
        self.assertEqual(status['candidates'], [])
        [failure] = status['gate_failures']
        self.assertEqual((failure['commit'], failure['revoked']), (self.head(), True))
        self.assertEqual([(check['name'], check['returncode']) for check in failure['checks']],
                         [('unit', 0), ('broken', 1)])

    def test_failed_check_revokes_the_earlier_prepare_of_that_commit(self):
        candidate = self.prepare()
        self.write_target(status='stale')
        self.assertRefused(lambda: self.gate().prepare(self.checks, self.target), 'live target')
        self.assertEqual([row['commit'] for row in self.status()['candidates']],
                         [candidate['commit']])
        self.assertRefused(lambda: self.prepare([dict(name='broken', argv=FAIL, timeout_s=60)]),
                           'broken')
        self.assertEqual(self.status()['candidates'], [])
        self.assertEqual([failure['revoked'] for failure in self.status()['gate_failures']],
                         [False, True])
        self.assertRefused(lambda: self.reserve(candidate['commit']), 'no prepare record')
        self.prepare()
        self.assertEqual(self.reserve(candidate['commit'])['state'], 'submitting')

    def test_repository_change_during_checks_is_refused(self):
        original = self.head()
        edit = python('open("src/main.rs", "a").write("// edited during checks\\n")')
        self.assertRefused(lambda: self.prepare([dict(name='edit', argv=edit, timeout_s=60)]),
                           'changed during the checks', 'not clean')
        self.git('checkout', '--', 'src/main.rs')
        commit = python('import subprocess\n'
                        'open("src/main.rs", "a").write("// committed during checks\\n")\n'
                        'subprocess.run(["git", "commit", "-q", "-a", "-m", "raced"], check=True)\n')
        self.assertRefused(lambda: self.prepare([dict(name='commit', argv=commit, timeout_s=60)]),
                           'changed during the checks', original)
        self.assertNotEqual(self.head(), original)
        status = self.status()
        self.assertEqual(status['candidates'], [])
        self.assertEqual([(failure['commit'], failure['revoked'])
                          for failure in status['gate_failures']],
                         [(original, False), (original, False)])

    def test_live_target_must_be_fresh_before_and_after_checks(self):
        marker = self.base / 'ran.txt'
        touch = self.write_checks([dict(name='touch', timeout_s=60, argv=python(
            'import sys; open(sys.argv[1], "w").write("ran")', str(marker)))], 'touch.json')
        for changes in (dict(status='stale'),
                        dict(last_success_at=(START - timedelta(seconds=91)).isoformat()),
                        dict(last_success_at=(START + timedelta(seconds=5)).isoformat()),
                        dict(last_success_at='yesterday')):
            with self.subTest(changes=changes):
                self.write_target(**changes)
                self.assertRefused(lambda: self.gate().prepare(touch, self.target), 'live target')
                self.assertFalse(marker.exists())
        self.target.unlink()
        self.assertRefused(lambda: self.gate().prepare(touch, self.target), 'live target')
        self.assertFalse(marker.exists())
        self.target.write_text('[' * 100000 + ']' * 100000, encoding='ascii')
        self.assertRefused(lambda: self.gate().prepare(touch, self.target), 'live target')
        self.assertFalse(marker.exists())
        go_stale = python('import json, sys\n'
                          'state = json.load(open(sys.argv[1]))\n'
                          'state["status"] = "stale"\n'
                          'json.dump(state, open(sys.argv[1], "w"))\n', str(self.target))
        self.assertRefused(lambda: self.prepare([dict(name='stale', argv=go_stale, timeout_s=60)]),
                           'live target')
        status = self.status()
        self.assertEqual(status['candidates'], [])
        self.assertEqual(status['gate_failures'][-1]['checks'][0]['returncode'], 0)

    def test_check_output_is_redacted_and_bounded(self):
        code = ('import os\n'
                'print("FIRST-LINE")\n'
                'print("Authorization: Bearer header-secret-value")\n'
                'print("Proxy-Authorization: Basic")\n'
                'print("  folded-secret-value")\n'
                'print("Cookie: session=cookie-secret-value")\n'
                'print("Set-Cookie: id=set-cookie-secret-value; HttpOnly")\n'
                'print("using " + os.environ["SAMPLE_API_TOKEN"])\n'
                'print("password=inline-secret-value")\n'
                'print("fetch https://user:url-secret-value@example.invalid/x")\n'
                'print(\'{"Authorization": "Bearer quoted-header-secret"}\')\n'
                'print({"token": "quoted-token-secret"})\n'
                'print("clone https://url-token-secret@example.invalid/x")\n'
                'print("ordinary diagnostics line\\n" * 2000)\n'
                'print("LAST-LINE")\n')
        program = self.base / 'noisy.py'
        program.write_text(code, encoding='ascii')
        candidate = self.prepare([
            dict(name='noisy', argv=[sys.executable, str(program)], timeout_s=60,
                 env={'SAMPLE_API_TOKEN': 'env-secret-value-123'}),
            dict(name='argv', argv=python('pass', 'api_key=argv-secret-value'), timeout_s=60)])
        self.assertEqual(candidate['checks'][1]['argv'][-1], 'api_key=[REDACTED]')
        record = candidate['checks'][0]
        self.assertLessEqual(len(record['output']), gate_module.OUTPUT_LIMIT)
        self.assertTrue(record['output_truncated'])
        self.assertGreater(record['output_characters'], gate_module.OUTPUT_LIMIT)
        self.assertIn('[TRUNCATED]', record['output'])
        self.assertTrue(record['output'].startswith('FIRST-LINE'))
        self.assertIn('LAST-LINE', record['output'])
        self.assertIn('[REDACTED]', record['output'])
        journal = self.journal_bytes().decode('ascii')
        for secret in ('header-secret-value', 'folded-secret-value', 'cookie-secret-value',
                       'set-cookie-secret-value', 'env-secret-value-123', 'inline-secret-value',
                       'url-secret-value', 'argv-secret-value', 'quoted-header-secret',
                       'quoted-token-secret', 'url-token-secret'):
            with self.subTest(secret=secret):
                self.assertNotIn(secret, journal)

    def test_check_timeout_kills_the_process_tree(self):
        code = ('import subprocess, sys, time\n'
                'subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])\n'
                'print("started", flush=True)\n'
                'time.sleep(60)\n')
        self.assertRefused(lambda: self.prepare([dict(name='slow', argv=python(code), timeout_s=2)]),
                           "'slow'", 'timed out')
        [failure] = self.status()['gate_failures']
        [check] = failure['checks']
        self.assertTrue(check['timed_out'])
        self.assertLess(check['elapsed_s'], gate_module.KILL_WAIT_S)

    def test_checks_file_is_validated(self):
        cases = [
            ('not json', 'not valid JSON'),
            ('{}', 'non-empty JSON list'),
            ('[]', 'non-empty JSON list'),
            ('[1]', 'must be an object'),
            ('[{"name": "a", "argv": ["x"]}]', "['timeout_s']"),
            ('[{"name": "a", "argv": "python", "timeout_s": 5}]', "'python'"),
            ('[{"name": "a", "argv": [], "timeout_s": 5}]', 'argv must be'),
            ('[{"name": "", "argv": ["x"], "timeout_s": 5}]', "got ''"),
            ('[{"name": "a", "argv": ["x"], "timeout_s": 0}]', 'got 0'),
            ('[{"name": "a", "argv": ["x"], "timeout_s": 86401}]', 'got 86401'),
            ('[{"name": "a", "argv": ["x"], "timeout_s": true}]', 'got True'),
            ('[{"name": "a", "argv": ["x"], "timeout_s": NaN}]', 'NaN'),
            ('[{"name": "a", "argv": ["x"], "timeout_s": 5, "shell": true}]', "['shell']"),
            ('[{"name": "a", "argv": ["x"], "timeout_s": 5}, '
             '{"name": "a", "argv": ["y"], "timeout_s": 5}]', "repeats the check name 'a'"),
            ('[{"name": "a", "argv": ["x"], "timeout_s": 5, "env": {"K": 1}}]', "'K'"),
            ('[{"name": "a", "argv": ["x"], "timeout_s": 5, "cwd": "missing-dir"}]', 'missing-dir'),
            ('[{"name": "a", "name": "b", "argv": ["x"], "timeout_s": 5}]', "duplicate key 'name'"),
        ]
        path = self.base / 'bad-checks.json'
        for text, fragment in cases:
            with self.subTest(text=text):
                path.write_text(text, encoding='utf-8')
                self.assertRefused(lambda: self.gate().prepare(path, self.target), fragment)
        self.assertRefused(lambda: self.gate().prepare(self.base / 'absent.json', self.target),
                           'absent.json')
        status = self.status()
        self.assertEqual(status['candidates'], [])
        self.assertEqual(len(status['gate_failures']), len(cases) + 1)
        self.assertTrue(all(failure['commit'] is None for failure in status['gate_failures']))

    def test_extreme_checks_files_are_refused_and_recorded(self):
        path = self.base / 'extreme-checks.json'
        journal = self.base / 'cli' / 'events.jsonl'
        for label, text, fragment in (
                ('huge timeout', '[{"name": "a", "argv": ["x"], "timeout_s": 1' + '0' * 400 + '}]',
                 'timeout_s'),
                ('deep nesting', '[' * 100000 + ']' * 100000, 'nests too deeply')):
            with self.subTest(label=label):
                path.write_text(text, encoding='ascii')
                self.assertRefused(lambda: self.gate().prepare(path, self.target), fragment)
                result = subprocess.run(
                    [sys.executable, str(SCRIPT), '--root', str(self.root), '--journal', str(journal),
                     'prepare', '--checks', str(path)], capture_output=True, text=True,
                    encoding='utf-8', errors='replace', timeout=120)
                self.assertEqual(result.returncode, 2, result.stderr[-500:])
                refusal = json.loads(result.stdout)
                self.assertIs(refusal['ready'], False)
                self.assertIn(fragment, refusal['error'])
        self.assertEqual(len(self.status()['gate_failures']), 2)

    def test_refusals_keep_check_secrets_out_of_the_journal(self):
        cases = (
            (dict(name='env', argv=['tool'], timeout_s=5,
                  env={'SAMPLE_TOKEN': 'env-file-secret-value', 'JOBS': 8}), 'JOBS'),
            (dict(name='argv', argv=['tool', 'token=argv-file-secret-value', 3], timeout_s=5),
             'argv must be'),
        )
        for check, fragment in cases:
            with self.subTest(check=check['name']):
                path = self.write_checks([check])
                error = self.assertRefused(lambda: self.gate().prepare(path, self.target), fragment)
                self.assertNotIn('secret-value', str(error))
        journal = self.journal_bytes().decode('ascii')
        self.assertEqual(journal.count('"gate_failed"'), 2)
        self.assertNotIn('secret-value', journal)


class AttemptTests(GateTest):
    """attempt reserves a submission only when every rule allows it."""

    def test_attempt_reserves_and_blocks_while_inflight(self):
        first = self.prepare()
        attempt = self.reserve(first['commit'])
        self.assertTrue(attempt['attempt_id'].startswith('A-'))
        self.assertEqual(attempt['state'], 'submitting')
        self.assertEqual((attempt['practice_limit'], attempt['practice_limit_source']),
                         (2, 'live target'))
        self.assertEqual(attempt['target']['status'], 'fresh')
        self.assertEqual(self.status()['inflight']['attempt_id'], attempt['attempt_id'])
        self.change_runtime()
        second = self.prepare()
        self.assertRefused(lambda: self.reserve(second['commit']), attempt['attempt_id'],
                           'submitting')
        self.now += timedelta(seconds=1)
        self.gate().record_receipt(attempt['attempt_id'], 'BB-00000001', EVIDENCE)
        self.assertRefused(lambda: self.reserve(second['commit']), 'scoring')
        self.now += timedelta(seconds=1)
        self.gate().record_result('BB-00000001', 'failed', None, EVIDENCE)
        self.assertIsNone(self.status()['inflight'])
        self.assertEqual(self.reserve(second['commit'])['state'], 'submitting')

    def test_server_remaining_must_be_an_int_within_the_practice_limit(self):
        commit = self.prepare()['commit']
        for remaining in (0, 3, -1):
            with self.subTest(remaining=remaining):
                self.assertRefused(lambda: self.reserve(commit, remaining), f'got {remaining}')
        for remaining in (True, '1', 1.0, None):
            with self.subTest(remaining=remaining):
                self.assertRefused(lambda: self.gate().attempt(
                    commit, remaining, self.now.isoformat(), self.target), repr(remaining))
        self.write_target(practice_limit=1)
        self.assertRefused(lambda: self.gate().attempt(commit, 2, self.now.isoformat(), self.target),
                           'practice limit 1')
        self.write_target(practice_limit=0)
        self.assertRefused(lambda: self.gate().attempt(commit, 1, self.now.isoformat(), self.target),
                           'practice_limit', 'got 0')
        self.assertEqual(self.status()['attempts'], [])
        self.now += timedelta(seconds=1)
        self.write_target(practice_limit=REMOVE)
        attempt = self.gate().attempt(commit, 2, self.now.isoformat(), self.target)
        self.assertEqual(attempt['practice_limit'], 2)
        self.assertIn('default', attempt['practice_limit_source'])
        self.assertIn('no practice_limit', attempt['practice_limit_source'])

    def test_server_observation_must_be_fresh_and_newer_than_the_last_record(self):
        commit = self.prepare()['commit']
        gate = self.gate()
        for observed, fragment in (((self.now - timedelta(seconds=91)).isoformat(), '91 s old'),
                                   ((self.now + timedelta(seconds=1)).isoformat(), 'in the future'),
                                   ('2026-11-21T01:00:00', 'timezone'),
                                   ('soon', 'not an ISO 8601 time')):
            with self.subTest(observed=observed):
                self.assertRefused(lambda: gate.attempt(commit, 2, observed, self.target), fragment)
        first = self.reserve(commit)
        self.resolve(first, 'BB-00000001')
        result_at = self.now
        self.change_runtime()
        second = self.prepare()
        self.now += timedelta(seconds=5)
        self.write_target()
        for observed in (first['server_checked_at'], (result_at - timedelta(seconds=1)).isoformat(),
                         result_at.isoformat()):
            with self.subTest(observed=observed):
                self.assertRefused(lambda: self.gate().attempt(second['commit'], 1, observed,
                                                               self.target),
                                   'not after the last reservation or result')
        later = (result_at + timedelta(seconds=1)).isoformat()
        self.assertEqual(self.gate().attempt(second['commit'], 1, later, self.target)['state'],
                         'submitting')

    def test_server_observation_must_be_newer_than_the_last_reservation(self):
        first = self.reserve(self.prepare()['commit'])
        reserved_at = self.now
        self.now -= timedelta(seconds=30)
        self.gate().record_receipt(first['attempt_id'], 'BB-00000001', EVIDENCE)
        self.gate().record_result('BB-00000001', 'failed', None, EVIDENCE)
        self.change_runtime()
        second = self.prepare()
        self.now = reserved_at + timedelta(seconds=10)
        self.write_target()
        self.assertRefused(lambda: self.gate().attempt(
            second['commit'], 1, (reserved_at - timedelta(seconds=1)).isoformat(), self.target),
            'not after the last reservation or result at ' + reserved_at.isoformat())
        self.assertEqual(self.gate().attempt(second['commit'], 1,
                                             (reserved_at + timedelta(seconds=1)).isoformat(),
                                             self.target)['state'], 'submitting')

    def test_attempt_requires_a_fresh_target_and_observation_when_it_writes(self):
        commit = self.prepare()['commit']
        self.now += timedelta(seconds=1)
        for changes in (dict(status='stale'),
                        dict(last_success_at=(self.now - timedelta(seconds=91)).isoformat())):
            with self.subTest(changes=changes):
                self.write_target(**changes)
                self.assertRefused(lambda: self.gate().attempt(commit, 2, self.now.isoformat(),
                                                               self.target), 'live target')
        self.target.unlink()
        self.assertRefused(lambda: self.gate().attempt(commit, 2, self.now.isoformat(), self.target),
                           'live target')
        self.write_target()
        identity = SubmissionGate._identity

        def slow_identity(gate):
            self.now += timedelta(seconds=91)
            return identity(gate)

        always_fresh = SubmissionGate(self.root, now=lambda: self.now, lock_timeout_s=5,
                                      target_reader=lambda path, now: dict(status='fresh',
                                                                           practice_limit=2))
        with mock.patch.object(SubmissionGate, '_identity', slow_identity):
            self.assertRefused(lambda: self.gate().attempt(commit, 2, self.now.isoformat(),
                                                           self.target), 'live target')
            self.assertRefused(lambda: always_fresh.attempt(commit, 2, self.now.isoformat(),
                                                            self.target), '91 s old')
        self.assertEqual(self.status()['attempts'], [])
        self.now += timedelta(seconds=1)
        self.write_target()
        oldest = (self.now - timedelta(seconds=gate_module.OBSERVATION_MAX_AGE_S)).isoformat()
        self.assertEqual(self.gate().attempt(commit, 2, oldest, self.target)['state'], 'submitting')

    def test_rolling_hour_holds_two_attempts(self):
        first = self.reserve(self.prepare()['commit'])
        self.resolve(first, 'BB-00000001')
        self.change_runtime()
        second = self.reserve(self.prepare()['commit'])
        self.resolve(second, 'BB-00000002')
        self.change_runtime()
        third = self.prepare()
        self.assertEqual(self.status()['local_remaining'], 0)
        self.assertRefused(lambda: self.reserve(third['commit']),
                           '2 attempts already exist in the rolling hour')
        self.now = datetime.fromisoformat(first['submitted_at']) + timedelta(seconds=3599)
        self.assertRefused(lambda: self.reserve(third['commit']), 'rolling hour')
        self.assertEqual(self.status()['local_remaining'], 0)
        self.assertEqual(self.reserve(third['commit'])['state'], 'submitting')
        self.assertEqual(self.status()['local_remaining'], 0)

    def test_attempt_requires_a_prepare_record_and_a_full_sha(self):
        head = self.head()
        self.assertRefused(lambda: self.reserve(head), 'no prepare record')
        for value in (head[:12], head.upper(), 'HEAD', ''):
            with self.subTest(value=value):
                self.assertRefused(lambda: self.gate().attempt(value, 2, self.now.isoformat(),
                                                               self.target), '40-character')
        self.assertEqual(self.status()['attempts'], [])

    def test_attempt_refuses_when_head_or_fingerprint_moved(self):
        candidate = self.prepare()
        commit = candidate['commit']
        self.write('src/main.rs', b'fn main() { dirty(); }\n')
        self.assertRefused(lambda: self.reserve(commit), 'not clean')
        self.git('checkout', '--', 'src/main.rs')
        self.write('untracked.txt', b'x\n')
        self.assertRefused(lambda: self.reserve(commit), 'untracked.txt')
        (self.root / 'untracked.txt').unlink()
        with mock.patch.object(SubmissionGate, 'fingerprint', return_value='f' * 64):
            self.assertRefused(lambda: self.reserve(commit), 'not the prepared ' + candidate['fingerprint'])
        self.change_runtime()
        self.assertRefused(lambda: self.reserve(commit), 'not the prepared commit')
        self.assertEqual(self.status()['attempts'], [])
        self.git('checkout', '-q', commit)
        self.assertEqual(self.reserve(commit)['commit'], commit)

    def test_same_commit_or_runtime_cannot_consume_a_second_attempt(self):
        candidate = self.prepare()
        attempt = self.reserve(candidate['commit'])
        self.resolve(attempt, 'BB-00000001', outcome='rejected')
        self.assertRefused(lambda: self.reserve(candidate['commit']), 'already consumed',
                           attempt['attempt_id'])
        self.write('README.md', b'documentation only\n')
        self.commit('docs')
        same_runtime = self.prepare()
        self.assertNotEqual(same_runtime['commit'], candidate['commit'])
        self.assertEqual(same_runtime['fingerprint'], candidate['fingerprint'])
        self.assertEqual({row['commit']: row['consumed_by'] for row in self.status()['candidates']},
                         {candidate['commit']: attempt['attempt_id'],
                          same_runtime['commit']: attempt['attempt_id']})
        self.assertRefused(lambda: self.reserve(same_runtime['commit']),
                           'fingerprint ' + candidate['fingerprint'], 'already consumed')
        self.change_runtime()
        self.assertEqual(self.reserve(self.prepare()['commit'])['state'], 'submitting')


class ReceiptAndResultTests(GateTest):
    """Receipts, imports and results are validated, idempotent and immutable."""

    def test_receipt_links_once_and_refuses_conflicts(self):
        gate = self.gate()
        gate.import_receipt('BB-0000000M', (self.now - timedelta(seconds=30)).isoformat(),
                            EVIDENCE)
        self.now += timedelta(seconds=1)
        gate.record_result('BB-0000000M', 'failed', None, EVIDENCE)
        attempt = self.reserve(self.prepare()['commit'])
        attempt_id = attempt['attempt_id']
        for receipt in ('BB-1234567', 'bb-00000001', 'BB-0000000a', 'BB-000000001', ''):
            with self.subTest(receipt=receipt):
                self.assertRefused(lambda: gate.record_receipt(attempt_id, receipt, EVIDENCE),
                                   'BB-')
        self.assertRefused(lambda: gate.record_receipt('A-unknown', 'BB-00000001', EVIDENCE),
                           "'A-unknown'")
        self.assertRefused(lambda: gate.record_receipt(attempt_id, 'BB-0000000M', EVIDENCE),
                           'already belongs to attempt M-BB-0000000M')
        self.assertRefused(lambda: gate.record_receipt(attempt_id, 'BB-00000001', 'local replay'),
                           '--evidence')
        linked = gate.record_receipt(attempt_id, 'BB-00000001', EVIDENCE)
        self.assertEqual((linked['state'], linked['receipt']), ('scoring', 'BB-00000001'))
        before = self.journal_bytes()
        self.assertEqual(gate.record_receipt(attempt_id, 'BB-00000001', EVIDENCE + '/again'),
                         linked)
        self.assertEqual(self.journal_bytes(), before)
        self.assertRefused(lambda: gate.record_receipt(attempt_id, 'BB-00000002', EVIDENCE),
                           'already has receipt BB-00000001')
        self.assertEqual(self.journal_bytes(), before)

    def test_evidence_must_be_an_organizer_buildathon_url(self):
        for good in ('https://www.joinbuilders.org/buildathon', EVIDENCE,
                     'https://www.joinbuilders.org/buildathon/standings/shopping-sessions'):
            self.assertEqual(gate_module.require_evidence(good), good)
        for bad in ('http://www.joinbuilders.org/buildathon',
                    'https://joinbuilders.org/buildathon',
                    'https://www.joinbuilders.org/other',
                    'https://www.joinbuilders.org/buildathonx',
                    'https://www.joinbuilders.org/buildathon?x=1',
                    'https://www.joinbuilders.org/buildathon?',
                    'https://www.joinbuilders.org/buildathon#top',
                    'https://user:pw@www.joinbuilders.org/buildathon',
                    'https://www.joinbuilders.org:8443/buildathon',
                    'https://www.joinbuilders.org/buildathon/../admin',
                    'https://www.joinbuilders.org/buildathon/%2e%2e/admin',
                    'https://www.joinbuilders.org/buildathon/x\\..\\..\\admin',
                    'https://www.joinbuilders.org/buildathon/%2e%2e%2fadmin',
                    'https://www.joinbuilders.org/buildathon/x%5C..%5Cadmin',
                    'https://www.joinbuilders.org/buildathon/ x',
                    'local replay', '', None):
            with self.subTest(bad=bad):
                self.assertRefused(lambda: gate_module.require_evidence(bad), '--evidence')

    def test_import_receipt_uses_historical_blobs_and_is_idempotent(self):
        prepared = self.prepare()
        self.change_runtime()
        gate = self.gate()
        submitted = (self.now - timedelta(minutes=5)).isoformat()
        imported = gate.import_receipt('BB-43CE87F9', submitted, EVIDENCE, prepared['commit'])
        self.assertEqual(imported['fingerprint'], prepared['fingerprint'])
        self.assertNotEqual(imported['fingerprint'], gate.fingerprint(self.head()))
        self.assertEqual((imported['attempt_id'], imported['state'], imported['manual']),
                         ('M-BB-43CE87F9', 'scoring', True))
        before = self.journal_bytes()
        self.assertEqual(gate.import_receipt('BB-43CE87F9', submitted, EVIDENCE,
                                             prepared['commit']), imported)
        self.assertEqual(self.journal_bytes(), before)
        for changes in (dict(submitted_at=(self.now - timedelta(minutes=6)).isoformat()),
                        dict(commit=None), dict(commit=self.head()),
                        dict(evidence=EVIDENCE + '/other')):
            arguments = dict(submitted_at=submitted, evidence=EVIDENCE, commit=prepared['commit'])
            arguments.update(changes)
            with self.subTest(changes=changes):
                self.assertRefused(lambda: gate.import_receipt('BB-43CE87F9', **arguments),
                                   'different details')
        self.assertEqual(self.journal_bytes(), before)

    def test_import_receipt_refuses_bad_input_and_inflight_attempts(self):
        gate = self.gate()
        head = self.head()
        for commit, fragment in (('0' * 40, '0' * 40), (head[:7], '40-character'),
                                 ('HEAD', '40-character')):
            with self.subTest(commit=commit):
                self.assertRefused(lambda: gate.import_receipt(
                    'BB-00000001', self.now.isoformat(), EVIDENCE, commit), fragment)
        self.assertRefused(lambda: gate.import_receipt(
            'BB-00000001', (self.now + timedelta(seconds=1)).isoformat(), EVIDENCE), 'in the future')
        self.assertRefused(lambda: gate.import_receipt('BB-00000001', '2026-11-21 01:00', EVIDENCE),
                           'timezone')
        self.assertEqual(self.status()['attempts'], [])
        attempt = self.reserve(self.prepare()['commit'])
        self.assertRefused(lambda: gate.import_receipt('BB-00000002', self.now.isoformat(), EVIDENCE),
                           attempt['attempt_id'])

    def test_imported_runtime_blocks_an_attempt_and_counts_in_the_hour(self):
        gate = self.gate()
        gate.import_receipt('BB-00000001', self.now.isoformat(), EVIDENCE, self.head())
        self.now += timedelta(seconds=1)
        gate.record_result('BB-00000001', 'scored', 1.4, EVIDENCE)
        self.assertEqual(self.status()['local_remaining'], 1)
        candidate = self.prepare()
        self.assertRefused(lambda: self.reserve(candidate['commit']), 'already consumed',
                           'M-BB-00000001')

    def test_result_requires_valid_outcome_and_never_changes(self):
        attempt = self.reserve(self.prepare()['commit'])
        gate = self.gate()
        self.assertRefused(lambda: gate.record_result('BB-00000001', 'scored', 1.0, EVIDENCE),
                           'not in the journal')
        gate.record_receipt(attempt['attempt_id'], 'BB-00000001', EVIDENCE)
        for score in (0, -1, float('nan'), float('inf'), True, None, '1.2', 10 ** 400):
            with self.subTest(score=score):
                self.assertRefused(lambda: gate.record_result('BB-00000001', 'scored', score,
                                                              EVIDENCE), 'finite --score above 0')
        self.assertRefused(lambda: gate.record_result('BB-00000001', 'rejected', 1.0, EVIDENCE),
                           'takes no --score')
        self.assertRefused(lambda: gate.record_result('BB-00000001', 'won', None, EVIDENCE),
                           "'won'")
        self.assertRefused(lambda: gate.record_result('BB-00000001', 'scored', 1.0,
                                                      'https://example.invalid/x'), '--evidence')
        self.assertEqual(self.status()['inflight']['state'], 'scoring')
        result = gate.record_result('BB-00000001', 'scored', 1.25, EVIDENCE)
        self.assertEqual((result['state'], result['score']), ('scored', 1.25))
        self.assertIsNone(self.status()['inflight'])
        before = self.journal_bytes()
        self.assertEqual(gate.record_result('BB-00000001', 'scored', 1.25, EVIDENCE), result)
        self.assertEqual(self.journal_bytes(), before)
        for outcome, score, evidence in (('scored', 1.2, EVIDENCE), ('failed', None, EVIDENCE),
                                         ('scored', 1.25, EVIDENCE + '/x')):
            with self.subTest(outcome=outcome, score=score, evidence=evidence):
                self.assertRefused(lambda: gate.record_result('BB-00000001', outcome, score,
                                                              evidence), 'results never change')
        self.assertEqual(self.journal_bytes(), before)

    def test_best_official_is_the_lowest_time_with_ties_to_the_earlier_submission(self):
        gate = self.gate()

        def record(receipt, minutes_before_start, outcome, score):
            self.now += timedelta(seconds=1)
            gate.import_receipt(receipt, (START - timedelta(minutes=minutes_before_start)).isoformat(),
                                EVIDENCE)
            self.now += timedelta(seconds=1)
            gate.record_result(receipt, outcome, score, EVIDENCE)
            best = self.status()['best_official']
            return best['receipt'] if best else None

        self.assertEqual(record('BB-00000001', 50, 'scored', 1.5), 'BB-00000001')
        self.assertEqual(record('BB-00000002', 40, 'scored', 1.2), 'BB-00000002')
        self.assertEqual(record('BB-00000003', 30, 'scored', 1.9), 'BB-00000002')
        self.assertEqual(record('BB-00000004', 20, 'scored', 1.2), 'BB-00000002')
        self.assertEqual(record('BB-00000005', 45, 'scored', 1.2), 'BB-00000005')
        self.assertEqual(record('BB-00000006', 10, 'rejected', None), 'BB-00000005')
        best = self.status()['best_official']
        self.assertEqual((best['score'], best['attempt_id']), (1.2, 'M-BB-00000005'))


class JournalTests(GateTest):
    """The journal is the only state; damage to it stops every command."""

    def test_corrupt_or_inconsistent_journal_refuses_every_command(self):
        candidate = self.prepare()
        attempt = self.reserve(candidate['commit'])
        good = self.journal_bytes()
        at = self.now.isoformat()
        corruptions = {
            'partial line': (b'{"action": "result"}', 'last line is incomplete'),
            'not json': (b'not json\n', 'corrupt at line 3'),
            'not an object': (b'[1, 2]\n', 'not a JSON object'),
            'non-finite number': (json.dumps(dict(
                action='result', at=at, attempt_id=attempt['attempt_id'],
                change=dict(state='scored', score=1.0))).replace('1.0', 'NaN').encode() + b'\n',
                'non-finite number NaN'),
            'non-ascii byte': (('{"action": "status' + chr(0xe9) + '"}\n').encode('utf-8'),
                               "'ascii' codec"),
            'unknown action': (json.dumps(dict(action='erase', at=at)).encode() + b'\n',
                               "unknown action 'erase'"),
            'unknown attempt': (json.dumps(dict(
                action='receipt', at=at, attempt_id='A-missing',
                change=dict(state='scoring', receipt='BB-00000009', receipt_evidence=EVIDENCE,
                            receipt_observed_at=at))).encode() + b'\n', "KeyError: 'A-missing'"),
            'extra change key': (json.dumps(dict(
                action='receipt', at=at, attempt_id=attempt['attempt_id'],
                change=dict(state='scoring', receipt='BB-00000009', receipt_evidence=EVIDENCE,
                            receipt_observed_at=at, commit='0' * 40))).encode() + b'\n',
                'receipt change has the keys'),
            'second inflight': (json.dumps(dict(action='import_receipt', at=at, attempt=dict(
                attempt_id='M-BB-00000008', state='scoring', manual=True, commit=None,
                fingerprint=None, submitted_at=at, receipt='BB-00000008',
                receipt_evidence=EVIDENCE, score=None))).encode() + b'\n', 'was inflight'),
            'deep nesting': (b'[' * 100000 + b']' * 100000 + b'\n', 'nests too deeply'),
            'huge score': ((json.dumps(dict(
                action='receipt', at=at, attempt_id=attempt['attempt_id'],
                change=dict(state='scoring', receipt='BB-00000009', receipt_evidence=EVIDENCE,
                            receipt_observed_at=at))) + '\n' + json.dumps(dict(
                action='result', at=at, attempt_id=attempt['attempt_id'],
                change=dict(state='scored', score=10 ** 400, result_evidence=EVIDENCE,
                            result_observed_at=at)))).encode() + b'\n', 'finite --score above 0'),
            'malformed prepare checks': (json.dumps(dict(action='prepare', at=at, candidate=dict(
                commit=candidate['commit'], fingerprint=candidate['fingerprint'],
                checks=[1]))).encode() + b'\n', 'check records'),
            'malformed failure checks': (json.dumps(dict(
                action='gate_failed', at=at, commit=None, revoked=False, error='x',
                checks='x')).encode() + b'\n', 'check records'),
        }
        actions = {
            'status': lambda gate: gate.status(),
            'prepare': lambda gate: gate.prepare(self.checks, self.target),
            'attempt': lambda gate: gate.attempt(candidate['commit'], 2, self.now.isoformat(),
                                                 self.target),
            'receipt': lambda gate: gate.record_receipt(attempt['attempt_id'], 'BB-00000001',
                                                        EVIDENCE),
            'import-receipt': lambda gate: gate.import_receipt('BB-00000002', at, EVIDENCE),
            'result': lambda gate: gate.record_result('BB-00000001', 'scored', 1.0, EVIDENCE),
        }
        for label, (corruption, fragment) in corruptions.items():
            self.journal.write_bytes(good + corruption)
            for name, action in actions.items():
                with self.subTest(corruption=label, action=name):
                    self.assertRefused(lambda: action(self.gate()), 'journal', fragment)
            self.assertEqual(self.journal_bytes(), good + corruption)

    def test_journal_lock_refuses_while_another_process_holds_it(self):
        lock_path = self.gate().lock_path
        holder = subprocess.Popen(
            python('import sys\n'
                   'import submission_gate\n'
                   'with submission_gate.journal_lock(sys.argv[1], 5):\n'
                   '    print("locked", flush=True)\n'
                   '    sys.stdin.read()\n', str(lock_path)),
            cwd=SCRIPT.parent, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
            encoding='utf-8', errors='replace')
        try:
            self.assertEqual(holder.stdout.readline().strip(), 'locked')
            started = time.monotonic()
            busy = SubmissionGate(self.root, now=lambda: self.now, lock_timeout_s=0.5)
            self.assertRefused(busy.status, 'held by another process')
            self.assertGreaterEqual(time.monotonic() - started, 0.4)
        finally:
            holder.stdin.close()
            holder.wait(timeout=30)
            holder.stdout.close()
        self.assertEqual(holder.returncode, 0)
        self.assertEqual(self.status()['attempts'], [])

    def test_every_command_refuses_a_root_that_is_not_the_top_of_a_work_tree(self):
        plain = self.base / 'plain'
        plain.mkdir()
        head = self.head()
        for root in (self.base / 'missing', plain, self.root / 'src'):
            gate = SubmissionGate(root, now=lambda: self.now, lock_timeout_s=5)
            actions = {
                'status': gate.status,
                'prepare': lambda: gate.prepare(self.checks, self.target),
                'attempt': lambda: gate.attempt(head, 2, self.now.isoformat(), self.target),
                'receipt': lambda: gate.record_receipt('A-unknown', 'BB-00000001', EVIDENCE),
                'import-receipt': lambda: gate.import_receipt('BB-00000001', self.now.isoformat(),
                                                              EVIDENCE),
                'result': lambda: gate.record_result('BB-00000001', 'failed', None, EVIDENCE),
            }
            for name, action in actions.items():
                with self.subTest(root=root.name, action=name):
                    self.assertRefused(action, '--root', root.name)
        self.assertFalse((self.base / 'missing').exists())
        self.assertEqual(list(plain.iterdir()), [])
        self.assertFalse((self.root / 'src' / 'artifacts').exists())
        self.assertFalse(self.journal.exists())

    def test_unexpected_errors_still_print_one_json_refusal(self):
        stdout, stderr = io.StringIO(), io.StringIO()
        with (mock.patch.object(SubmissionGate, 'status', side_effect=RuntimeError('probe failure')),
              contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr)):
            code = gate_module.main(['--root', str(self.root), 'status'])
        self.assertEqual(code, 2)
        refusal = json.loads(stdout.getvalue())
        self.assertEqual(set(refusal), {'error', 'ready'})
        self.assertIs(refusal['ready'], False)
        self.assertIn('RuntimeError: probe failure', refusal['error'])
        self.assertIn('Traceback', stderr.getvalue())

    def test_events_are_fsynced_before_success_is_reported(self):
        synced = []
        real_fsync = os.fsync

        def spy(descriptor):
            synced.append(descriptor)
            real_fsync(descriptor)

        with mock.patch.object(gate_module.os, 'fsync', spy):
            self.gate().import_receipt('BB-00000001', self.now.isoformat(), EVIDENCE)
        self.assertTrue(synced)
        self.now += timedelta(seconds=1)
        with mock.patch.object(gate_module.os, 'fsync', side_effect=OSError(5, 'fsync refused')):
            with self.assertRaises(OSError):
                self.gate().record_result('BB-00000001', 'failed', None, EVIDENCE)
            stdout = io.StringIO()
            submitted = (datetime.now(timezone.utc) - timedelta(seconds=5)).isoformat()
            with contextlib.redirect_stdout(stdout):
                code = gate_module.main(['--root', str(self.root), 'import-receipt', 'BB-00000002',
                                         '--submitted-at', submitted, '--evidence', EVIDENCE])
        self.assertEqual(code, 2)
        self.assertIn('fsync refused', json.loads(stdout.getvalue())['error'])

    def test_journal_is_ascii_json_lines_in_order(self):
        candidate = self.prepare()
        self.resolve(self.reserve(candidate['commit']), 'BB-00000001')
        data = self.journal_bytes()
        data.decode('ascii')
        self.assertTrue(data.endswith(b'\n'))
        events = [json.loads(line) for line in data.splitlines()]
        self.assertEqual([event['action'] for event in events],
                         ['prepare', 'attempt', 'receipt', 'result'])
        self.assertTrue(all(event['pid'] == os.getpid() for event in events))

    def test_command_line_prints_one_json_document_with_exit_codes(self):
        journal = self.base / 'elsewhere' / 'events.jsonl'
        prefix = [sys.executable, str(SCRIPT), '--root', str(self.root), '--journal', str(journal)]

        def run(*args):
            result = subprocess.run([*prefix, *args], capture_output=True, text=True,
                                    encoding='utf-8', errors='replace', timeout=120)
            return result.returncode, json.loads(result.stdout)

        code, state = run('status')
        self.assertEqual(code, 0)
        self.assertEqual(state['attempts'], [])
        head = self.head()
        submitted = (datetime.now(timezone.utc) - timedelta(seconds=5)).isoformat()
        code, imported = run('import-receipt', 'BB-0000CL11', '--submitted-at', submitted,
                             '--evidence', EVIDENCE, '--commit', head)
        self.assertEqual(code, 0, imported)
        code, result = run('result', 'BB-0000CL11', 'scored', '--score', '1.33',
                           '--evidence', EVIDENCE)
        self.assertEqual(code, 0, result)
        code, state = run('status')
        self.assertEqual(code, 0)
        self.assertEqual((state['best_official']['score'], state['best_official']['commit']),
                         (1.33, head))
        self.assertTrue(journal.is_file())
        self.assertFalse(self.journal.exists())
        for args in (('result', 'BB-0000CL11', 'scored', '--score', '2', '--evidence', EVIDENCE),
                     ('result', 'bad', 'scored', '--evidence', EVIDENCE),
                     ('attempt', head, '--server-remaining', 'two', '--server-checked-at', submitted),
                     ('launch',),
                     ()):
            with self.subTest(args=args):
                code, refusal = run(*args)
                self.assertEqual(code, 2)
                self.assertEqual(set(refusal), {'error', 'ready'})
                self.assertIs(refusal['ready'], False)


if __name__ == '__main__':
    unittest.main()
