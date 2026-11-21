#!/usr/bin/env python3
import os
import subprocess
import sys


def main():
    args = sys.argv[1:]
    events, clicks = 20_000, 5_000
    if "--events" in args:
        at = args.index("--events")
        events = int(args[at + 1])
        del args[at:at + 2]
    if "--clicks" in args:
        at = args.index("--clicks")
        clicks = int(args[at + 1])
        del args[at:at + 2]
    generate, solver, work = args[0], args[1], args[2]
    seeds = [int(s) for s in args[3:]]
    here = os.path.dirname(os.path.abspath(__file__))
    failed = []
    for seed in seeds:
        data = os.path.join(work, f"seed-{seed}")
        out_solver = os.path.join(work, f"seed-{seed}-solver")
        out_ref = os.path.join(work, f"seed-{seed}-ref")
        subprocess.run([generate, "--seed", str(seed), "--events", str(events), "--clicks", str(clicks), "--output", data], check=True)
        os.makedirs(out_solver, exist_ok=True)
        env = dict(os.environ, DATA_DIR=data, OUTPUT_DIR=out_solver)
        run = subprocess.run([solver], env=env, capture_output=True, text=True, errors="replace")
        if run.returncode != 0:
            failed.append(seed)
            print(f"FAIL seed {seed}: solver exit {run.returncode}\n{run.stderr[-2000:]}")
            continue
        subprocess.run([sys.executable, os.path.join(here, "reference.py"), data, out_ref], check=True)
        compare = subprocess.run([sys.executable, os.path.join(here, "compare.py"), out_solver, out_ref], capture_output=True, text=True, errors="replace")
        verdict = compare.stdout.strip().splitlines()[-1] if compare.stdout.strip() else "no output"
        if compare.returncode != 0:
            failed.append(seed)
            print(f"FAIL seed {seed}\n{compare.stdout[-3000:]}")
        else:
            print(f"ok   seed {seed}: {verdict}")
    print(f"\n{len(seeds) - len(failed)}/{len(seeds)} seeds matched")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
