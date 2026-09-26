#!/usr/bin/env bash
# End-to-end CLI benchmark for `semantic-docs` (+ `semantic-docs-replay`).
# usage: bench.sh <deepwiki-rs binary> [items=20000] [runs=5]
# Generates a deterministic synthetic Rustdoc JSON, runs the real CLI `runs` times,
# checks every run is byte-identical, replays the receipt, and prints median/min/max ms.
set -euo pipefail
BIN=$1; N=${2:-20000}; RUNS=${3:-5}
WORK=$(mktemp -d); trap 'rm -rf "$WORK"' EXIT
python3 - "$N" "$WORK/in.json" <<'PY'
import json, sys
n, dst = int(sys.argv[1]), sys.argv[2]
index, paths = {}, {}
for i in range(n):
    nxt = (i + 1) % n
    index[str(i)] = {"name": f"item_{i}", "visibility": "public",
        "docs": f"Item {i} links [`item_{nxt}`] and \"quotes\".\nSecond line.",
        "span": {"filename": f"src/m{i % 50}.rs", "begin": [i + 1, 1], "end": [i + 9, 2]},
        "inner": {"function": {}}, "links": {f"`item_{nxt}`": nxt, "`Self`": i}}
    paths[str(i)] = {"path": ["bench", f"item_{i}"], "kind": "function"}
json.dump({"root": 0, "format_version": 61, "index": index, "paths": paths}, open(dst, "w"), sort_keys=True)
PY
REV=0123456789abcdef0123456789abcdef01234567
times=()
for r in $(seq 1 "$RUNS"); do
  s=$(python3 -c 'import time;print(time.perf_counter_ns())')
  "$BIN" semantic-docs --rustdoc-json "$WORK/in.json" --repository bench/bench --revision $REV -o "$WORK/g.trig" >/dev/null
  e=$(python3 -c 'import time;print(time.perf_counter_ns())')
  times+=($(( (e - s) / 1000000 )))
  d=$(shasum -a 256 "$WORK/g.trig" | cut -d' ' -f1)
  if [ -n "${prev:-}" ] && [ "$d" != "$prev" ]; then echo "NONDETERMINISTIC run $r" >&2; exit 1; fi
  prev=$d
done
"$BIN" semantic-docs-replay --receipt "$WORK/g.trig.receipt.json" >/dev/null
sorted=$(printf '%s\n' "${times[@]}" | sort -n)
echo "items=$N runs=$RUNS input_bytes=$(wc -c <"$WORK/in.json" | tr -d ' ') output_bytes=$(wc -c <"$WORK/g.trig" | tr -d ' ') output_sha256=$prev" \
  "median_ms=$(echo "$sorted" | sed -n "$(( (RUNS + 1) / 2 ))p") min_ms=$(echo "$sorted" | head -1) max_ms=$(echo "$sorted" | tail -1) replay=REPLAYED"
