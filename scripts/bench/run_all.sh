#!/usr/bin/env bash
# SPDX-FileCopyrightText: © 2026 Olof Johansson
# SPDX-License-Identifier: MIT

# Drive every benchmark in this directory, combine the per-bench
# CSVs into one timestamped run.csv, and (optionally) compare the
# new run against a baseline — exit nonzero on a >10% regression.
#
# Usage:
#   bash scripts/bench/run_all.sh                      # just collect numbers
#   bash scripts/bench/run_all.sh --baseline path.csv  # compare + fail on regression
#
# Each sub-bench boots its own L2CPU 0 (default). Runs are
# sequential — net.py needs the daemon up with a working slirp
# before disk.py grew the rootfs into a non-default state, etc.

set -uo pipefail

# Resolve any --baseline arg's path against the original cwd before
# we cd, so operators can pass either an absolute path or a path
# relative to where they ran us from.
orig_pwd="$(pwd)"
cd "$(dirname "$0")"

ts="$(date +%Y%m%d-%H%M%S)"
out_dir="results"
mkdir -p "$out_dir"
run_csv="$out_dir/run-$ts.csv"

baseline=""
while [ $# -gt 0 ]; do
    case "$1" in
        --baseline)
            # Resolve against the original cwd if not absolute.
            if [[ "$2" = /* ]]; then
                baseline="$2"
            else
                baseline="$orig_pwd/$2"
            fi
            shift 2
            ;;
        -h|--help)
            sed -n '2,15p' "$0"
            exit 0
            ;;
        *)
            echo "unknown arg: $1" >&2
            exit 2
            ;;
    esac
done

note() { echo "[run_all] $*"; }

# Combined CSV header.
echo "benchmark,metric,value,unit" > "$run_csv"

run_bench() {
    local name="$1"
    local script="$2"
    shift 2
    local per_csv="$out_dir/${name}-${ts}.csv"
    note "running ${name} -> ${per_csv}"
    if ! python3 "$script" --csv "$per_csv" "$@"; then
        note "${name} FAILED"
        return 1
    fi
    # Skip the header when concatenating.
    tail -n +2 "$per_csv" >> "$run_csv"
}

# Order: disk first (most stable target), console second (relies on
# `connect` which is well-tested), net last (needs iperf3 + slirp).
# Default iperf3 port 5201 is held by the system iperf3.service on
# the dev VM; route net.py to free ports.
overall_rc=0
run_bench "disk" "./disk.py" || overall_rc=$?
run_bench "console" "./console.py" || overall_rc=$?
run_bench "net" "./net.py" --host-port 16201 --ingress-port 16202 || overall_rc=$?

note "combined results -> $run_csv ($(wc -l < "$run_csv") lines)"

if [ -n "$baseline" ]; then
    if [ ! -f "$baseline" ]; then
        echo "FAIL: baseline $baseline not found" >&2
        exit 3
    fi
    note "comparing against baseline: $baseline"
    # Diff via a small inline python so we use lib.py's compare logic.
    python3 - "$run_csv" "$baseline" <<'PY'
import sys
from pathlib import Path
# We cd'd into scripts/bench/ at the top of run_all.sh, so lib.py
# is in the cwd.
sys.path.insert(0, ".")
from lib import compare_to_baseline, load_csv  # noqa: E402

cur_path = Path(sys.argv[1])
base_path = Path(sys.argv[2])
cur = list(load_csv(cur_path).values())
base = load_csv(base_path)
regressions = compare_to_baseline(cur, base)
if regressions:
    print(f"REGRESSION: {len(regressions)} metric(s) worse by >10%:")
    for r in regressions:
        print(f"  {r}")
    sys.exit(1)
print("OK: no metric regressed by >10%")
PY
    rc=$?
    if [ $rc -ne 0 ]; then
        overall_rc=$rc
    fi
fi

if [ $overall_rc -eq 0 ]; then
    echo "PASS: $(wc -l < "$run_csv") metric rows in $run_csv"
else
    echo "FAIL: see $run_csv (exit=$overall_rc)" >&2
fi
exit $overall_rc
