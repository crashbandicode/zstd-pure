#!/usr/bin/env bash
# Generate a self-contained flat coverage badge SVG from a line-coverage
# percentage — no third-party badge service. Used by the Coverage workflow
# (which publishes the result to the `badges` branch) and runnable locally to
# seed/preview the badge.
#
# Usage: coverage_badge.sh <percent> <output.svg>
set -euo pipefail

pct="${1:?usage: coverage_badge.sh <percent> <out.svg>}"
out="${2:?usage: coverage_badge.sh <percent> <out.svg>}"

# Normalise to one decimal place (accepts e.g. 92, 92.98, 92.984321).
pct=$(printf '%.1f' "$pct")
msg="${pct}%"

# Colour by threshold, matching the usual shields palette.
int=${pct%.*}
if   [ "$int" -ge 95 ]; then color="#4c1"     # brightgreen
elif [ "$int" -ge 90 ]; then color="#97ca00"  # green
elif [ "$int" -ge 80 ]; then color="#a4a61d"  # yellowgreen
elif [ "$int" -ge 70 ]; then color="#dfb317"  # yellow
elif [ "$int" -ge 60 ]; then color="#fe7d37"  # orange
else                         color="#e05d44"  # red
fi

label="coverage"
label_w=61
# ~8px per glyph plus padding sizes the coloured box to the message.
msg_w=$(( ${#msg} * 8 + 12 ))
total_w=$(( label_w + msg_w ))
# Text coords are in 1/10 units (the <text> is drawn at scale(.1)).
label_cx=$(( label_w * 10 / 2 ))
msg_cx=$(( (label_w + msg_w / 2) * 10 ))
label_tl=$(( (label_w - 10) * 10 ))
msg_tl=$(( (msg_w - 12) * 10 ))

cat > "$out" <<SVG
<svg xmlns="http://www.w3.org/2000/svg" width="${total_w}" height="20" role="img" aria-label="${label}: ${msg}">
  <title>${label}: ${msg}</title>
  <linearGradient id="s" x2="0" y2="100%"><stop offset="0" stop-color="#bbb" stop-opacity=".1"/><stop offset="1" stop-opacity=".1"/></linearGradient>
  <clipPath id="r"><rect width="${total_w}" height="20" rx="3" fill="#fff"/></clipPath>
  <g clip-path="url(#r)">
    <rect width="${label_w}" height="20" fill="#555"/>
    <rect x="${label_w}" width="${msg_w}" height="20" fill="${color}"/>
    <rect width="${total_w}" height="20" fill="url(#s)"/>
  </g>
  <g fill="#fff" text-anchor="middle" font-family="Verdana,Geneva,DejaVu Sans,sans-serif" text-rendering="geometricPrecision" font-size="110">
    <text x="${label_cx}" y="150" transform="scale(.1)" fill="#010101" fill-opacity=".3" textLength="${label_tl}">${label}</text>
    <text x="${label_cx}" y="140" transform="scale(.1)" textLength="${label_tl}">${label}</text>
    <text x="${msg_cx}" y="150" transform="scale(.1)" fill="#010101" fill-opacity=".3" textLength="${msg_tl}">${msg}</text>
    <text x="${msg_cx}" y="140" transform="scale(.1)" textLength="${msg_tl}">${msg}</text>
  </g>
</svg>
SVG

echo "wrote ${out}: ${msg} (${color})"
