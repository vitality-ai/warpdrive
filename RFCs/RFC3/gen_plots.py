"""
Generate all plots for RFC3-segment-storage.md.
Outputs individual PNGs into the same directory.

Usage:
    python3 gen_plots.py
"""

import numpy as np
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import matplotlib.patches as mpatches
from matplotlib.ticker import FuncFormatter
from scipy.optimize import minimize_scalar, brentq

# ── shared style ────────────────────────────────────────────────────────────
plt.rcParams.update({
    "font.size": 11,
    "axes.titlesize": 12,
    "axes.labelsize": 11,
    "legend.fontsize": 10,
    "figure.dpi": 150,
})
BLUE   = "#2e6fbd"
ORANGE = "#e07b39"
GREEN  = "#2e8b57"
GRAY   = "#888888"


# ── helpers ──────────────────────────────────────────────────────────────────

def obj_C(T, alpha, beta):
    if T <= 0:
        return float("inf")
    return alpha * (1 + T) + beta * (1 + T) / T

def solve_C(alpha, beta):
    r = minimize_scalar(obj_C, bounds=(1e-4, 0.5), method="bounded", args=(alpha, beta))
    T = r.x
    return {"T": T, "f": T/(1+T), "WA": 1+T, "SA": (1+T)/T, "V": r.fun}

def obj_B(T, alpha, beta, rho):
    if T <= 0:
        return float("inf")
    return alpha * (1 + T) * rho + beta / T

def solve_B(alpha, beta, rho):
    r = minimize_scalar(obj_B, bounds=(1e-4, 0.5), method="bounded", args=(alpha, beta, rho))
    T = r.x
    return {"T": T, "WA_eff": (1+T)*rho, "WA": 1+T, "SA": 1/T, "V": r.fun}

def crossover_rho(alpha, beta):
    vC = solve_C(alpha, beta)["V"]
    def gap(rho): return solve_B(alpha, beta, rho)["V"] - vC
    if gap(1.0) >= 0:
        return 1.0
    try:
        return brentq(gap, 1e-4, 100.0)
    except ValueError:
        return float("nan")


# ════════════════════════════════════════════════════════════════════════════
# Plot 1 — Virtual address space → segment files
# ════════════════════════════════════════════════════════════════════════════

def plot_address_space():
    SEG = 512  # MB units for display

    fig, ax = plt.subplots(figsize=(10, 3.2))
    ax.set_xlim(0, 3 * SEG)
    ax.set_ylim(-0.55, 1.3)
    ax.axis("off")

    colors = [BLUE, ORANGE, GREEN]
    labels = ["0000000000.seg\nsealed", "0000000001.seg\nsealed", "0000000002.seg\nactive"]

    for i, (col, lbl) in enumerate(zip(colors, labels)):
        left = i * SEG
        ax.barh(0, SEG, left=left, height=0.55, color=col, alpha=0.22, edgecolor=col, linewidth=1.8)
        # boundary offset labels
        start_gb = i * SEG / 1024
        ax.text(left, -0.32, f"{start_gb:.3f} GB", ha="center", va="top", fontsize=9, color=GRAY)
        # file label above bar
        ax.text(left + SEG / 2, 0.62, lbl, ha="center", va="bottom",
                fontsize=9, color=col, fontweight="bold")

    # rightmost boundary
    ax.text(3 * SEG, -0.32, f"{3 * SEG / 1024:.3f} GB", ha="center", va="top", fontsize=9, color=GRAY)

    # decode example: object at local offset 200 MB inside segment 1
    example_x = 1 * SEG + 200  # 712 MB
    ax.annotate(
        f"  global_offset = {example_x} MB\n"
        f"  seg_id        = {example_x} ÷ 512  =  1  →  0000000001.seg\n"
        f"  local_offset  = {example_x % SEG} MB  →  seek here",
        xy=(example_x, 0.28),
        xytext=(1.82 * SEG, 1.1),
        fontsize=9,
        arrowprops=dict(arrowstyle="->", color="black", lw=1.2),
        bbox=dict(boxstyle="round,pad=0.35", fc="lightyellow", ec="gray"),
        va="bottom",
    )
    # dot at the example point
    ax.scatter([example_x], [0], color="black", s=30, zorder=5)

    ax.set_title("Global Offset → Segment File Decoding  (SEGMENT_SIZE = 512 MB)", pad=10)
    plt.tight_layout()
    plt.savefig("plot_address_space.png", bbox_inches="tight")
    plt.close()
    print("Saved plot_address_space.png")


# ════════════════════════════════════════════════════════════════════════════
# Plot 2 — Utilization timeline: write → delete → compact
# ════════════════════════════════════════════════════════════════════════════

def plot_utilization_timeline():
    fig, ax = plt.subplots(figsize=(9, 4.5))

    # Simulate three phases over 30 time steps
    t_write_end   = 10
    t_delete_end  = 20
    t_compact_end = 30
    total = t_compact_end

    ts = np.arange(total + 1, dtype=float)

    # Phase 1: write 10 GB → disk and live both rise linearly
    # Phase 2: delete 70% → live drops immediately, disk stays
    # Phase 3: compaction cycle → disk drops to match live
    peak_gb = 10.0

    disk = np.empty(total + 1)
    live = np.empty(total + 1)

    for i, t in enumerate(ts):
        if t <= t_write_end:
            frac = t / t_write_end
            live[i] = peak_gb * frac
            disk[i] = peak_gb * frac
        elif t <= t_delete_end:
            # delete 70% instantly at t=10, hold disk constant until compaction
            frac_del = (t - t_write_end) / (t_delete_end - t_write_end)
            live[i] = peak_gb * (1.0 - 0.70 * frac_del)
            disk[i] = peak_gb
        else:
            # compaction runs: disk converges toward live
            frac_cmp = (t - t_delete_end) / (t_compact_end - t_delete_end)
            live[i] = peak_gb * 0.30
            target = peak_gb * 0.30 * 1.10   # slight overhead from headroom
            disk[i] = peak_gb + (target - peak_gb) * frac_cmp

    ax.fill_between(ts, disk, live, alpha=0.15, color=ORANGE, label="Dead bytes (space waste)")
    ax.fill_between(ts, live, alpha=0.25, color=BLUE,   label="Live bytes")
    ax.plot(ts, disk, color=ORANGE, lw=2,   label="Disk usage")
    ax.plot(ts, live, color=BLUE,   lw=2,   label="Live data size")

    # Phase bands
    for xstart, xend, label, col in [
        (0, t_write_end,  "Phase 1\nWrite 10 GB",   "#e0ffe0"),
        (t_write_end, t_delete_end,  "Phase 2\nDelete 70%",    "#fff0e0"),
        (t_delete_end, t_compact_end, "Phase 3\nCompaction",   "#e0f0ff"),
    ]:
        ax.axvspan(xstart, xend, alpha=0.25, color=col, zorder=0)
        ax.text((xstart + xend)/2, 10.4, label, ha="center", va="bottom",
                fontsize=9, color=GRAY, style="italic")

    ax.axvline(t_write_end,   color=GRAY, lw=1, ls="--")
    ax.axvline(t_delete_end,  color=GRAY, lw=1, ls="--")

    ax.set_xlabel("Time (arbitrary units)")
    ax.set_ylabel("Storage (GB)")
    ax.set_ylim(0, 11.5)
    ax.set_xlim(0, total)
    ax.set_title("Space Amplification Over Time: Write → Delete → Compact")
    ax.legend(loc="upper right")
    ax.grid(True, alpha=0.25)
    plt.tight_layout()
    plt.savefig("plot_utilization_timeline.png", bbox_inches="tight")
    plt.close()
    print("Saved plot_utilization_timeline.png")


# ════════════════════════════════════════════════════════════════════════════
# Plot 3 — Objective surface over T for several α/β weights
# ════════════════════════════════════════════════════════════════════════════

def plot_objective_surface():
    fig, ax = plt.subplots(figsize=(8, 5))

    Ts = np.linspace(0.01, 0.50, 400)

    pairs = [
        (0.9, 0.1, "α=0.9, β=0.1  (WA-dominant)"),
        (0.5, 0.5, "α=0.5, β=0.5  (balanced)"),
        (0.1, 0.9, "α=0.1, β=0.9  (SA-dominant)"),
    ]
    palette = [BLUE, ORANGE, GREEN]

    for (alpha, beta, lbl), col in zip(pairs, palette):
        vals = [obj_C(T, alpha, beta) for T in Ts]
        ax.plot(Ts, vals, color=col, lw=2, label=lbl)
        # mark minimum
        T_opt = min(np.sqrt(beta / alpha), 0.5) if alpha > 0 else 0.5
        v_opt = obj_C(T_opt, alpha, beta)
        ax.scatter([T_opt], [v_opt], color=col, s=70, zorder=5)
        ax.annotate(f" T*={T_opt:.2f}", (T_opt, v_opt),
                    textcoords="offset points", xytext=(6, 2), fontsize=8.5, color=col)

    ax.axvline(0.5,  color=GRAY, lw=1, ls="--", label="T cap = 0.5")
    ax.axvline(0.30, color="red", lw=1, ls=":",  label="Proposed T = 0.30")

    ax.set_xlabel("Cold threshold  T")
    ax.set_ylabel("Objective  α·WA + β·SA")
    ax.set_title("Objective Function vs T  (Approach B, f = T/(1+T))")
    ax.legend()
    ax.grid(True, alpha=0.25)
    plt.tight_layout()
    plt.savefig("plot_objective_surface.png", bbox_inches="tight")
    plt.close()
    print("Saved plot_objective_surface.png")


# ════════════════════════════════════════════════════════════════════════════
# Plot 4 — WA / SA Pareto frontier for Approach B
# ════════════════════════════════════════════════════════════════════════════

def plot_pareto_c():
    fig, ax = plt.subplots(figsize=(7, 5))

    alphas = np.linspace(0.05, 0.95, 37)
    was, sas, Ts_val = [], [], []
    for alpha in alphas:
        r = solve_C(alpha, 1 - alpha)
        was.append(r["WA"])
        sas.append(r["SA"])
        Ts_val.append(r["T"])

    sc = ax.scatter(was, sas, c=Ts_val, cmap="viridis", s=40, zorder=3)
    ax.plot(was, sas, color=GRAY, lw=1, alpha=0.5, zorder=2)
    cbar = fig.colorbar(sc, ax=ax, pad=0.02)
    cbar.set_label("Optimal T*")

    # proposed operating point
    T_prop, f_prop = 0.30, 0.29
    WA_prop = 1 + T_prop
    SA_prop = (1 + T_prop) / T_prop
    ax.scatter([WA_prop], [SA_prop], color="red", s=90, zorder=5,
               label=f"Proposed  T={T_prop}, f={f_prop}")

    # optimal balanced
    r_opt = solve_C(0.5, 0.5)
    ax.scatter([r_opt["WA"]], [r_opt["SA"]], color=GREEN, s=120, marker="*", zorder=5,
               label=f"Optimal balanced  T={r_opt['T']:.2f}")

    ax.set_xlabel("Write Amplification (WA)")
    ax.set_ylabel("Space Amplification (SA)")
    ax.set_title("WA / SA Pareto Frontier — Approach B")
    ax.legend()
    ax.grid(True, alpha=0.25)
    plt.tight_layout()
    plt.savefig("plot_pareto_c.png", bbox_inches="tight")
    plt.close()
    print("Saved plot_pareto_c.png")


# ════════════════════════════════════════════════════════════════════════════
# Plot 5 — A vs B crossover as a function of ρ
# ════════════════════════════════════════════════════════════════════════════

def plot_b_vs_c():
    fig, axes = plt.subplots(1, 2, figsize=(12, 5))

    rhos = np.linspace(1.0, 4.0, 200)

    # Left panel: balanced α=β=0.5
    ax = axes[0]
    alpha, beta = 0.5, 0.5
    vC = solve_C(alpha, beta)["V"]
    vBs = [solve_B(alpha, beta, rho)["V"] for rho in rhos]
    rc  = crossover_rho(alpha, beta)

    ax.plot(rhos, vBs, color=ORANGE, lw=2, label="V*(A, ρ)  — Approach A")
    ax.axhline(vC, color=BLUE, lw=2, ls="--", label=f"V*(B) = {vC:.2f}  — Approach B")
    ax.axvline(rc, color=GRAY, lw=1.2, ls=":", label=f"ρ_c = {rc:.2f}")
    ax.fill_between(rhos, vBs, vC,
                    where=[v > vC for v in vBs],
                    alpha=0.15, color=BLUE, label="B preferred")
    ax.fill_between(rhos, vBs, vC,
                    where=[v <= vC for v in vBs],
                    alpha=0.15, color=ORANGE, label="A preferred")
    ax.set_xlabel("Write interference  ρ")
    ax.set_ylabel("Optimal objective  V*")
    ax.set_title("A vs B — Balanced weights  (α = β = 0.5)")
    ax.legend(fontsize=9)
    ax.grid(True, alpha=0.25)

    # Right panel: ρ_c across different α/β
    ax = axes[1]
    ab_pairs = [(0.9, 0.1), (0.7, 0.3), (0.5, 0.5), (0.3, 0.7), (0.1, 0.9)]
    rho_cs = [crossover_rho(a, b) for a, b in ab_pairs]
    labels = [f"α={a:.1f}, β={b:.1f}" for a, b in ab_pairs]
    colors_bar = plt.cm.viridis(np.linspace(0.15, 0.85, len(ab_pairs)))

    bars = ax.barh(labels, rho_cs, color=colors_bar, edgecolor="white", height=0.55)
    ax.axvline(1.0, color=GRAY, lw=1, ls="--")
    for bar, rc_val in zip(bars, rho_cs):
        ax.text(rc_val + 0.05, bar.get_y() + bar.get_height()/2,
                f"{rc_val:.2f}", va="center", fontsize=9)
    ax.set_xlabel("Crossover  ρ_c  (C wins above this ρ)")
    ax.set_title("Crossover ρ_c by Priority Weight")
    ax.set_xlim(0, max(rho_cs) * 1.18)
    ax.grid(True, alpha=0.25, axis="x")

    plt.tight_layout()
    plt.savefig("plot_b_vs_c.png", bbox_inches="tight")
    plt.close()
    print("Saved plot_b_vs_c.png")


# ════════════════════════════════════════════════════════════════════════════
# Plot 6 — WA ablation: effect of k (object lifetime L) and T
# ════════════════════════════════════════════════════════════════════════════

def wa(T, L):
    """WA for objects that live L segment-fill cycles.
    k = T*(1 - T^L)/(1-T)  →  WA = 1 + k
    Special cases: L=1 gives WA=1+T, L→∞ gives WA=1/(1-T).
    """
    if T <= 0:
        return 1.0
    if T >= 1:
        return float("inf")
    if L == float("inf"):
        return 1.0 / (1.0 - T)
    k = T * (1.0 - T**L) / (1.0 - T)
    return 1.0 + k


def plot_wa_ablation():
    fig, axes = plt.subplots(1, 2, figsize=(13, 5))

    Ts = np.linspace(0.01, 0.50, 400)

    # ── Left: WA vs T for different object lifetimes L ──────────────────────
    ax = axes[0]

    L_values = [1, 2, 3, 5, 10, float("inf")]
    labels    = ["L = 1  (WA = 1+T, short-lived)", "L = 2", "L = 3",
                 "L = 5", "L = 10", "L → ∞  (WA = 1/(1−T), long-lived)"]
    palette   = plt.cm.viridis(np.linspace(0.1, 0.9, len(L_values)))

    for L, lbl, col in zip(L_values, labels, palette):
        was = [wa(T, L) for T in Ts]
        ls  = "--" if L == float("inf") else "-"
        lw  = 2.2  if L in (1, float("inf")) else 1.4
        ax.plot(Ts, was, color=col, lw=lw, ls=ls, label=lbl)

    ax.axvline(0.30, color="red",  lw=1.2, ls=":", label="Proposed T = 0.30")
    ax.axvline(0.50, color=GRAY,   lw=1.0, ls="--", label="Optimal T = 0.50")
    ax.set_xlabel("Cold threshold  T")
    ax.set_ylabel("Write Amplification  (WA)")
    ax.set_title("WA vs T — Ablation over Object Lifetime L\n(L = segment-fill cycles an object lives)")
    ax.legend(fontsize=8.5)
    ax.grid(True, alpha=0.25)

    # ── Right: k vs L for proposed and optimal T ─────────────────────────────
    ax = axes[1]

    Ls = np.linspace(1, 20, 300)
    for T_val, col, lbl in [(0.30, "red", "T = 0.30  (proposed)"),
                             (0.50, BLUE,  "T = 0.50  (optimal)")]:
        ks = [T_val * (1 - T_val**L) / (1 - T_val) for L in Ls]
        ax.plot(Ls, ks, color=col, lw=2, label=lbl)
        # asymptote
        k_inf = T_val / (1 - T_val)
        ax.axhline(k_inf, color=col, lw=1, ls="--",
                   label=f"k → {k_inf:.2f} as L → ∞")

    ax.set_xlabel("Object lifetime  L  (segment-fill cycles)")
    ax.set_ylabel("Expected compactions per byte  k")
    ax.set_title("k vs Object Lifetime — How Many Times a Byte Gets Compacted")
    ax.legend(fontsize=9)
    ax.grid(True, alpha=0.25)

    plt.tight_layout()
    plt.savefig("plot_wa_ablation.png", bbox_inches="tight")
    plt.close()
    print("Saved plot_wa_ablation.png")


# ════════════════════════════════════════════════════════════════════════════

if __name__ == "__main__":
    plot_address_space()
    plot_utilization_timeline()
    plot_objective_surface()
    plot_pareto_c()
    plot_b_vs_c()
    plot_wa_ablation()
    print("\nAll plots generated.")
