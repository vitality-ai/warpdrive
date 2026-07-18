"""
Regenerates the four Neon + WarpDrive characterization charts from
docs/benchmarks/neon_tpcc_results.html as standalone, poster-quality
matplotlib figures (PNG @ 600dpi + vector PDF).

Data is copied verbatim from the SCALING / SLAB arrays embedded in that
HTML page (sysbench-tpcc, scale=10, 120s/run, Phase 9 config, 2026-07-17).

Usage:
    ../../.venv/bin/python3 generate_neon_characterization_plots.py
Output:
    docs/benchmarks/poster_figures/*.png
    docs/benchmarks/poster_figures/*.pdf
"""

import math
import matplotlib.pyplot as plt
import matplotlib.ticker as mticker
import numpy as np
from pathlib import Path

OUTDIR = Path(__file__).parent
OUTDIR.mkdir(exist_ok=True)

# ── data (verbatim from neon_tpcc_results.html) ─────────────────────────────
T_LABELS = ["T=1", "T=4", "T=8", "T=16"]

SCALING = [
    dict(T=1,  tps=35.38, tpsPer=35.4, latAvg=113,   latP95=282,    puts=301,  gets=68,   getD=58,   getI=10,  dele=3),
    dict(T=4,  tps=35.58, tpsPer=8.9,  latAvg=457,   latP95=1304,   puts=1123, gets=109,  getD=97,   getI=12,  dele=3),
    dict(T=8,  tps=38.89, tpsPer=4.9,  latAvg=828,   latP95=5125,   puts=773,  gets=954,  getD=764,  getI=190, dele=6),
    dict(T=16, tps=4.10,  tpsPer=0.3,  latAvg=63692, latP95=100000, puts=849,  gets=2981, getD=2583, getI=398, dele=12),
]
SLAB = [
    dict(T=1,  tps=51.8, tpsPer=51.8, latAvg=77,  latP95=215,  puts=369, gets=6,   getD=1,   getI=5,  dele=0),
    dict(T=4,  tps=55.7, tpsPer=13.9, latAvg=288, latP95=1014, puts=620, gets=220, getD=188, getI=32, dele=6),
    dict(T=8,  tps=40.3, tpsPer=5.0,  latAvg=794, latP95=3706, puts=674, gets=287, getD=228, getI=59, dele=5),
    dict(T=16, tps=41.8, tpsPer=2.6,  latAvg=603, latP95=5919, puts=789, gets=231, getD=156, getI=75, dele=5),
]

# ── palette (validated categorical order, see dataviz skill references/palette.md) ──
BLUE   = "#2a78d6"   # baseline
GREEN  = "#008300"   # slab (co-located)
INK    = "#0b0b0b"
INK_2  = "#52514e"
MUTED  = "#898781"
GRID   = "#e1e0d9"
SURFACE = "#fcfcfb"

plt.rcParams.update({
    "font.family": "sans-serif",
    "font.sans-serif": ["DejaVu Sans", "Arial", "Helvetica"],
    "text.color": INK,
    "axes.edgecolor": GRID,
    "axes.labelcolor": INK_2,
    "xtick.color": INK_2,
    "ytick.color": INK_2,
    "figure.facecolor": SURFACE,
    "axes.facecolor": SURFACE,
    "savefig.facecolor": SURFACE,
})

FIGSIZE = (9, 6.2)
DPI_PNG = 600

def _pow10_fmt(v, _):
    if v == 0:
        return "0"
    if v < 0:
        return ""
    exp = round(math.log10(v))
    if math.isclose(v, 10 ** exp, rel_tol=1e-6):
        return f"$10^{{{exp}}}$"
    return f"{v:g}"

class _EquidistantLogMinorLocator(mticker.Locator):
    """Minor ticks split each decade into N equal steps in log space
    (10^(e+1/N), 10^(e+2/N), ...) instead of matplotlib's default 2,3,4..9
    subs, which bunch together toward the top of each decade."""
    def __init__(self, n=5):
        self.fracs = [i / n for i in range(1, n)]

    def __call__(self):
        vmin, vmax = self.axis.get_view_interval()
        vmin = max(vmin, 1e-9)
        lo = math.floor(math.log10(vmin))
        hi = math.ceil(math.log10(max(vmax, vmin * 10)))
        ticks = [10 ** (e + f) for e in range(lo, hi + 1) for f in self.fracs]
        return self.raise_if_exceeds(np.array(ticks))

def _style_axes(ax, title, ylabel, logy=False, symlog=False, linthresh=1):
    ax.set_title(title, fontsize=18, fontweight="bold", color=INK, pad=14)
    ax.set_ylabel(ylabel, fontsize=13, color=INK_2)
    ax.set_axisbelow(True)
    for spine in ("top", "right"):
        ax.spines[spine].set_visible(False)
    ax.spines["bottom"].set_color(MUTED)
    ax.spines["left"].set_color(MUTED)
    ax.spines["left"].set_visible(True)

    if symlog:
        ax.set_yscale("symlog", linthresh=linthresh, linscale=0.6)
    elif logy:
        ax.set_yscale("log")

    if logy or symlog:
        ax.yaxis.set_major_formatter(mticker.FuncFormatter(_pow10_fmt))
        ax.yaxis.set_minor_formatter(mticker.NullFormatter())
        ax.yaxis.set_minor_locator(_EquidistantLogMinorLocator(n=5))
    else:
        ax.minorticks_on()

    ax.tick_params(axis="x", which="minor", bottom=False, top=False)
    ax.tick_params(axis="both", which="major", labelsize=12, length=5, width=1, color=MUTED)
    ax.tick_params(axis="both", which="minor", length=3, width=0.8, color=MUTED)
    ax.grid(axis="y", which="major", color=GRID, linewidth=1, zorder=0)
    ax.grid(axis="y", which="minor", color=GRID, linewidth=0.6, alpha=0.6, zorder=0)

def _legend(ax, ncol=2):
    leg = ax.legend(
        loc="upper center", bbox_to_anchor=(0.5, -0.14), ncol=ncol,
        frameon=False, fontsize=11.5, handlelength=2.2, columnspacing=1.6,
    )
    for text in leg.get_texts():
        text.set_color(INK_2)

def _save(fig, name):
    fig.tight_layout()
    fig.savefig(OUTDIR / f"{name}.png", dpi=DPI_PNG, bbox_inches="tight")
    fig.savefig(OUTDIR / f"{name}.pdf", bbox_inches="tight")
    plt.close(fig)
    print(f"wrote {name}.png / {name}.pdf")


def _stack_group_labels(ax, x_data, items, min_gap_px=26, above_px=11, fontsize=9.5,
                          floor_margin_px=6):
    """Place one text label per (value, text, color) at x_data, stacking
    vertically in pixel space so close/equal values never overlap.
    Labels that render identically (same text + color) are collapsed to one.
    If the stack would dip below the axis floor, the whole block shifts up
    instead of letting the bottom label collide with the x-axis/tick labels."""
    trans = ax.transData
    inv = trans.inverted()
    x_disp, _ = trans.transform((x_data, 0))
    y0_disp = trans.transform((x_data, ax.get_ylim()[0]))[1]

    seen, deduped = set(), []
    for it in items:
        key = (it["text"], it["color"])
        if key in seen:
            continue
        seen.add(key)
        deduped.append(it)

    deduped.sort(key=lambda d: d["value"], reverse=True)
    positions = []
    prev_y_disp = None
    for it in deduped:
        _, y_disp = trans.transform((x_data, it["value"]))
        desired = y_disp + above_px
        if prev_y_disp is not None and desired > prev_y_disp - min_gap_px:
            desired = prev_y_disp - min_gap_px
        prev_y_disp = desired
        positions.append(desired)

    floor = y0_disp + floor_margin_px
    deficit = floor - min(positions)
    if deficit > 0:
        positions = [p + deficit for p in positions]

    for it, desired in zip(deduped, positions):
        _, y_lbl = inv.transform((x_disp, desired))
        ax.text(x_data, y_lbl, it["text"], color=it["color"], fontsize=fontsize,
                 fontweight="bold", ha="center", va="bottom", zorder=5)


# ── 1. TPS — total & per-tenant vs tenant count ─────────────────────────────
def chart_tps():
    x = np.arange(len(T_LABELS))
    fig, ax = plt.subplots(figsize=FIGSIZE)

    series = [
        ("Total TPS (baseline)",  [d["tps"] for d in SCALING],    BLUE,  "-",  "o"),
        ("TPS/tenant (baseline)", [d["tpsPer"] for d in SCALING], BLUE,  "--", "o"),
        ("Total TPS (slab 4MB)",  [d["tps"] for d in SLAB],       GREEN, "-",  "D"),
        ("TPS/tenant (slab)",     [d["tpsPer"] for d in SLAB],    GREEN, "--", "D"),
    ]
    for label, values, color, ls, marker in series:
        ax.plot(x, values, color=color, linestyle=ls, linewidth=2.4,
                 marker=marker, markersize=7, markerfacecolor=color,
                 markeredgecolor=SURFACE, markeredgewidth=1.2, label=label, zorder=3)

    ax.set_xticks(x)
    ax.set_xticklabels(T_LABELS, fontsize=13)
    ax.set_ylim(0, 62)
    _style_axes(ax, "TPS — Total & Per-Tenant vs Tenant Count", "Transactions / sec")
    ax.yaxis.set_major_locator(mticker.MultipleLocator(10))
    ax.yaxis.set_minor_locator(mticker.MultipleLocator(2))

    def fmt_num(v):
        return f"{v:.0f}" if abs(v - round(v)) < 0.05 else f"{v:.1f}"

    for xi in x:
        items = [dict(value=values[xi], text=fmt_num(values[xi]), color=color)
                  for _, values, color, _, _ in series]
        _stack_group_labels(ax, xi, items, min_gap_px=36, above_px=12)

    _legend(ax, ncol=2)
    _save(fig, "01_tps_scaling")


# ── 2. GET amplification — delta vs image, baseline vs slab (log) ──────────
def chart_gets():
    x = np.arange(len(T_LABELS))
    width = 0.34
    fig, ax = plt.subplots(figsize=FIGSIZE)

    base_d = np.array([d["getD"] for d in SCALING], dtype=float)
    base_i = np.array([d["getI"] for d in SCALING], dtype=float)
    slab_d = np.array([d["getD"] for d in SLAB], dtype=float)
    slab_i = np.array([d["getI"] for d in SLAB], dtype=float)

    xb, xs = x - width / 2 - 0.02, x + width / 2 + 0.02

    ax.bar(xb, base_d, width, color=BLUE, alpha=0.95, label="delta GETs (baseline)", zorder=3)
    ax.bar(xb, base_i, width, bottom=base_d, color=BLUE, alpha=0.4, hatch="///",
           edgecolor=SURFACE, linewidth=0.6, label="image GETs (baseline)", zorder=3)
    ax.bar(xs, slab_d, width, color=GREEN, alpha=0.95, label="delta GETs (slab ⬡)", zorder=3)
    ax.bar(xs, slab_i, width, bottom=slab_d, color=GREEN, alpha=0.4, hatch="///",
           edgecolor=SURFACE, linewidth=0.6, label="image GETs (slab ⬡)", zorder=3)

    for xi, d, i in zip(xb, base_d, base_i):
        ax.annotate(f"{int(d + i)}", (xi, d + i), textcoords="offset points",
                    xytext=(0, 4), ha="center", fontsize=9.5, fontweight="bold", color=BLUE)
    for xi, d, i in zip(xs, slab_d, slab_i):
        ax.annotate(f"{int(d + i)}", (xi, d + i), textcoords="offset points",
                    xytext=(0, 4), ha="center", fontsize=9.5, fontweight="bold", color=GREEN)

    ax.set_xticks(x)
    ax.set_xticklabels(T_LABELS, fontsize=13)
    ax.set_ylim(1, 6000)
    _style_axes(ax, "GET Amplification — Delta vs Image vs Tenant Count", "GET requests (log scale)", logy=True)
    _legend(ax, ncol=2)
    _save(fig, "02_get_amplification")


# ── 3. PUTs & DELETE_OBJECTS vs tenant count — stacked bars (log) ──────────
# Exact mirror of chart 2's pattern: PUTs (bottom) + DELETE_OBJECTS (hatched
# cap on top), baseline vs slab, per T, single total label above each stack.
def chart_puts():
    x = np.arange(len(T_LABELS))
    width = 0.34
    fig, ax = plt.subplots(figsize=FIGSIZE)

    base_p = np.array([d["puts"] for d in SCALING], dtype=float)
    base_del = np.array([d["dele"] for d in SCALING], dtype=float)
    slab_p = np.array([d["puts"] for d in SLAB], dtype=float)
    slab_del = np.array([d["dele"] for d in SLAB], dtype=float)

    xb, xs = x - width / 2 - 0.02, x + width / 2 + 0.02

    ax.bar(xb, base_p, width, color=BLUE, alpha=0.95, label="PUTs (baseline)", zorder=3)
    ax.bar(xb, base_del, width, bottom=base_p, color=BLUE, alpha=0.4, hatch="///",
           edgecolor=SURFACE, linewidth=0.6, label="DELETE_OBJECTS (baseline)", zorder=3)
    ax.bar(xs, slab_p, width, color=GREEN, alpha=0.95, label="PUTs (slab ⬡)", zorder=3)
    ax.bar(xs, slab_del, width, bottom=slab_p, color=GREEN, alpha=0.4, hatch="///",
           edgecolor=SURFACE, linewidth=0.6, label="DELETE_OBJECTS (slab ⬡)", zorder=3)

    for xi, p, d in zip(xb, base_p, base_del):
        ax.annotate(f"{int(p)}", (xi, p), textcoords="offset points",
                    xytext=(0, 4), ha="center", fontsize=9.5, fontweight="bold", color=BLUE)
        ax.annotate(f"+{int(d)}", (xi, p + d), textcoords="offset points",
                    xytext=(0, 19), ha="center", fontsize=8, color=BLUE)
    for xi, p, d in zip(xs, slab_p, slab_del):
        ax.annotate(f"{int(p)}", (xi, p), textcoords="offset points",
                    xytext=(0, 4), ha="center", fontsize=9.5, fontweight="bold", color=GREEN)
        ax.annotate(f"+{int(d)}", (xi, p + d), textcoords="offset points",
                    xytext=(0, 19), ha="center", fontsize=8, color=GREEN)

    ax.set_xticks(x)
    ax.set_xticklabels(T_LABELS, fontsize=13)
    ax.set_ylim(1, 6000)
    _style_axes(ax, "PUTs & DELETE_OBJECTS vs Tenant Count", "Requests (log scale)", logy=True)
    _legend(ax, ncol=2)
    _save(fig, "03_puts_delete_objects")


# ── 4. Latency (avg & p95) vs tenant count (log) ────────────────────────────
def chart_latency():
    x = np.arange(len(T_LABELS))
    fig, ax = plt.subplots(figsize=FIGSIZE)

    series = [
        ("avg lat (baseline)", [d["latAvg"] for d in SCALING], BLUE,  "-",  "o"),
        ("p95 lat (baseline)", [d["latP95"] for d in SCALING], BLUE,  "--", "o"),
        ("avg lat (slab 4MB)", [d["latAvg"] for d in SLAB],    GREEN, "-",  "D"),
        ("p95 lat (slab)",     [d["latP95"] for d in SLAB],    GREEN, "--", "D"),
    ]
    def fmt_lat(v):
        return f"{v/1000:.1f}s" if v >= 1000 else f"{v:g}ms"

    for label, values, color, ls, marker in series:
        ax.plot(x, values, color=color, linestyle=ls, linewidth=2.4,
                 marker=marker, markersize=7, markerfacecolor=color,
                 markeredgecolor=SURFACE, markeredgewidth=1.2, label=label, zorder=3)

    ax.set_xticks(x)
    ax.set_xticklabels(T_LABELS, fontsize=13)
    ax.set_ylim(50, 260000)
    _style_axes(ax, "Latency (avg & p95) vs Tenant Count", "Latency, ms (log scale)", logy=True)

    for xi in x:
        items = [dict(value=values[xi], text=fmt_lat(values[xi]), color=color)
                  for _, values, color, _, _ in series]
        _stack_group_labels(ax, xi, items, min_gap_px=30, above_px=9)

    _legend(ax, ncol=2)
    _save(fig, "04_latency_scaling")


if __name__ == "__main__":
    chart_tps()
    chart_gets()
    chart_puts()
    chart_latency()
    print(f"\nAll figures written to {OUTDIR}/")
