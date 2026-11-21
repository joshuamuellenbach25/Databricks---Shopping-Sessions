"""Tests for scripts/ab_bench.py: statistics, verdict rule, run order and failure paths.

Builds, solver runs and table checks are injected, so no test needs cargo or a
dataset. Process tests start the current Python interpreter as a stand-in
solver or build tool, and the worktree tests use a throwaway local git
repository. Temporary folders live under artifacts/test-tmp in the repository.
"""
from contextlib import redirect_stderr, redirect_stdout
import errno
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

import ab_bench

TEMP_ROOT = Path(__file__).resolve().parents[1] / "artifacts" / "test-tmp"
PHASE = "lookups and links: 0.012 s (total 0.012 s)"
NO_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0)
MAKE_BINARY = (
    "import os, pathlib\n"
    "path = pathlib.Path(os.environ['CARGO_TARGET_DIR'], 'release', %r)\n"
    "path.parent.mkdir(parents=True, exist_ok=True)\n"
    "path.write_text('binary')\n" % ab_bench.BINARY)


def quiet(message):
    """Discard progress lines."""
    return None


def temp_dir(test):
    """A temporary folder under artifacts/test-tmp, removed when the test ends."""
    TEMP_ROOT.mkdir(parents=True, exist_ok=True)
    holder = tempfile.TemporaryDirectory(dir=TEMP_ROOT)
    test.addCleanup(holder.cleanup)
    return Path(holder.name)


def config(test, runs=3, warmup=1, min_gain=0.02):
    """Parsed arguments with a real empty data folder and scratch folder."""
    root = temp_dir(test)
    (root / "data").mkdir()
    return ab_bench.parse_args(["--baseline", "base", "--candidate", "cand", "--data", str(root / "data"),
                                "--scratch", str(root / "scratch"), "--runs", str(runs),
                                "--warmup", str(warmup), "--min-gain", str(min_gain)])


class FakePrepare:
    """Stand-in builder: returns fixed sides whose binaries are named bin-A and bin-B."""

    def __init__(self):
        self.requests = []

    def __call__(self, role, rev):
        self.requests.append((role, rev))
        label = "A" if role == "baseline" else "B"
        return ab_bench.Side(label=label, role=role, rev=rev, sha=label.lower() * 40, dirty=False,
                             source="src", target="target", binary=f"bin-{label}", build_seconds=0.0)


class FakeRunner:
    """Stand-in solver: scripted times and exit codes per side; records calls and reused folders."""

    def __init__(self, times, fail=None):
        self.times = {label: list(values) for label, values in times.items()}
        self.fail = fail or {}
        self.calls = []
        self.stale = []

    def __call__(self, command, data_dir, out_dir, threads):
        label = command[0][-1]
        out = Path(out_dir)
        if not out.is_dir() or any(out.iterdir()) or any(call[1] == str(out) for call in self.calls):
            self.stale.append(str(out))
        (out / "part-000.parquet").write_text(label)
        count = sum(call[0] == label for call in self.calls)
        self.calls.append((label, str(out), threads, str(data_dir)))
        code = self.fail.get((label, count), 0)
        stderr = PHASE + "\nlinked ids: 7\n" if code == 0 else "error: boom\n"
        return ab_bench.RunResult(self.times[label][count], code, stderr)


class FakeCompare:
    """Stand-in table check: scripted results in call order, MATCH once they run out.

    A scripted result is True (MATCH), False (a DIFFER report) or a (matched, output) pair.
    """

    def __init__(self, results=()):
        self.results = list(results)
        self.calls = []

    def __call__(self, out_a, out_b):
        self.calls.append((Path(out_a), Path(out_b)))
        result = self.results.pop(0) if self.results else True
        if isinstance(result, tuple):
            return result
        return result, "MATCH" if result else "user_daily: rows 3 vs 4\nDIFFER"


class StatisticsTests(unittest.TestCase):
    """Sign test, paired summary and run order."""

    def test_sign_test_p_values(self):
        self.assertEqual(ab_bench.sign_test_p(3, 3), 0.125)
        self.assertEqual(ab_bench.sign_test_p(7, 7), 1 / 128)
        self.assertEqual(ab_bench.sign_test_p(6, 7), 8 / 128)
        self.assertEqual(ab_bench.sign_test_p(0, 4), 1.0)
        self.assertEqual(ab_bench.sign_test_p(0, 0), 1.0)
        for wins, trials in ((4, 3), (-1, 3), (True, 3), (1.0, 3), (1, -1)):
            with self.subTest(wins=wins, trials=trials), self.assertRaises(ValueError):
                ab_bench.sign_test_p(wins, trials)

    def test_summary_statistics(self):
        stats = ab_bench.summarize([1.0, 1.2, 1.1], [0.9, 1.0, 1.05], 0.02)
        self.assertEqual(stats["deltas"], [-0.1, -0.2, -0.05])
        self.assertEqual(stats["delta_median"], -0.1)
        self.assertEqual(stats["delta_mean"], -0.116667)
        self.assertEqual((stats["a_median"], stats["a_min"]), (1.1, 1.0))
        self.assertEqual((stats["b_median"], stats["b_min"]), (1.0, 0.9))
        self.assertEqual((stats["b_wins"], stats["a_wins"], stats["ties"]), (3, 0, 0))
        self.assertEqual(stats["sign_test_p"], 0.125)
        self.assertEqual(stats["threshold_seconds"], 0.022)
        self.assertEqual(stats["verdict"], "faster")

    def test_summary_rejects_bad_times(self):
        for a_times, b_times in (([], []), ([1.0], [1.0, 2.0]), ([1.0], [float("nan")]),
                                 ([0.0], [1.0]), ([1.0], [True]), ([-1.0], [1.0])):
            with self.subTest(a=a_times, b=b_times), self.assertRaises(ValueError):
                ab_bench.summarize(a_times, b_times, 0.02)

    def test_round_orders_alternate(self):
        self.assertEqual(ab_bench.round_orders(5), ["AB", "BA", "AB", "BA", "AB"])
        self.assertEqual(ab_bench.round_orders(1), ["AB"])
        for bad in (0, -1, 2.0, True):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                ab_bench.round_orders(bad)

    def test_phase_lines_are_parsed(self):
        lines = [PHASE, "  people: 10 links in 0.001 s", "events dedup: 1.5 s (total 2 s)", "sessions: 9"]
        self.assertEqual(ab_bench.parse_phases(lines), [
            dict(name="lookups and links", seconds=0.012, total=0.012),
            dict(name="events dedup", seconds=1.5, total=2.0)])


class VerdictTests(unittest.TestCase):
    """The 80 percent and min-gain rule in both directions."""

    def test_faster_needs_eighty_percent_wins_and_enough_gain(self):
        verdict = ab_bench.verdict
        self.assertEqual(verdict([-0.1] * 6 + [0.05], 1.0, 0.02), "faster")
        self.assertEqual(verdict([-0.1] * 5 + [0.05] * 2, 1.0, 0.02), "no clear difference")
        self.assertEqual(verdict([-0.01] * 7, 1.0, 0.02), "no clear difference")
        self.assertEqual(verdict([-0.1] * 4 + [0.1], 1.0, 0.02), "faster")
        self.assertEqual(verdict([-0.1] * 2 + [0.1], 1.0, 0.02), "no clear difference")

    def test_slower_is_the_mirror_rule(self):
        verdict = ab_bench.verdict
        self.assertEqual(verdict([0.1] * 6 + [-0.05], 1.0, 0.02), "slower")
        self.assertEqual(verdict([0.1] * 5 + [-0.05] * 2, 1.0, 0.02), "no clear difference")
        self.assertEqual(verdict([0.01] * 7, 1.0, 0.02), "no clear difference")

    def test_threshold_is_strict(self):
        verdict = ab_bench.verdict
        self.assertEqual(verdict([-0.25] * 5, 1.0, 0.25), "no clear difference")
        self.assertEqual(verdict([0.25] * 5, 1.0, 0.25), "no clear difference")
        self.assertEqual(verdict([-0.5] * 5, 1.0, 0.25), "faster")
        self.assertEqual(verdict([-0.001] * 5, 1.0, 0.0), "faster")

    def test_zero_delta_is_a_win_for_neither_side(self):
        verdict = ab_bench.verdict
        self.assertEqual(verdict([0.0, -1.0, -1.0, -1.0, -1.0], 2.0, 0.02), "faster")
        self.assertEqual(verdict([0.0, 0.0, -1.0, -1.0, -1.0], 2.0, 0.02), "no clear difference")
        stats = ab_bench.summarize([1.0, 1.0, 1.0], [1.0, 0.5, 0.5], 0.02)
        self.assertEqual((stats["b_wins"], stats["ties"], stats["sign_test_p"]), (2, 1, 0.25))

    def test_invalid_inputs_raise(self):
        for args in (([], 1.0, 0.02), ([float("nan")], 1.0, 0.02), ([-0.1], 0.0, 0.02),
                     ([-0.1], 1.0, -0.01), ([-0.1], 1.0, float("inf"))):
            with self.subTest(args=args), self.assertRaises(ValueError):
                ab_bench.verdict(*args)


class BenchTests(unittest.TestCase):
    """Run order, table checks and failure paths with an injected builder, runner and check."""

    def test_rounds_alternate_after_warmup_and_warmup_is_discarded(self):
        cfg = config(self, runs=4, warmup=1)
        runner = FakeRunner({"A": [9.0, 1.0, 1.1, 1.2, 1.3], "B": [9.0, 0.9, 1.0, 1.1, 1.2]})
        prepare = FakePrepare()
        compare = FakeCompare()
        report = ab_bench.bench(cfg, prepare, runner, compare, log=quiet)
        self.assertEqual(prepare.requests, [("baseline", "base"), ("candidate", "cand")])
        self.assertEqual([call[0] for call in runner.calls], ["A", "B", "A", "B", "B", "A", "A", "B", "B", "A"])
        folders = [Path(call[1]) for call in runner.calls]
        self.assertEqual(compare.calls, [(folders[2], folders[3]), (folders[9], folders[8])])
        self.assertEqual(runner.stale, [])
        self.assertEqual({call[2] for call in runner.calls}, {8})
        self.assertEqual({call[3] for call in runner.calls}, {str(cfg.data)})
        self.assertEqual(report["a_times"], [1.0, 1.1, 1.2, 1.3])
        self.assertEqual(report["b_times"], [0.9, 1.0, 1.1, 1.2])
        self.assertEqual([row["order"] for row in report["rounds"]], ["AB", "BA", "AB", "BA"])
        self.assertEqual(report["verdict"], "faster")
        self.assertEqual([record["kind"] for record in report["run_details"]], ["warmup"] * 2 + ["measured"] * 8)
        self.assertEqual(report["run_details"][0]["phases"], [dict(name="lookups and links", seconds=0.012, total=0.012)])
        self.assertEqual(report["run_details"][0]["stderr_lines"], [PHASE, "linked ids: 7"])
        self.assertIsNone(report["outputs_left"])
        self.assertEqual(list((cfg.scratch / "runs").iterdir()), [])

    def test_tables_checked_after_first_and_last_round(self):
        cfg = config(self, runs=5, warmup=0)
        runner = FakeRunner({"A": [1.0] * 5, "B": [1.0] * 5})
        compare = FakeCompare()
        report = ab_bench.bench(cfg, FakePrepare(), runner, compare, log=quiet)
        folders = [Path(call[1]) for call in runner.calls]
        self.assertEqual(compare.calls, [(folders[0], folders[1]), (folders[8], folders[9])])
        self.assertEqual(report["tables_match_after_rounds"], [1, 5])
        self.assertEqual(report["verdict"], "no clear difference")
        self.assertEqual(report["ties"], 5)

    def test_single_round_is_checked_once(self):
        cfg = config(self, runs=1, warmup=0)
        compare = FakeCompare()
        report = ab_bench.bench(cfg, FakePrepare(), FakeRunner({"A": [1.0], "B": [0.5]}), compare, log=quiet)
        self.assertEqual(len(compare.calls), 1)
        self.assertEqual(report["tables_match_after_rounds"], [1])
        self.assertEqual(report["verdict"], "faster")

    def test_nonzero_exit_fails_and_keeps_the_output(self):
        cfg = config(self, runs=3, warmup=1)
        runner = FakeRunner({"A": [1.0] * 4, "B": [1.0] * 4}, fail={("B", 2): 3})
        with self.assertRaises(ab_bench.BenchError) as caught:
            ab_bench.bench(cfg, FakePrepare(), runner, FakeCompare(), log=quiet)
        message = str(caught.exception)
        self.assertIn("B (candidate cand) exited with status 3 in measured round 2", message)
        self.assertIn("error: boom", message)
        self.assertEqual([call[0] for call in runner.calls], ["A", "B", "A", "B", "B"])
        self.assertTrue(Path(runner.calls[-1][1]).is_dir())

    def test_table_mismatch_after_first_round_stops_the_bench(self):
        cfg = config(self, runs=4, warmup=1)
        runner = FakeRunner({"A": [1.0] * 5, "B": [1.0] * 5})
        with self.assertRaises(ab_bench.BenchError) as caught:
            ab_bench.bench(cfg, FakePrepare(), runner, FakeCompare([False]), log=quiet)
        message = str(caught.exception)
        self.assertIn("tables differ after round 1", message)
        self.assertIn("DIFFER", message)
        self.assertEqual(len(runner.calls), 4)
        self.assertTrue(Path(runner.calls[-1][1]).is_dir())

    def test_table_mismatch_after_last_round_fails(self):
        cfg = config(self, runs=3, warmup=0)
        runner = FakeRunner({"A": [1.0] * 3, "B": [0.5] * 3})
        with self.assertRaises(ab_bench.BenchError) as caught:
            ab_bench.bench(cfg, FakePrepare(), runner, FakeCompare([True, False]), log=quiet)
        self.assertIn("tables differ after round 3", str(caught.exception))

    def test_table_check_without_a_verdict_is_not_called_a_difference(self):
        cfg = config(self, runs=2, warmup=0)
        crash = (False, "Traceback (most recent call last):\nValueError: no parquet in out")
        with self.assertRaises(ab_bench.BenchError) as caught:
            ab_bench.bench(cfg, FakePrepare(), FakeRunner({"A": [1.0] * 2, "B": [1.0] * 2}),
                           FakeCompare([crash]), log=quiet)
        message = str(caught.exception)
        self.assertIn("table check gave no MATCH or DIFFER verdict after round 1", message)
        self.assertIn("ValueError: no parquet in out", message)
        self.assertNotIn("tables differ", message)

    def test_json_path_naming_a_folder_is_refused_before_any_run(self):
        cfg = config(self)
        folder = cfg.scratch.parent / "reports"
        folder.mkdir()
        argv = ["--baseline", "base", "--candidate", "cand", "--data", str(cfg.data), "--scratch", str(cfg.scratch),
                "--runs", "1", "--warmup", "0", "--json", str(folder)]
        runner = FakeRunner({"A": [1.0], "B": [1.0]})
        with redirect_stderr(io.StringIO()), redirect_stdout(io.StringIO()), self.assertRaises(SystemExit) as caught:
            ab_bench.main(argv, prepare=FakePrepare(), run=runner, compare=FakeCompare(), log=quiet)
        self.assertEqual(caught.exception.code, 2)
        self.assertEqual(runner.calls, [])
        self.assertEqual(sorted(path.name for path in cfg.scratch.parent.iterdir()), ["data", "reports"])

    def test_failed_json_rename_leaves_no_temporary_file(self):
        root = temp_dir(self)
        target = root / "report.json"
        target.mkdir()
        with self.assertRaises(OSError):
            ab_bench.write_json(target, {"verdict": "faster"})
        self.assertEqual([path.name for path in root.iterdir()], ["report.json"])

    def test_build_failure_exits_one_with_the_message(self):
        def broken(role, rev):
            raise ab_bench.BenchError("build failed in src (exit 101):\nerror[E0425]: cannot find value")
        cfg = config(self)
        argv = ["--baseline", "base", "--candidate", "cand", "--data", str(cfg.data), "--scratch", str(cfg.scratch)]
        errors = io.StringIO()
        with redirect_stderr(errors), redirect_stdout(io.StringIO()):
            code = ab_bench.main(argv, prepare=broken, run=FakeRunner({}), compare=FakeCompare(), log=quiet)
        self.assertEqual(code, 1)
        self.assertIn("ab_bench FAILED", errors.getvalue())
        self.assertIn("error[E0425]", errors.getvalue())

    def test_main_prints_summary_and_writes_json(self):
        cfg = config(self)
        report_path = cfg.scratch.parent / "reports" / "ab.json"
        argv = ["--baseline", "base", "--candidate", "cand", "--data", str(cfg.data), "--scratch", str(cfg.scratch),
                "--runs", "3", "--json", str(report_path)]
        runner = FakeRunner({"A": [2.0, 1.0, 1.02, 0.98], "B": [2.0, 0.8, 0.81, 0.79]})
        printed = io.StringIO()
        with redirect_stdout(printed), redirect_stderr(io.StringIO()):
            code = ab_bench.main(argv, prepare=FakePrepare(), run=runner, compare=FakeCompare(), log=quiet)
        self.assertEqual(code, 0)
        report = json.loads(report_path.read_text(encoding="utf-8"))
        self.assertEqual(report["verdict"], "faster")
        self.assertEqual(report["a_times"], [1.0, 1.02, 0.98])
        self.assertEqual(report["b_times"], [0.8, 0.81, 0.79])
        self.assertEqual((report["b_wins"], report["sign_test_p"]), (3, 0.125))
        self.assertEqual(report["baseline"]["rev"], "base")
        self.assertEqual(report["candidate"]["sha"], "b" * 40)
        self.assertEqual(report["created_worktrees"], [])
        self.assertEqual(len(report["run_details"]), 8)
        text = printed.getvalue()
        self.assertIn("verdict: faster", text)
        self.assertIn("tables: MATCH after rounds 1 and 3", text)
        self.assertIn("B won 3 of 3 rounds", text)


class CompareTests(unittest.TestCase):
    """Reading the table check result."""

    def test_match_needs_exit_zero_and_a_final_match_line(self):
        self.assertTrue(ab_bench.compare_matched(0, "user_daily: rows 3 vs 3\nMATCH\n"))
        self.assertFalse(ab_bench.compare_matched(1, "user_daily: rows 3 vs 4\nDIFFER\n"))
        self.assertFalse(ab_bench.compare_matched(0, "DIFFER\n"))
        self.assertFalse(ab_bench.compare_matched(0, ""))
        self.assertFalse(ab_bench.compare_matched(1, "MATCH\n"))
        self.assertFalse(ab_bench.compare_matched(0, "MATCHED\n"))
        self.assertFalse(ab_bench.compare_matched(0, "MATCH\ntraceback follows\n"))

    def test_missing_compare_script_is_named(self):
        root = temp_dir(self)
        with self.assertRaises(ab_bench.BenchError) as caught:
            ab_bench.require_compare(root / "compare.py")
        self.assertIn("compare.py", str(caught.exception))


class ProcessTests(unittest.TestCase):
    """The real runner and build wrapper, with the current Python as the child process."""

    def test_run_solver_sets_environment_and_keeps_stderr(self):
        root = temp_dir(self)
        out = root / "out"
        out.mkdir()
        script = ("import json, os, sys\n"
                  "values = {key: os.environ[key] for key in ('DATA_DIR', 'OUTPUT_DIR', 'NCPU')}\n"
                  "with open(os.path.join(os.environ['OUTPUT_DIR'], 'env.json'), 'w') as handle:\n"
                  "    json.dump(values, handle)\n"
                  "print('lookups and links: 0.001 s (total 0.001 s)', file=sys.stderr)\n")
        result = ab_bench.run_solver([sys.executable, "-c", script], root / "data", out, 6)
        self.assertEqual(result.returncode, 0)
        self.assertGreater(result.seconds, 0)
        values = json.loads((out / "env.json").read_text())
        self.assertEqual(values, {"DATA_DIR": str(root / "data"), "OUTPUT_DIR": str(out), "NCPU": "6"})
        self.assertIn("lookups and links: 0.001 s", result.stderr)

    def test_run_solver_reports_exit_status_and_stops_at_timeout(self):
        root = temp_dir(self)
        result = ab_bench.run_solver([sys.executable, "-c", "import sys; sys.exit(5)"], root, root, 1)
        self.assertEqual(result.returncode, 5)
        with self.assertRaises(ab_bench.BenchError) as caught:
            ab_bench.run_solver([sys.executable, "-c", "import time; time.sleep(30)"], root, root, 1, timeout=0.5)
        self.assertIn("was stopped", str(caught.exception))
        with self.assertRaises(ab_bench.BenchError) as caught:
            ab_bench.run_solver([str(root / "no-such-binary")], root, root, 1)
        self.assertIn("cannot start", str(caught.exception))

    def test_build_failure_shows_the_log_tail(self):
        root = temp_dir(self)
        script = ("import sys\n"
                  "for index in range(50):\n"
                  "    print('line', index)\n"
                  "print('error[E0308]: mismatched types')\n"
                  "sys.exit(101)\n")
        with self.assertRaises(ab_bench.BenchError) as caught:
            ab_bench.cargo_build([sys.executable, "-c", script], root, root / "target", root / "build.log")
        message = str(caught.exception)
        self.assertIn("exit 101", message)
        self.assertIn("error[E0308]: mismatched types", message)
        self.assertIn("line 49", message)
        self.assertNotIn("line 0\n", message)
        self.assertIn("error[E0308]", (root / "build.log").read_text())

    def test_build_uses_the_target_dir_and_requires_the_binary(self):
        root = temp_dir(self)
        binary, seconds = ab_bench.cargo_build([sys.executable, "-c", MAKE_BINARY], root, root / "target",
                                               root / "build.log")
        self.assertEqual(binary, root / "target" / "release" / ab_bench.BINARY)
        self.assertGreater(seconds, 0)
        with self.assertRaises(ab_bench.BenchError) as caught:
            ab_bench.cargo_build([sys.executable, "-c", "pass"], root, root / "other", root / "build2.log")
        self.assertIn("is missing", str(caught.exception))


class LockTests(unittest.TestCase):
    """One bench per scratch folder."""

    def test_second_holder_is_refused_until_release(self):
        root = temp_dir(self)
        with ab_bench.scratch_lock(root):
            with self.assertRaises(ab_bench.BenchError) as caught:
                with ab_bench.scratch_lock(root):
                    self.fail("second lock acquired")
            self.assertIn("another ab_bench run holds", str(caught.exception))
        with ab_bench.scratch_lock(root) as path:
            self.assertTrue(path.is_file())

    def test_lock_failure_other_than_contention_names_the_error(self):
        root = temp_dir(self)
        module, name = (ab_bench.msvcrt, "locking") if os.name == "nt" else (ab_bench.fcntl, "flock")
        failure = OSError(errno.ENOLCK, "No locks available")
        with mock.patch.object(module, name, side_effect=failure), self.assertRaises(ab_bench.BenchError) as caught:
            with ab_bench.scratch_lock(root):
                self.fail("lock acquired")
        self.assertIn("No locks available", str(caught.exception))
        self.assertNotIn("another ab_bench run", str(caught.exception))


class ArgumentTests(unittest.TestCase):
    """Command-line checks."""

    def setUp(self):
        self.root = temp_dir(self)
        (self.root / "data").mkdir()
        self.base = ["--baseline", "a", "--candidate", "b", "--data", str(self.root / "data"),
                     "--scratch", str(self.root / "scratch")]

    def exit_code(self, argv):
        with redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as caught:
            ab_bench.parse_args(argv)
        return caught.exception.code

    def test_invalid_values_exit_with_status_two(self):
        for extra in (["--runs", "0"], ["--warmup", "-1"], ["--threads", "0"], ["--min-gain", "-0.1"],
                      ["--min-gain", "nan"], ["--min-gain", "1.5"], ["--runs", "x"], ["--baseline=-x"],
                      ["--json", ""], ["--json", str(self.root)]):
            with self.subTest(extra=extra):
                self.assertEqual(self.exit_code(self.base + extra), 2)
        self.assertEqual(self.exit_code(self.base[:4] + ["--data", str(self.root / "missing")] + self.base[6:]), 2)
        self.assertEqual(self.exit_code(self.base[:6]), 2)

    def test_defaults(self):
        args = ab_bench.parse_args(self.base)
        self.assertEqual((args.runs, args.warmup, args.threads, args.min_gain, args.json), (7, 1, 8, 0.02, None))
        self.assertEqual(args.repo, Path(ab_bench.__file__).resolve().parents[1])
        self.assertTrue(args.scratch.is_absolute())


class WorktreeTests(unittest.TestCase):
    """Revision resolution, worktree reuse and refusal, with a throwaway git repository."""

    def setUp(self):
        if shutil.which("git") is None:
            self.skipTest("git is not installed")
        self.root = temp_dir(self)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.git("init", "-q")
        self.first = self.commit("one\n")
        self.second = self.commit("two\n")
        self.scratch = self.root / "scratch"
        self.scratch.mkdir()

    def git(self, *args, cwd=None):
        """Run git in the throwaway repository and return its stdout."""
        done = subprocess.run(["git", "-c", "user.name=ab-bench-test", "-c", "user.email=ab-bench-test",
                               "-c", "commit.gpgsign=false", *args], cwd=str(cwd or self.repo),
                              capture_output=True, text=True, errors="replace", creationflags=NO_WINDOW)
        if done.returncode != 0:
            self.fail(f"git {args} failed: {done.stderr}")
        return done.stdout.strip()

    def commit(self, content):
        """Commit one version of file.txt and return its SHA."""
        (self.repo / "file.txt").write_text(content)
        self.git("add", "file.txt")
        self.git("commit", "-q", "-m", "fixture")
        return self.git("rev-parse", "HEAD")

    def builder(self, command=None):
        """A Builder on the throwaway repository with a stand-in build command."""
        return ab_bench.Builder(self.repo, self.scratch, log=quiet,
                                cargo_command=command or [sys.executable, "-c", MAKE_BINARY])

    def test_revisions_resolve_to_full_shas_and_unknown_ones_are_named(self):
        builder = self.builder()
        self.assertEqual(builder.resolve(self.first[:7]), self.first)
        self.assertEqual(builder.resolve("HEAD"), self.second)
        with self.assertRaises(ab_bench.BenchError) as caught:
            builder.resolve("no-such-branch")
        self.assertIn("'no-such-branch'", str(caught.exception))

    def test_worktree_is_added_once_then_reused(self):
        builder = self.builder()
        path = builder.worktree(self.first)
        self.assertEqual(path, self.scratch / f"wt-{self.first[:12]}")
        self.assertEqual((path / "file.txt").read_text(), "one\n")
        self.assertEqual(builder.created, [str(path)])
        again = self.builder()
        self.assertEqual(again.worktree(self.first), path)
        self.assertEqual(again.created, [])

    def test_worktree_at_another_sha_with_changes_or_not_a_worktree_is_refused(self):
        wrong = self.scratch / f"wt-{self.second[:12]}"
        self.git("worktree", "add", "--detach", str(wrong), self.first)
        with self.assertRaises(ab_bench.BenchError) as caught:
            self.builder().worktree(self.second)
        self.assertIn("expected " + self.second, str(caught.exception))
        path = self.builder().worktree(self.first)
        (path / "extra.txt").write_text("local change\n")
        with self.assertRaises(ab_bench.BenchError) as caught:
            self.builder().worktree(self.first)
        self.assertIn("local changes", str(caught.exception))
        shutil.rmtree(wrong)
        wrong.mkdir()
        with self.assertRaises(ab_bench.BenchError) as caught:
            self.builder().worktree(self.second)
        self.assertIn("not a git worktree", str(caught.exception))

    def test_builder_builds_a_revision_and_the_working_tree(self):
        builder = self.builder()
        a = builder("baseline", self.first[:7])
        self.assertEqual((a.label, a.sha, a.dirty), ("A", self.first, False))
        self.assertTrue(Path(a.binary).is_file())
        self.assertEqual(Path(a.target).name, f"target-{self.first[:12]}")
        self.assertEqual(Path(a.source), self.scratch / f"wt-{self.first[:12]}")
        (self.repo / "file.txt").write_text("three\n")
        b = builder("candidate", ab_bench.WORKTREE)
        self.assertEqual((b.label, b.sha, b.dirty), ("B", self.second, True))
        self.assertTrue(ab_bench.same_path(b.source, self.repo))
        self.assertEqual(Path(b.target).name, "target-worktree")
        self.assertTrue((self.scratch / "build-target-worktree.log").is_file())

    def test_paths_with_non_ascii_characters(self):
        accented = "caf" + chr(0xE9)
        self.repo = self.root / accented / "repo"
        self.scratch = self.root / accented / "scratch"
        self.repo.mkdir(parents=True)
        self.scratch.mkdir()
        self.git("init", "-q")
        first = self.commit("one\n")
        builder = self.builder()
        self.assertTrue(ab_bench.same_path(builder.repo, self.repo))
        side = builder("baseline", "HEAD")
        self.assertEqual((side.sha, Path(side.source)), (first, self.scratch / f"wt-{first[:12]}"))
        self.assertEqual(self.builder().worktree(first), Path(side.source))
        self.assertTrue(Path(builder("candidate", ab_bench.WORKTREE).binary).is_file())

    def test_working_tree_that_changes_during_its_build_is_refused(self):
        commit = ("import subprocess\n"
                  "subprocess.run(['git', '-c', 'user.name=ab-bench-test', '-c', 'user.email=ab-bench-test',"
                  " '-c', 'commit.gpgsign=false', 'commit', '-q', '--allow-empty', '-m', 'moved'], check=True)\n")
        edit = "open('extra.txt', 'w').write('new')\n"
        for name, prefix in (("HEAD moves", commit), ("a local change appears", edit)):
            with self.subTest(name):
                with self.assertRaises(ab_bench.BenchError) as caught:
                    self.builder([sys.executable, "-c", prefix + MAKE_BINARY])("candidate", ab_bench.WORKTREE)
                self.assertIn("changed during the build", str(caught.exception))

    def test_second_working_tree_side_keeps_the_identity_of_its_binary(self):
        builder = self.builder()
        a = builder("baseline", ab_bench.WORKTREE)
        (self.repo / "file.txt").write_text("three\n")
        b = builder("candidate", ab_bench.WORKTREE)
        self.assertEqual((b.sha, b.dirty, b.binary, b.build_seconds), (a.sha, False, a.binary, 0.0))

    def test_status_checks_leave_the_index_untouched(self):
        index = self.repo / ".git" / "index"
        later = time.time() + 3600
        os.utime(self.repo / "file.txt", (later, later))
        before = index.read_bytes()
        side = self.builder()("candidate", ab_bench.WORKTREE)
        self.assertFalse(side.dirty)
        self.assertEqual(index.read_bytes(), before)


if __name__ == "__main__":
    unittest.main()
