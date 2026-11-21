#!/usr/bin/env python3
"""Paired A/B run-time benchmark for two builds of the solver.

Builds a baseline (A) and a candidate (B), times both on one dataset in
alternating order, checks that they write identical tables, and reports
whether B is faster than A, slower, or not clearly different.

Usage:
  python scripts/ab_bench.py --baseline REV --candidate REV --data DIR --scratch DIR
      [--runs 7] [--warmup 1] [--threads 8] [--min-gain 0.02] [--json PATH] [--repo PATH]

REV is a git revision of the repository; the literal WORKTREE stands for the
working tree as it is now, uncommitted changes included.

Builds: a revision resolves to its full commit SHA and gets a detached git
worktree at SCRATCH/wt-SHA12 (the first 12 characters of the SHA), reused when
it already sits clean at that SHA. It builds with cargo build --release --locked
and CARGO_TARGET_DIR=SCRATCH/target-SHA12. WORKTREE builds the repository
itself into SCRATCH/target-worktree and stops the bench when HEAD or the
presence of local changes moves during that build. A failed build stops the
bench and shows the tail of its log. Worktrees stay in place for later runs;
none is removed.

Runs: each warmup round runs A, then B, and is left out of the statistics. The
measured rounds run in the order AB, BA, AB, ... so a slow drift of the
machine reaches both builds alike. Every run gets a fresh empty output folder
and DATA_DIR, OUTPUT_DIR and NCPU in its environment; its time is the wall
clock around the whole process. scripts/compare.py, run with this interpreter,
checks the A and B tables after the first and after the last measured round,
and anything but MATCH stops the bench. A failed run or check keeps its output
folders; a completed bench deletes them.

Verdict: faster when B won at least 80 percent of the rounds and the median
paired delta (B minus A) is below minus min-gain times the median time of A;
slower by the mirror rule; otherwise no clear difference. The report adds a
one-sided exact sign-test p-value for B being faster.

Exit status: 0 when the bench completed, whatever the verdict; 1 when a build,
a run, the table check or the scratch lock failed; 2 for invalid arguments.
"""
import argparse
from contextlib import contextmanager
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
import errno
import importlib.util
import json
import math
import os
from pathlib import Path
import platform
import re
import shutil
import statistics
import subprocess
import sys
import time

if os.name == "nt":
    import msvcrt
else:
    import fcntl

HERE = Path(__file__).resolve().parent
DEFAULT_REPO = HERE.parent
COMPARE_SCRIPT = HERE / "compare.py"
WORKTREE = "WORKTREE"
BINARY = "solver.exe" if sys.platform in ("win32", "cygwin", "msys") else "solver"
CARGO_ARGS = ("build", "--release", "--locked")
RUN_TIMEOUT = 900.0
TAIL_LINES = 30
STDERR_KEEP = 200
NO_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0)
PHASE_LINE = re.compile(r"(?P<name>[^:]+): (?P<seconds>\d+(?:\.\d+)?) s \(total (?P<total>\d+(?:\.\d+)?) s\)")
FULL_SHA = re.compile(r"[0-9a-f]{40}|[0-9a-f]{64}")
LOCK_BUSY = frozenset((errno.EACCES, errno.EAGAIN, errno.EWOULDBLOCK, errno.EDEADLK))


class BenchError(RuntimeError):
    """A build, run, table check or setup step failed; the message names the cause."""


@dataclass
class Side:
    """One build under test: the revision, where it was built and how long the build took."""

    label: str
    role: str
    rev: str
    sha: str
    dirty: bool
    source: str
    target: str
    binary: str
    build_seconds: float


@dataclass
class RunResult:
    """Wall-clock seconds, exit status and decoded stderr of one solver process."""

    seconds: float
    returncode: int
    stderr: str


def log_line(message):
    """Print one progress line to stderr."""
    print(message, file=sys.stderr, flush=True)


def tail(text, count=TAIL_LINES):
    """The last `count` non-empty lines of text, or a marker when there are none."""
    lines = [line.rstrip() for line in text.splitlines() if line.strip()]
    return "\n".join(lines[-count:]) if lines else "(no output)"


def is_number(value):
    """True for a finite int or float; booleans do not count."""
    return type(value) in (int, float) and math.isfinite(value)


def check_times(values, what):
    """Raise ValueError unless values is a non-empty list of finite positive seconds."""
    if not values:
        raise ValueError(f"{what}: no times")
    for value in values:
        if not is_number(value) or value <= 0:
            raise ValueError(f"{what}: invalid time {value!r}")


def sign_test_p(wins, trials):
    """One-sided exact sign-test p-value.

    The chance of at least `wins` successes in `trials` fair coin flips, that
    is, of B winning that often if neither build were faster. Rounds with a
    zero delta are left out of `trials`.
    """
    if type(wins) is not int or type(trials) is not int or not 0 <= wins <= trials:
        raise ValueError(f"invalid sign test counts: wins={wins!r}, trials={trials!r}")
    if trials == 0:
        return 1.0
    return sum(math.comb(trials, k) for k in range(wins, trials + 1)) / 2 ** trials


def verdict(deltas, median_a, min_gain):
    """Classify paired deltas (B minus A, in seconds) as faster, slower or no clear difference.

    faster: B won at least 80 percent of the rounds and the median delta is
    below -min_gain * median_a. slower: A won at least 80 percent of the rounds
    and the median delta is above +min_gain * median_a. A zero delta is a win
    for neither side.
    """
    if not deltas:
        raise ValueError("verdict needs at least one paired delta")
    for delta in deltas:
        if not is_number(delta):
            raise ValueError(f"invalid paired delta {delta!r}")
    if not is_number(median_a) or median_a <= 0:
        raise ValueError(f"invalid median time of A {median_a!r}")
    if not is_number(min_gain) or min_gain < 0:
        raise ValueError(f"invalid min gain {min_gain!r}")
    rounds = len(deltas)
    b_wins = sum(delta < 0 for delta in deltas)
    a_wins = sum(delta > 0 for delta in deltas)
    middle = statistics.median(deltas)
    threshold = min_gain * median_a
    if 5 * b_wins >= 4 * rounds and middle < -threshold:
        return "faster"
    if 5 * a_wins >= 4 * rounds and middle > threshold:
        return "slower"
    return "no clear difference"


def summarize(a_times, b_times, min_gain):
    """Paired statistics and verdict; a_times[i] and b_times[i] come from the same round.

    Derived values are rounded to 6 decimals; the verdict uses unrounded values.
    """
    check_times(a_times, "A")
    check_times(b_times, "B")
    if len(a_times) != len(b_times):
        raise ValueError(f"unpaired times: {len(a_times)} for A, {len(b_times)} for B")
    deltas = [b - a for a, b in zip(a_times, b_times)]
    median_a = statistics.median(a_times)
    delta_median = statistics.median(deltas)
    b_wins = sum(delta < 0 for delta in deltas)
    a_wins = sum(delta > 0 for delta in deltas)
    return dict(
        a_times=list(a_times), a_median=round(median_a, 6), a_min=min(a_times),
        b_times=list(b_times), b_median=round(statistics.median(b_times), 6), b_min=min(b_times),
        deltas=[round(delta, 6) for delta in deltas], delta_median=round(delta_median, 6),
        delta_mean=round(statistics.fmean(deltas), 6),
        delta_median_fraction=round(delta_median / median_a, 6),
        b_wins=b_wins, a_wins=a_wins, ties=len(deltas) - a_wins - b_wins,
        sign_test_p=sign_test_p(b_wins, a_wins + b_wins),
        threshold_seconds=round(min_gain * median_a, 6),
        verdict=verdict(deltas, median_a, min_gain))


def round_orders(rounds):
    """Run order of each measured round: AB, BA, AB, ..."""
    if type(rounds) is not int or rounds < 1:
        raise ValueError(f"rounds must be a positive integer, got {rounds!r}")
    return ["AB" if index % 2 == 0 else "BA" for index in range(rounds)]


def parse_phases(lines):
    """Phase timings from solver stderr lines shaped 'NAME: 0.123 s (total 0.456 s)'."""
    phases = []
    for line in lines:
        match = PHASE_LINE.fullmatch(line)
        if match:
            phases.append(dict(name=match["name"], seconds=float(match["seconds"]),
                               total=float(match["total"])))
    return phases


def run_solver(command, data_dir, out_dir, threads, timeout=RUN_TIMEOUT):
    """Run one solver process with DATA_DIR, OUTPUT_DIR and NCPU set, and time it.

    command is the argv list, binary first. The time is time.perf_counter
    around the whole process, rounded to microseconds. stdout is discarded and
    stderr kept. A process still running after `timeout` seconds is stopped
    and raises BenchError.
    """
    env = dict(os.environ, DATA_DIR=str(data_dir), OUTPUT_DIR=str(out_dir), NCPU=str(threads))
    start = time.perf_counter()
    try:
        done = subprocess.run(list(command), env=env, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                              stderr=subprocess.PIPE, timeout=timeout, creationflags=NO_WINDOW)
    except subprocess.TimeoutExpired as error:
        raise BenchError(f"{command[0]} still ran after {timeout:g} s on {data_dir} and was stopped") from error
    except OSError as error:
        raise BenchError(f"cannot start {command[0]}: {error}") from error
    seconds = time.perf_counter() - start
    return RunResult(round(seconds, 6), done.returncode, done.stderr.decode("utf-8", errors="replace"))


def compare_matched(returncode, stdout):
    """True only when the table check exited 0 and its last non-empty line is exactly MATCH."""
    lines = [line.strip() for line in stdout.splitlines() if line.strip()]
    return returncode == 0 and bool(lines) and lines[-1] == "MATCH"


def compare_tables(out_a, out_b, script=COMPARE_SCRIPT):
    """Run scripts/compare.py on two output folders with this interpreter; return (matched, output)."""
    env = dict(os.environ, PYTHONIOENCODING="utf-8")
    try:
        done = subprocess.run([sys.executable, str(script), str(out_a), str(out_b)], env=env,
                              stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                              creationflags=NO_WINDOW)
    except OSError as error:
        raise BenchError(f"cannot run {script}: {error}") from error
    stdout = done.stdout.decode("utf-8", errors="replace")
    stderr = done.stderr.decode("utf-8", errors="replace")
    return compare_matched(done.returncode, stdout), (stdout + stderr).strip()


def require_compare(script=COMPARE_SCRIPT):
    """Raise BenchError before any build when the table check cannot run under this interpreter."""
    if not Path(script).is_file():
        raise BenchError(f"table check script not found: {script}")
    if importlib.util.find_spec("polars") is None:
        raise BenchError(f"{Path(script).name} needs polars, which {sys.executable} cannot import")


def require_tool(name):
    """Full path of an executable on PATH; BenchError naming it when it is missing."""
    path = shutil.which(name)
    if path is None:
        raise BenchError(f"{name} not found on PATH")
    return path


def same_path(left, right):
    """True when two paths name the same file system entry after resolving links and case."""
    return os.path.normcase(os.path.realpath(left)) == os.path.normcase(os.path.realpath(right))


def cargo_build(command, source, target, log_path):
    """Run a release build in source with CARGO_TARGET_DIR=target; return (binary path, seconds).

    The combined build output goes to log_path. A nonzero exit or a missing
    binary raises BenchError with the tail of that output.
    """
    env = dict(os.environ, CARGO_TARGET_DIR=str(target))
    start = time.perf_counter()
    try:
        done = subprocess.run(list(command), cwd=str(source), env=env, stdin=subprocess.DEVNULL,
                              stdout=subprocess.PIPE, stderr=subprocess.STDOUT, creationflags=NO_WINDOW)
    except OSError as error:
        raise BenchError(f"cannot start the build command {command[0]}: {error}") from error
    seconds = time.perf_counter() - start
    Path(log_path).write_bytes(done.stdout)
    text = done.stdout.decode("utf-8", errors="replace")
    if done.returncode != 0:
        raise BenchError(f"build failed in {source} (exit {done.returncode}, log {log_path}):\n{tail(text)}")
    binary = Path(target) / "release" / BINARY
    if not binary.is_file():
        raise BenchError(f"build in {source} exited 0 but {binary} is missing (log {log_path}):\n{tail(text)}")
    return binary, seconds


class Builder:
    """Resolve revisions, keep detached worktrees under scratch and build release binaries.

    Calling an instance with (role, rev) returns the built Side. self.created
    lists the worktrees this instance added; no worktree is ever removed.
    """

    def __init__(self, repo, scratch, log=log_line, cargo_command=None):
        """Bind one repository and scratch folder; BenchError when git, cargo or the repository is missing."""
        self.git_path = require_tool("git")
        self.repo = Path(self.git("rev-parse", "--show-toplevel", cwd=repo))
        self.scratch = Path(scratch)
        self.log = log
        self.cargo_command = list(cargo_command) if cargo_command else [require_tool("cargo"), *CARGO_ARGS]
        self.created = []
        self.built = {}

    def git(self, *args, cwd=None):
        """Run git and return its stripped stdout; BenchError with git's message on failure.

        The output is decoded as UTF-8, the encoding git writes paths in.
        GIT_OPTIONAL_LOCKS=0 keeps status queries from rewriting an index that
        other git processes may be using.
        """
        where = self.repo if cwd is None else cwd
        env = dict(os.environ, GIT_OPTIONAL_LOCKS="0")
        try:
            done = subprocess.run([self.git_path, *args], cwd=str(where), env=env, capture_output=True, text=True,
                                  encoding="utf-8", errors="replace", stdin=subprocess.DEVNULL,
                                  creationflags=NO_WINDOW)
        except OSError as error:
            raise BenchError(f"cannot run git in {where}: {error}") from error
        if done.returncode != 0:
            message = (done.stderr or done.stdout).strip() or f"exit {done.returncode}"
            raise BenchError(f"git {' '.join(args)} failed in {where}: {message}")
        return done.stdout.strip()

    def resolve(self, rev):
        """Full commit SHA of a revision; BenchError naming the revision when it is not a commit."""
        if not rev or rev.startswith("-"):
            raise BenchError(f"invalid revision {rev!r}")
        try:
            sha = self.git("rev-parse", "--verify", "--quiet", f"{rev}^{{commit}}")
        except BenchError as error:
            raise BenchError(f"not a commit in {self.repo}: {rev!r}") from error
        if not FULL_SHA.fullmatch(sha):
            raise BenchError(f"git resolved {rev!r} to {sha!r}, which is not a full commit SHA")
        return sha

    def worktree(self, sha):
        """Detached worktree for sha under scratch: reuse a clean one at that SHA or add a new one."""
        path = self.scratch / f"wt-{sha[:12]}"
        if path.exists():
            try:
                top = self.git("rev-parse", "--show-toplevel", cwd=path)
            except BenchError as error:
                raise BenchError(f"{path} exists but is not a git worktree; move it or use another --scratch") from error
            if not same_path(top, path):
                raise BenchError(f"{path} exists but is not a git worktree; move it or use another --scratch")
            head = self.git("rev-parse", "HEAD", cwd=path)
            if head != sha:
                raise BenchError(f"{path} is a worktree at {head}, expected {sha}; refusing to reuse it")
            changes = self.git("status", "--porcelain", "--untracked-files=all", cwd=path)
            if changes:
                raise BenchError(f"{path} has local changes; refusing to reuse it:\n{tail(changes)}")
            self.log(f"reusing worktree {path}")
            return path
        self.log(f"adding worktree {path} at {sha[:12]}")
        self.git("worktree", "add", "--detach", str(path), sha)
        self.created.append(str(path))
        return path

    def tree_state(self):
        """(HEAD SHA, whether git status lists local changes) of the repository's working tree."""
        return self.git("rev-parse", "HEAD"), bool(self.git("status", "--porcelain"))

    def __call__(self, role, rev):
        """Resolve, check out and build one side; role is baseline (A) or candidate (B).

        A WORKTREE build reads tree_state before and after cargo runs and raises
        BenchError when it moved in between. A side whose target folder was
        already built in this run reuses that binary and reports the SHA and
        local-changes flag the binary was built from.
        """
        labels = {"baseline": "A", "candidate": "B"}
        if role not in labels:
            raise ValueError(f"unknown role {role!r}")
        label = labels[role]
        if rev == WORKTREE:
            state = self.tree_state()
            source, target = self.repo, self.scratch / "target-worktree"
        else:
            state = (self.resolve(rev), False)
            source, target = self.worktree(state[0]), self.scratch / f"target-{state[0][:12]}"
        key = os.path.normcase(str(target))
        if key in self.built:
            (binary, state), seconds = self.built[key], 0.0
        else:
            self.log(f"building {label} ({role} {rev}) in {source}")
            binary, seconds = cargo_build(self.cargo_command, source, target,
                                          self.scratch / f"build-{target.name}.log")
            if rev == WORKTREE:
                after = self.tree_state()
                if after != state:
                    raise BenchError(f"the working tree of {self.repo} changed during the build of {label}: "
                                     f"{tree_label(state)} before, {tree_label(after)} after; "
                                     "rerun once nothing commits or edits files there")
            self.built[key] = (binary, state)
            self.log(f"built {label} in {seconds:.1f} s: {binary}")
        sha, dirty = state
        return Side(label, role, rev, sha, dirty, str(source), str(target), str(binary), round(seconds, 3))


def tree_label(state):
    """'SHA12 clean' or 'SHA12 with local changes' for a (sha, dirty) pair."""
    sha, dirty = state
    return f"{sha[:12]} {'with local changes' if dirty else 'clean'}"


@contextmanager
def scratch_lock(scratch):
    """Hold an exclusive lock on SCRATCH/.ab_bench.lock.

    BenchError when another run holds it, or naming the system error when the
    lock call fails for any other reason.
    """
    path = Path(scratch) / ".ab_bench.lock"
    handle = open(path, "a+b")
    try:
        try:
            if os.name == "nt":
                handle.seek(0)
                msvcrt.locking(handle.fileno(), msvcrt.LK_NBLCK, 1)
            else:
                fcntl.flock(handle.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError as error:
            if error.errno in LOCK_BUSY:
                raise BenchError(f"another ab_bench run holds {path}; wait for it or use another --scratch") from error
            raise BenchError(f"cannot lock {path}: {error}") from error
        try:
            yield path
        finally:
            if os.name == "nt":
                handle.seek(0)
                msvcrt.locking(handle.fileno(), msvcrt.LK_UNLCK, 1)
            else:
                fcntl.flock(handle.fileno(), fcntl.LOCK_UN)
    finally:
        handle.close()


def remove_tree(path, attempts=5, pause=0.2):
    """Delete a folder this run created, retrying while another process still holds a file in it.

    Returns None once the folder is gone, or the last error after the final attempt.
    """
    last = None
    for attempt in range(attempts):
        try:
            shutil.rmtree(path)
            return None
        except FileNotFoundError:
            return None
        except OSError as error:
            last = error
            time.sleep(pause * (attempt + 1))
    return f"{type(last).__name__}: {last}"


def bench(config, prepare, run, compare, log=log_line):
    """Build both sides, run the warmup and paired rounds, check the tables and return the report.

    prepare(role, rev) returns a built Side; run(command, data_dir, out_dir,
    threads) returns a RunResult; compare(out_a, out_b) returns (matched,
    output). A nonzero exit or a table check without MATCH raises BenchError and
    keeps the output folders of that run; the message says the tables differ
    only when the check printed DIFFER.
    """
    sides = {"A": prepare("baseline", config.baseline), "B": prepare("candidate", config.candidate)}
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    outputs = Path(config.scratch) / "runs" / f"{stamp}-{os.getpid()}"
    outputs.mkdir(parents=True)
    records = []

    def run_one(label, kind, number):
        """Run one side in a fresh output folder; return (folder, seconds)."""
        side = sides[label]
        out_dir = outputs / f"{kind[0]}{number}-{label}"
        out_dir.mkdir()
        result = run([side.binary], config.data, out_dir, config.threads)
        if result.returncode != 0:
            raise BenchError(f"{label} ({side.role} {side.rev}) exited with status {result.returncode} in "
                             f"{kind} round {number}; output kept in {out_dir}; stderr tail:\n{tail(result.stderr)}")
        lines = [line.rstrip() for line in result.stderr.splitlines() if line.strip()]
        records.append(dict(side=label, kind=kind, round=number, seconds=result.seconds,
                            phases=parse_phases(lines), stderr_lines=lines[-STDERR_KEEP:]))
        return out_dir, result.seconds

    for number in range(1, config.warmup + 1):
        warm = {label: run_one(label, "warmup", number)[1] for label in "AB"}
        log(f"warmup {number}/{config.warmup}: A {warm['A']:.3f} s, B {warm['B']:.3f} s")
    orders = round_orders(config.runs)
    times = {"A": [], "B": []}
    checked = []
    for number, order in enumerate(orders, 1):
        folders = {}
        for label in order:
            folders[label], seconds = run_one(label, "measured", number)
            times[label].append(seconds)
        log(f"round {number}/{config.runs} {order}: A {times['A'][-1]:.3f} s, B {times['B'][-1]:.3f} s")
        if number in (1, config.runs):
            matched, output = compare(folders["A"], folders["B"])
            if not matched:
                differ = "DIFFER" in {line.strip() for line in output.splitlines()}
                what = "A and B tables differ" if differ else "the table check gave no MATCH or DIFFER verdict"
                raise BenchError(f"{what} after round {number}; outputs kept in {folders['A']} "
                                 f"and {folders['B']}:\n{tail(output)}")
            checked.append(number)
    stats = summarize(times["A"], times["B"], config.min_gain)
    leftover = remove_tree(outputs)
    if leftover:
        log(f"could not delete {outputs}: {leftover}")
    rounds = [dict(round=number, order=order, a_seconds=a, b_seconds=b, delta_seconds=round(b - a, 6))
              for number, (order, a, b) in enumerate(zip(orders, times["A"], times["B"]), 1)]
    return dict(
        tool="ab_bench", finished_at=datetime.now(timezone.utc).isoformat(timespec="seconds"),
        data_dir=str(config.data), threads=config.threads, runs=config.runs, warmup=config.warmup,
        min_gain=config.min_gain,
        machine=dict(system=platform.system(), release=platform.release(), arch=platform.machine(),
                     python=platform.python_version(), cpus=os.cpu_count()),
        rustflags=os.environ.get("RUSTFLAGS"),
        baseline=asdict(sides["A"]), candidate=asdict(sides["B"]),
        rounds=rounds, tables_match_after_rounds=checked, **stats,
        outputs_left=str(outputs) if leftover else None, run_details=records)


def describe(side):
    """'REV = SHA12', marked when a WORKTREE build had local changes."""
    changes = " with local changes" if side["dirty"] else ""
    return f"{side['rev']} = {side['sha'][:12]}{changes}"


def series(values):
    """Times with three decimals, space separated."""
    return " ".join(f"{value:.3f}" for value in values)


def format_summary(report):
    """Five-line human summary of a completed bench report."""
    a, b = report["baseline"], report["candidate"]
    checked = report["tables_match_after_rounds"]
    return "\n".join([
        f"A baseline  {describe(a)}: median {report['a_median']:.3f} s, min {report['a_min']:.3f} s; "
        f"runs {series(report['a_times'])}",
        f"B candidate {describe(b)}: median {report['b_median']:.3f} s, min {report['b_min']:.3f} s; "
        f"runs {series(report['b_times'])}",
        f"B minus A: median {report['delta_median']:+.3f} s ({100 * report['delta_median_fraction']:+.1f}% of A), "
        f"mean {report['delta_mean']:+.3f} s; B won {report['b_wins']} of {report['runs']} rounds; "
        f"sign test p = {report['sign_test_p']:.4f}",
        f"tables: MATCH after round{'s' if len(checked) > 1 else ''} {' and '.join(map(str, checked))}; "
        f"{report['runs']} rounds after {report['warmup']} warmup, NCPU={report['threads']}",
        f"verdict: {report['verdict']} (faster needs B to win at least 80% of rounds with a median delta "
        f"below -{report['threshold_seconds']:.3f} s)",
    ])


def write_json(path, report):
    """Write the report as UTF-8 JSON through a temporary file and an atomic rename.

    A failed rename removes the temporary file and raises.
    """
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_text(json.dumps(report, indent=2, allow_nan=False) + "\n", encoding="utf-8", newline="\n")
    try:
        os.replace(temporary, path)
    except OSError:
        temporary.unlink(missing_ok=True)
        raise


def parse_args(argv=None):
    """Parse and check the command line; invalid values exit with status 2."""
    parser = argparse.ArgumentParser(
        prog="ab_bench.py", description="Paired A/B run-time benchmark for two builds of the solver.",
        epilog="REV is a git revision; WORKTREE means the working tree as it is now.")
    parser.add_argument("--baseline", required=True, metavar="REV", help="build A")
    parser.add_argument("--candidate", required=True, metavar="REV", help="build B")
    parser.add_argument("--data", required=True, type=Path, metavar="DIR", help="dataset folder, passed as DATA_DIR")
    parser.add_argument("--scratch", required=True, type=Path, metavar="DIR",
                        help="folder for worktrees, builds, logs and outputs")
    parser.add_argument("--runs", type=int, default=7, metavar="N", help="paired rounds (default 7)")
    parser.add_argument("--warmup", type=int, default=1, metavar="N", help="discarded warmup rounds (default 1)")
    parser.add_argument("--threads", type=int, default=8, metavar="N", help="NCPU for the solver (default 8)")
    parser.add_argument("--min-gain", type=float, default=0.02, metavar="FRACTION",
                        help="smallest median gain that counts, as a fraction of the median time of A (default 0.02)")
    parser.add_argument("--json", type=Path, metavar="PATH", help="also write the full report as JSON")
    parser.add_argument("--repo", type=Path, default=DEFAULT_REPO, metavar="PATH",
                        help="git repository (default: the one holding this script)")
    args = parser.parse_args(argv)
    if args.runs < 1:
        parser.error(f"--runs must be at least 1, got {args.runs}")
    if args.warmup < 0:
        parser.error(f"--warmup must be 0 or more, got {args.warmup}")
    if args.threads < 1:
        parser.error(f"--threads must be at least 1, got {args.threads}")
    if not math.isfinite(args.min_gain) or not 0 <= args.min_gain < 1:
        parser.error(f"--min-gain must be a fraction from 0 up to 1, got {args.min_gain}")
    for option, rev in (("--baseline", args.baseline), ("--candidate", args.candidate)):
        if not rev.strip() or rev.startswith("-"):
            parser.error(f"{option} is not a revision: {rev!r}")
    if not args.data.is_dir():
        parser.error(f"--data is not a folder: {args.data}")
    if args.json is not None and (not args.json.name.strip() or args.json.is_dir()):
        parser.error(f"--json must name a file, not a folder: {str(args.json)!r}")
    args.data = args.data.resolve()
    args.scratch = args.scratch.resolve()
    args.repo = args.repo.resolve()
    if args.json is not None:
        args.json = args.json.resolve()
    return args


def main(argv=None, prepare=None, run=run_solver, compare=None, log=log_line):
    """Run the bench from the command line and return the exit status.

    prepare defaults to a Builder (git worktrees and cargo), run to
    run_solver and compare to scripts/compare.py, which needs polars.
    """
    args = parse_args(argv)
    try:
        if compare is None:
            require_compare()
            compare = compare_tables
        args.scratch.mkdir(parents=True, exist_ok=True)
        with scratch_lock(args.scratch):
            builder = prepare if prepare is not None else Builder(args.repo, args.scratch, log)
            report = bench(args, builder, run, compare, log)
        report["created_worktrees"] = list(getattr(builder, "created", []))
        print(format_summary(report), flush=True)
        if args.json is not None:
            write_json(args.json, report)
            print(f"report: {args.json}", flush=True)
    except (BenchError, OSError) as error:
        print(f"ab_bench FAILED: {error}", file=sys.stderr, flush=True)
        return 1
    return 0


if __name__ == "__main__":
    for stream in (sys.stdout, sys.stderr):
        stream.reconfigure(errors="replace")
    sys.exit(main())
