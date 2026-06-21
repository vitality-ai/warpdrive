"""
RFC3 Storage Parameter Optimization
=====================================
Finds optimal COLD_THRESHOLD (T) and HEADROOM fraction (f) by solving:

    minimize    alpha * WA + beta * SA
    subject to  WA = 1 + T
                SA = 1 / ((1 - f) * T)
                f  >= T / (1 + T)       (headroom must absorb survivors)
                0  <  T <= 0.5
                0  <= f <= 0.5

Setting f = T/(1+T) (minimum feasible headroom) reduces SA to (1+T)/T
and gives the closed-form solution T* = sqrt(beta/alpha), capped at 0.5.

This script:
  1. Solves numerically with scipy for a range of alpha/beta weights
  2. Compares against the closed-form T* = sqrt(beta/alpha)
  3. Sweeps the Pareto frontier and validates the proposed constants
  4. Saves a Pareto plot to pareto.png

Usage:
    pip install scipy matplotlib numpy
    python3 optimize.py
"""

import numpy as np
from scipy.optimize import minimize_scalar


# ---------------------------------------------------------------------------
# Objective: minimize alpha*WA + beta*SA over T in (0, 0.5]
# With f = T/(1+T), SA = (1+T)/T, WA = 1+T
# ---------------------------------------------------------------------------

def objective(T: float, alpha: float, beta: float) -> float:
    if T <= 0:
        return float("inf")
    WA = 1 + T
    SA = (1 + T) / T
    return alpha * WA + beta * SA


def solve(alpha: float, beta: float) -> dict:
    """Numerically minimize the objective over T in (0, 0.5]."""
    result = minimize_scalar(
        objective,
        bounds=(1e-4, 0.5),
        method="bounded",
        args=(alpha, beta),
    )
    T = result.x
    f = T / (1 + T)
    WA = 1 + T
    SA = (1 + T) / T
    return {"T": T, "f": f, "WA": WA, "SA": SA}


def closed_form(alpha: float, beta: float) -> dict:
    """Closed-form solution: T* = sqrt(beta/alpha), capped at 0.5."""
    if alpha == 0:
        T = 0.5
    elif beta == 0:
        T = 1e-4
    else:
        T = min(np.sqrt(beta / alpha), 0.5)
    f = T / (1 + T)
    return {"T": T, "f": f, "WA": 1 + T, "SA": (1 + T) / T}


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

if __name__ == "__main__":
    print("=" * 65)
    print("RFC3 Storage Parameter Optimization")
    print("=" * 65)

    # 1. Numerical vs closed-form spot checks
    print("\n--- Numerical vs Closed-Form ---\n")
    print(f"{'alpha':>6} {'beta':>6} | {'T_num':>6} {'T_cf':>6} | "
          f"{'f_num':>6} {'f_cf':>6} | {'WA':>5} {'SA':>5}")
    print("-" * 65)

    weights = [(0.9, 0.1), (0.7, 0.3), (0.5, 0.5), (0.3, 0.7), (0.1, 0.9)]
    for alpha, beta in weights:
        num = solve(alpha, beta)
        cf  = closed_form(alpha, beta)
        print(f"{alpha:>6.1f} {beta:>6.1f} | {num['T']:>6.3f} {cf['T']:>6.3f} | "
              f"{num['f']:>6.3f} {cf['f']:>6.3f} | {num['WA']:>5.2f} {num['SA']:>5.2f}")

    # 2. Pareto frontier sweep
    print("\n--- Pareto Frontier ---\n")
    print(f"{'alpha':>6} {'beta':>6} | {'T*':>6} {'f*':>6} | {'WA':>5} {'SA':>5}")
    print("-" * 50)

    alphas = np.linspace(0.05, 0.95, 19)
    frontier = []
    for alpha in alphas:
        beta = 1 - alpha
        r = solve(alpha, beta)
        frontier.append((alpha, beta, r["WA"], r["SA"], r["T"], r["f"]))
        print(f"{alpha:>6.2f} {beta:>6.2f} | {r['T']:>6.3f} {r['f']:>6.3f} | "
              f"{r['WA']:>5.2f} {r['SA']:>5.2f}")

    # 3. Proposed constants vs optimal
    print("\n--- Proposed Constants Check ---\n")
    T_prop, f_prop = 0.30, 0.29
    WA_prop = 1 + T_prop
    SA_prop = (1 + T_prop) / T_prop
    opt = closed_form(0.5, 0.5)
    print(f"  Proposed : T={T_prop}, f={f_prop}, WA={WA_prop:.3f}, SA={SA_prop:.3f}x")
    print(f"  Optimal  : T={opt['T']:.3f}, f={opt['f']:.3f}, "
          f"WA={opt['WA']:.3f}, SA={opt['SA']:.3f}x")
    print(f"  Gap      : WA +{WA_prop - opt['WA']:.3f}, SA +{SA_prop - opt['SA']:.3f}x")
    print(f"\n  Note: proposed T=0.30 is intentionally conservative vs T*=0.50")
    print(f"  to avoid compacting segments that are temporarily cold.")

    # 4. Plot Pareto frontier
    try:
        import matplotlib.pyplot as plt

        was = [p[2] for p in frontier]
        sas = [p[3] for p in frontier]

        plt.figure(figsize=(7, 5))
        plt.plot(was, sas, "o-", color="steelblue", label="Pareto frontier")
        plt.scatter([WA_prop], [SA_prop], color="red", zorder=5, s=80,
                    label=f"Proposed (T={T_prop}, f={f_prop})")
        plt.scatter([opt['WA']], [opt['SA']], color="green", zorder=5, s=80,
                    marker="*",
                    label=f"Optimal balanced (T={opt['T']:.2f})")
        for alpha, _, wa, sa, T, f in frontier[::3]:
            plt.annotate(f"α={alpha:.2f}", (wa, sa),
                         textcoords="offset points", xytext=(5, 4), fontsize=7)
        plt.xlabel("Write Amplification (WA)")
        plt.ylabel("Space Amplification (SA)")
        plt.title("WA vs SA Pareto Frontier — RFC3 Segment Storage")
        plt.legend()
        plt.grid(True, alpha=0.3)
        plt.tight_layout()
        plt.savefig("pareto.png", dpi=150)
        print("\nPareto plot saved to pareto.png")
    except ImportError:
        print("\n(matplotlib not available — skipping plot)")
