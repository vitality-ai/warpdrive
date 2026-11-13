"""
Plots from results.json. Styled for visual continuity with RFC3's own
gen_plots.py (clean matplotlib, no seaborn), reusing the violet/magenta/
amber-on-ink palette established for WarpDrive's own README/benchmarks
this cycle rather than inventing a new one.
"""
import json
import matplotlib.pyplot as plt
import matplotlib.ticker as mticker

INK = "#0b0b0b"
INK_2 = "#52514e"
MUTED = "#898781"
GRID = "#e1e0d9"
SURFACE = "#fcfcfb"

# one color per SELECTIVITY REGIME (the workload) -- the frontier line
# connects granularity choices *within* a regime, since granularity is the
# actual knob a customer tunes; selectivity is a workload property, not a
# choice, so it must not be the thing the connecting line varies over.
SEL_COLORS = {"low_selectivity_1pct": "#4a0e63", "med_selectivity_10pct": "#c92a6b", "high_selectivity_50pct": "#ff8a3d"}
SEL_LABELS = {"low_selectivity_1pct": "1% selectivity", "med_selectivity_10pct": "10% selectivity", "high_selectivity_50pct": "50% selectivity"}

plt.rcParams.update({
    "font.family": "sans-serif",
    "text.color": INK, "axes.edgecolor": GRID, "axes.labelcolor": INK_2,
    "xtick.color": INK_2, "ytick.color": INK_2,
    "figure.facecolor": SURFACE, "axes.facecolor": SURFACE, "savefig.facecolor": SURFACE,
})


def style_axes(ax, title, xlabel, ylabel, logx=False, logy=False):
    ax.set_title(title, fontsize=15, fontweight="bold", color=INK, pad=12)
    ax.set_xlabel(xlabel, fontsize=12, color=INK_2)
    ax.set_ylabel(ylabel, fontsize=12, color=INK_2)
    for spine in ("top", "right"):
        ax.spines[spine].set_visible(False)
    ax.spines["bottom"].set_color(MUTED)
    ax.spines["left"].set_color(MUTED)
    if logx:
        ax.set_xscale("log")
        ax.xaxis.set_minor_locator(mticker.LogLocator(base=10, subs=range(2, 10)))
        ax.xaxis.set_minor_formatter(mticker.NullFormatter())
    if logy:
        ax.set_yscale("log")
        ax.yaxis.set_minor_locator(mticker.LogLocator(base=10, subs=range(2, 10)))
        ax.yaxis.set_minor_formatter(mticker.NullFormatter())
    ax.minorticks_on()
    ax.tick_params(which="major", length=6, width=1, color=MUTED, labelsize=10)
    ax.tick_params(which="minor", length=3, width=0.7, color=MUTED)
    ax.grid(True, which="major", color=GRID, linewidth=1, zorder=0)
    ax.grid(True, which="minor", color=GRID, linewidth=0.5, alpha=0.6, zorder=0)
    ax.set_axisbelow(True)


def pareto_plot(results, format_name, granularities_order, ax):
    """x = round trips (log), y = byte amplification % (log) -- the two
    axes that pull in opposite directions as granularity coarsens. The
    connecting line is the actual Pareto frontier: within one selectivity
    regime (one workload, one color), it traces the tradeoff as the
    GRANULARITY KNOB sweeps through many batch sizes (finest -> coarsest),
    every point a real, distinct measurement, not 3 hand-picked labels.
    Selectivity regimes are never connected to each other -- selectivity
    isn't a knob, it's a property of the workload, so a line across
    regimes would not be a frontier."""
    for regime, color in SEL_COLORS.items():
        xs, ys = [], []
        for gran in granularities_order:
            r = results[format_name][regime][gran]
            rt = r["round_trips"]
            ba = r["byte_amp"]
            ax.errorbar(rt["med"], ba["med"],
                        xerr=[[rt["med"] - rt["lo"]], [rt["hi"] - rt["med"]]],
                        yerr=[[ba["med"] - ba["lo"]], [ba["hi"] - ba["med"]]],
                        fmt="o", color=color, markersize=6, capsize=2,
                        markeredgecolor=SURFACE, markeredgewidth=0.6, zorder=3)
            xs.append(rt["med"]); ys.append(ba["med"])
        ax.plot(xs, ys, "-", color=color, linewidth=1.6, alpha=0.65, zorder=2)
        # label only the two endpoints (finest / coarsest) to avoid clutter
        ax.annotate("finest", (xs[0], ys[0]), textcoords="offset points", xytext=(6, 6),
                    fontsize=7.5, color=color, fontweight="bold")
        ax.annotate("coarsest", (xs[-1], ys[-1]), textcoords="offset points", xytext=(6, -10),
                    fontsize=7.5, color=color, fontweight="bold")

    display_name = {"parquet": "Parquet", "vector": "Vector index"}[format_name]
    style_axes(ax, f"{display_name}: round trips vs byte amplification",
               "Round trips per query (log)", "Byte read amplification, % (log)", logx=True, logy=True)


def legend_figure(fig):
    from matplotlib.lines import Line2D
    sel_handles = [Line2D([0], [0], marker="o", color=SEL_COLORS[k], linestyle="-", markersize=8,
                           label=SEL_LABELS[k]) for k in SEL_COLORS]
    fig.legend(handles=sel_handles, loc="lower center", ncol=3,
               frameon=False, fontsize=9.5, bbox_to_anchor=(0.5, -0.02))


def main():
    with open("results.json") as f:
        results = json.load(f)

    parquet_grans = list(results["parquet"]["low_selectivity_1pct"].keys())
    vector_grans = list(results["vector"]["low_selectivity_1pct"].keys())

    fig, axes = plt.subplots(1, 2, figsize=(14, 6))
    pareto_plot(results, "parquet", parquet_grans, axes[0])
    pareto_plot(results, "vector", vector_grans, axes[1])
    fig.suptitle("Per-object granularity knob: round trips vs. bytes, both formats", fontsize=16, fontweight="bold", y=1.03)
    legend_figure(fig)
    fig.tight_layout()
    fig.savefig("pareto_both_formats.png", dpi=200, bbox_inches="tight")
    print("wrote pareto_both_formats.png")

    # Storage overhead vs batch size -- now a real curve (dense sweep),
    # not a 3-bar sketch. Selectivity doesn't affect storage overhead (it's
    # a packing property, not a query property), so this is regime-
    # independent; med_selectivity_10pct's overhead numbers are identical
    # across regimes and used here as the canonical values.
    fig2, ax2 = plt.subplots(figsize=(8, 6))
    color_p, color_v = SEL_COLORS["low_selectivity_1pct"], SEL_COLORS["high_selectivity_50pct"]
    for grans, fmt_name, color, marker in [(parquet_grans, "parquet", color_p, "o"), (vector_grans, "vector", color_v, "s")]:
        xs = [int(g.split(" (")[0].replace("batch_", "")) for g in grans]
        ys = [results[fmt_name]["med_selectivity_10pct"][g]["overhead"]["med"] for g in grans]
        ax2.plot(xs, ys, "-", marker=marker, color=color, markersize=7,
                  markeredgecolor=SURFACE, markeredgewidth=0.6, linewidth=1.8, zorder=3,
                  label={"parquet": "Parquet", "vector": "Vector index"}[fmt_name])
    style_axes(ax2, "Storage overhead vs. batch size (granularity knob), both formats",
               "Batch size, computable units per storage unit (log)", "Storage overhead vs optimal, % (log)",
               logx=True, logy=True)
    ax2.legend(frameon=False, fontsize=10)
    fig2.tight_layout()
    fig2.savefig("overhead_by_granularity.png", dpi=200, bbox_inches="tight")
    print("wrote overhead_by_granularity.png")


if __name__ == "__main__":
    main()
