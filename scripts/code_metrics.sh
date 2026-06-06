#!/usr/bin/env bash
# Code metrics for the crate via Mozilla's rust-code-analysis: per-file and
# per-function cyclomatic & cognitive complexity, SLOC, and the maintainability
# index. Informational only — it never fails the build on a threshold; it just
# prints a Markdown summary (stdout) so CI can drop it into the run summary and a
# developer can spot complexity hot-spots.
#
# The crates.io `rust-code-analysis-cli` (0.0.25, the latest release) does not
# compile on current stable Rust, so this uses the prebuilt release binary,
# cached under `target/tools/` (or `$RCA_CACHE`). Set `$RCA_CLI` to point at an
# existing binary to skip the download.
#
# Usage: scripts/code_metrics.sh [path]   (default: the crate's src/)
set -euo pipefail

VERSION="v0.0.25"
ASSET="rust-code-analysis-linux-cli-x86_64.tar.gz"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CACHE="${RCA_CACHE:-$ROOT/target/tools}"
CLI="${RCA_CLI:-}"

# Resolve the CLI: explicit $RCA_CLI, else one already on PATH, else download the
# prebuilt linux binary into the cache.
if [ -z "$CLI" ]; then
  if command -v rust-code-analysis-cli >/dev/null 2>&1; then
    CLI="$(command -v rust-code-analysis-cli)"
  else
    CLI="$CACHE/rust-code-analysis-cli"
    if [ ! -x "$CLI" ]; then
      mkdir -p "$CACHE"
      echo "code_metrics: downloading $ASSET ($VERSION) ..." >&2
      curl -fsSL "https://github.com/mozilla/rust-code-analysis/releases/download/$VERSION/$ASSET" \
        -o "$CACHE/$ASSET"
      tar xzf "$CACHE/$ASSET" -C "$CACHE"
    fi
  fi
fi

SRC="${1:-$ROOT/src}"

# Run rust-code-analysis per file (one JSON object each) and aggregate in Python:
# a per-file table sorted by cognitive complexity, totals, and the most complex
# functions. `mi_visual_studio` is the Visual-Studio-scaled maintainability index
# (higher is better; rust-code-analysis flags < ~20 as worth a look).
python3 - "$CLI" "$SRC" "$ROOT" <<'PY'
import json, os, subprocess, sys

cli, src, root = sys.argv[1], sys.argv[2], sys.argv[3]
files = sorted(
    os.path.join(d, n)
    for d, _, names in os.walk(src)
    for n in names
    if n.endswith(".rs")
)

def funcs(space, acc, rel):
    for s in space.get("spaces", []):
        m = s.get("metrics", {})
        acc.append((
            m.get("cognitive", {}).get("sum", 0) or 0,
            m.get("cyclomatic", {}).get("max", 0) or 0,
            s.get("name", "?"), rel, s.get("start_line", 0),
        ))
        funcs(s, acc, rel)

rows, allfuncs = [], []
tot_sloc = tot_cyc = tot_cog = 0.0
for f in files:
    r = subprocess.run([cli, "-m", "-p", f, "-O", "json"],
                       capture_output=True, text=True)
    if r.returncode != 0 or not r.stdout.strip():
        continue
    d = json.loads(r.stdout)
    m = d.get("metrics", {})
    sloc = m.get("loc", {}).get("sloc", 0) or 0
    cyc = m.get("cyclomatic", {}).get("sum", 0) or 0
    cog = m.get("cognitive", {}).get("sum", 0) or 0
    mi = m.get("mi", {}).get("mi_visual_studio", 0) or 0
    rel = os.path.relpath(f, root)
    rows.append((rel, sloc, cyc, cog, mi))
    tot_sloc += sloc; tot_cyc += cyc; tot_cog += cog
    funcs(d, allfuncs, rel)

print("## Code metrics — rust-code-analysis\n")
print(f"**{len(rows)} files** · {int(tot_sloc)} SLOC · cyclomatic {int(tot_cyc)} "
      f"· cognitive {int(tot_cog)}\n")
print("Per file (sorted by cognitive complexity):\n")
print("| file | SLOC | cyclomatic | cognitive | MI (VS) |")
print("|---|--:|--:|--:|--:|")
for rel, sloc, cyc, cog, mi in sorted(rows, key=lambda r: -r[3]):
    print(f"| {rel} | {int(sloc)} | {int(cyc)} | {int(cog)} | {mi:.0f} |")
print("\nMost cognitively complex functions:\n")
print("| cognitive | cyclomatic | function | location |")
print("|--:|--:|---|---|")
for cog, cyc, name, rel, line in sorted(allfuncs, key=lambda r: -r[0])[:15]:
    print(f"| {int(cog)} | {int(cyc)} | `{name}` | {rel}:{line} |")
PY
