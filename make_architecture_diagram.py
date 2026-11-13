"""
Architecture diagram for the Methodology section: illustrates what that
section already describes in prose (three placement classes behind one
interface, selected per bucket, with an automatic budget fallback, and
WarpDrive's RS(6,3) striping across its 9-node cluster) -- adds no new
claims, just a visual of the existing text. The cluster box shows two
representative nodes (N0 data, N8 parity) with an ellipsis standing in
for the other seven, rather than drawing all 9, to keep the figure
compact. DuckDB/Lance logos are the real, official project logos
(downloaded from their own GitHub repos: duckdb/duckdb's logo/
directory, and lancedb/lance's README), not redrawn or approximated.

Run: python3 make_architecture_diagram.py
Writes PNG into ./plots/
"""
import os

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib.patches import FancyBboxPatch, FancyArrowPatch

OUT_DIR = "plots"
ASSETS_DIR = "assets"
os.makedirs(OUT_DIR, exist_ok=True)

# Same Okabe-Ito colorblind-safe palette as make_hipc_plots.py, for a
# visually consistent poster.
BLUE = "#0072B2"      # static / data shard
ORANGE = "#E69F00"    # parametric / parity shard
PURPLE = "#CC79A7"    # imperative
GREEN = "#009E73"     # shared interface
GREY = "#555555"
DARK = "#333333"
LIGHT = "#f4f4f4"

fig, ax = plt.subplots(figsize=(3.4, 3.87))
ax.set_xlim(0, 10)
ax.set_ylim(2.3, 16.5)
ax.axis("off")


def box(cx, cy, w, h, text, color, textcolor="white", fontsize=7.2, weight="bold", lw=1.3):
    b = FancyBboxPatch(
        (cx - w / 2, cy - h / 2), w, h,
        boxstyle="round,pad=0.08,rounding_size=0.12",
        linewidth=lw, edgecolor=color, facecolor=color if textcolor == "white" else LIGHT,
        zorder=3,
    )
    ax.add_patch(b)
    ax.text(cx, cy, text, ha="center", va="center", fontsize=fontsize,
             color=textcolor, weight=weight, zorder=4, linespacing=1.3)
    return b


def logo_box(cx, cy, w, h, img_path, pad=0.12):
    """A light bordered box framing a real logo image (not redrawn)."""
    b = FancyBboxPatch(
        (cx - w / 2, cy - h / 2), w, h,
        boxstyle="round,pad=0.05,rounding_size=0.1",
        linewidth=1.3, edgecolor="#999999", facecolor="white", zorder=3,
    )
    ax.add_patch(b)
    img = plt.imread(img_path)
    img_h, img_w = img.shape[0], img.shape[1]
    avail_w, avail_h = w - 2 * pad, h - 2 * pad
    scale = min(avail_w / img_w, avail_h / (img_h * (avail_w / img_w) / avail_w)) if False else None
    # Fit image preserving aspect ratio inside (avail_w, avail_h).
    img_aspect = img_w / img_h
    box_aspect = avail_w / avail_h
    if img_aspect > box_aspect:
        disp_w = avail_w
        disp_h = avail_w / img_aspect
    else:
        disp_h = avail_h
        disp_w = avail_h * img_aspect
    ax.imshow(img, extent=(cx - disp_w / 2, cx + disp_w / 2, cy - disp_h / 2, cy + disp_h / 2),
              zorder=4, aspect="auto")
    return b


def arrow(x0, y0, x1, y1, color=GREY, style="-|>", lw=1.3, ls="solid", rad=0.0):
    a = FancyArrowPatch((x0, y0), (x1, y1), arrowstyle=style, mutation_scale=9,
                         linewidth=lw, color=color, zorder=2, linestyle=ls,
                         connectionstyle=f"arc3,rad={rad}", shrinkA=2, shrinkB=2)
    ax.add_patch(a)


# Row 0: real client applications evaluated against WarpDial (real logos)
logo_box(3.0, 15.6, 3.4, 1.1, os.path.join(ASSETS_DIR, "duckdb_logo.png"))
logo_box(7.0, 15.6, 3.4, 1.1, os.path.join(ASSETS_DIR, "lance_logo.png"))

# Row 1: client request
box(5, 13.9, 6.4, 0.9, "PUT/GET bucket/key\n(+ units)", GREY, fontsize=6.6)
arrow(3.5, 15.0, 4.6, 14.4)
arrow(6.5, 15.0, 5.4, 14.4)

# Row 2: bucket config
box(5, 12.2, 5.6, 0.9, "Bucket config:\npolicy + overhead budget", DARK, fontsize=6.8)
arrow(5, 13.45, 5, 12.65)

# Row 3: three policy classes
box(1.9, 10.3, 2.9, 1.15, "Static\nCRUSH-style hash\n$O$(buckets)", BLUE, fontsize=6.3)
box(5.0, 10.3, 2.9, 1.15, "Parametric\nseed / epoch\n$O$(objects)", ORANGE, fontsize=6.3, textcolor="black")
box(8.1, 10.3, 2.9, 1.15, "Imperative\ncontent-aware\n$O$(objects$\\times$units)", PURPLE, fontsize=6.3)

arrow(4.3, 11.75, 2.3, 10.9)
arrow(5, 11.75, 5, 10.9)
arrow(5.7, 11.75, 7.7, 10.9)

# Row 4: shared interface
box(5, 8.3, 7.2, 1.0, "PlacementPolicy / StripePacker\n(one interface, selected per bucket)", GREEN, fontsize=6.8)
arrow(2.3, 9.7, 4.0, 8.8)
arrow(5, 9.7, 5, 8.8)
arrow(7.7, 9.7, 6.0, 8.8)

# Fallback arrow: Imperative -> Static (dashed)
arrow(8.1, 9.7, 2.2, 9.85, color=PURPLE, ls="dashed", lw=1.1, rad=-0.35, style="-|>")
ax.text(5.0, 11.25, "falls back if over budget", ha="center", va="center",
        fontsize=5.3, color=PURPLE, style="italic", zorder=4)

arrow(5, 7.8, 5, 6.85)

# Compact WarpDrive cluster box: two representative nodes (N0 data,
# N8 parity) with an ellipsis standing in for the other 7, one stripe
# of one object striped across all 9 under RS(6,3).
cluster = FancyBboxPatch(
    (0.6, 4.5), 8.8, 2.3,
    boxstyle="round,pad=0.08,rounding_size=0.15",
    linewidth=1.6, edgecolor=DARK, facecolor="#eef0f2", zorder=1,
)
ax.add_patch(cluster)
ax.text(5.0, 6.45, "WarpDrive cluster — RS(6,3)", ha="center", va="center",
        fontsize=7.4, weight="bold", color=DARK, zorder=3)

node_w, node_h = 2.3, 1.05
box(2.3, 5.3, node_w, node_h, "N0\ndata", BLUE, fontsize=6.3)
ax.text(5.0, 5.3, "...", ha="center", va="center", fontsize=12,
        color=DARK, weight="bold", zorder=4)
box(7.7, 5.3, node_w, node_h, "N8\nparity", ORANGE, fontsize=6.3, textcolor="black")

arrow(5, 4.5, 5, 3.9)

# Row 6: outputs
box(5, 3.3, 7.8, 1.05, "PUT response: policy used + cost\nover budget $\\rightarrow$ falls back to Static",
    "white", textcolor="#333333", fontsize=6.4, weight="normal", lw=1.1)

plt.tight_layout(pad=0.15)
out_path = os.path.join(OUT_DIR, "architecture.png")
fig.savefig(out_path, dpi=300, bbox_inches="tight")
print(f"wrote {out_path}")
