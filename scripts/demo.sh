#!/usr/bin/env bash
# Run the full enterprise workflow against a scratch copy of the sample
# project. Requires a built binary: cargo build --release
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
B="$ROOT/target/release/stratz"
test -x "$B" || { echo "build first: cargo build --release"; exit 1; }

DEMO="${STRATZ_DEMO_DIR:-$(mktemp -d)/stratz-demo}"
rm -rf "$DEMO"
mkdir -p "$DEMO"
cp -r "$ROOT/sample-project/." "$DEMO/"
cd "$DEMO"

echo "== doctor =="
"$B" doctor

echo; echo "== run v1 (latest) =="
"$B" run | head -20

echo; echo "== run v1 pinned =="
"$B" run --strategy-version v1 --json > /dev/null
echo "recorded"

echo; echo "== registry =="
"$B" registry list --limit 5

echo; echo "== tag runs for robustness =="
"$B" registry tag 1 trials
"$B" registry tag 2 trials

echo; echo "== sweep =="
"$B" sweep --grid sweep_grid.yaml

echo; echo "== walk-forward =="
"$B" walk-forward --window 10d --oos 5d

echo; echo "== stress matrix =="
"$B" stress --scenario-dir scenarios

echo; echo "== robustness =="
"$B" robust --registry-tag trials || true

echo; echo "== query =="
"$B" query registry "SELECT id, kind, label, return_pct FROM runs ORDER BY id DESC LIMIT 5"

echo; echo "== html report =="
RUN_DIR=$(for d in .stratz/results/*/; do [ -f "$d/metrics.json" ] && echo "$d" && break; done | head -1)
"$B" report "$RUN_DIR" --html | tail -2

echo; echo "== AI dry-run (offline) =="
"$B" ai status
"$B" ai compile --dry-run | head -6

echo
echo "demo complete — project at $DEMO"
