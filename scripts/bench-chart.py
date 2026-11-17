#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Su Kang Yin <paradyse@gmail.com>
# SPDX-License-Identifier: Apache-2.0
"""bench-chart.py OUT.svg MATCHED.json: the README's benchmark chart.

One panel per metric, one bar per tool, each the median of the samples a
matched run (scripts/benchmark_matched.py) recorded. Each panel says whether
more or less is better; herdr-revive's bar is coloured. The README's chart:

    scripts/bench-chart.py assets/bench.svg docs/benchmarks/linux-matched-2026-09-18.json
"""

import html
import json
import statistics
import sys

# (sample key suffix, field, title, unit, scale)
METRICS = [
    ("100_{}_save", "wall_ms", "Save, 100 panes", "ms", 1),
    ("100_{}_preview", "wall_ms", "Preview, 100 panes", "ms", 1),
    ("1_{}_event_debounced", "wall_ms", "Debounced event", "ms", 1),
    ("100_{}_save", "rss_kib", "Memory per save, 100 panes", "MiB", 1 / 1024),
]
# herdr-revive first; its default transport is the direct one.
TOOLS = [("herdr-revive", "rust_direct", True), ("herdr-resurrect", "javascript_cli", False)]

PANEL_W, BAR_H, GAP, LABEL_W, VALUE_W = 420, 28, 8, 140, 80
BG, BORDER, TEXT, HINT = "#1e1f29", "#44475a", "#e6e6e6", "#8a8fa8"
OURS, OTHERS = "#50fa7b", "#6272a4"
FONT = "font-family=\"-apple-system, 'Segoe UI', Helvetica, Arial, sans-serif\""


def fmt(v):
    return f"{v:,.0f}" if v >= 100 else f"{v:,.1f}" if v >= 10 else f"{v:,.2f}"


def main():
    out_path, report_path = sys.argv[1], sys.argv[2]
    samples = json.load(open(report_path))["samples"]
    panel_h = 48 + len(TOOLS) * (BAR_H + GAP)
    cols = 2
    rows = (len(METRICS) + cols - 1) // cols
    width = cols * PANEL_W + (cols + 1) * 20
    height = 20 + rows * (panel_h + 20)
    svg = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" '
        f'viewBox="0 0 {width} {height}" {FONT} font-size="16">',
        f'<rect width="100%" height="100%" rx="10" fill="{BG}" stroke="{BORDER}"/>',
    ]
    for i, (key, field, title, unit, scale) in enumerate(METRICS):
        x0 = 20 + (i % cols) * (PANEL_W + 20)
        y0 = 20 + (i // cols) * (panel_h + 20)
        values = [
            statistics.median(s[field] for s in samples[key.format(variant)]) * scale
            for _, variant, _ in TOOLS
        ]
        top = max(values)
        svg.append(
            f'<text x="{x0}" y="{y0 + 17}" font-weight="600" font-size="17" fill="{TEXT}">'
            f"{html.escape(title)}</text>"
            f'<text x="{x0 + PANEL_W}" y="{y0 + 17}" text-anchor="end" fill="{HINT}" '
            f'font-size="14">{unit}, lower is better</text>'
        )
        bar_w = PANEL_W - LABEL_W - VALUE_W
        for j, ((name, _, ours), v) in enumerate(zip(TOOLS, values)):
            y = y0 + 34 + j * (BAR_H + GAP)
            w = max(2, bar_w * v / top)
            weight = ' font-weight="600"' if ours else ""
            svg.append(
                f'<text x="{x0}" y="{y + 20}" fill="{TEXT}"{weight}>{html.escape(name)}</text>'
                f'<rect x="{x0 + LABEL_W}" y="{y}" width="{w:.1f}" height="{BAR_H}" rx="3" '
                f'fill="{OURS if ours else OTHERS}"/>'
                f'<text x="{x0 + LABEL_W + w + 6:.1f}" y="{y + 20}" fill="{TEXT}">{fmt(v)}</text>'
            )
    svg.append("</svg>")
    open(out_path, "w").write("\n".join(svg) + "\n")


if __name__ == "__main__":
    main()
