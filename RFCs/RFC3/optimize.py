"""
RFC3 Storage Parameter Optimization
=====================================
Finds optimal COLD_THRESHOLD (T) and HEADROOM fraction (f).

Problem 1 — Approach C (compact into active-1 headroom):
    minimize    alpha * WA + beta * SA
    subject to  WA = 1 + T
                SA = (1 + T) / T          (after setting f = T/(1+T))
                0  <  T <= 0.5

    Closed-form:  T* = sqrt(beta/alpha), capped at 0.5

Problem 2 — Bi-level: B vs C parameterized by rho:
    Approach B (compact into active, no headroom):
        WA_B = (1 + T) * rho             (rho = write interference factor)
        SA_B = 1 / T                     (no headroom waste)
        min_{T} alpha*(1+T)*rho + beta/T
        T_B* = sqrt(beta / (alpha*rho))

    Approach C (compact into active-1 headroom):
        WA_C = 1 + T                     (no interference)
        SA_C = (1 + T) / T               (headroom overhead)
        min_{T} alpha*(1+T) + beta*(1+T)/T
        T_C* = sqrt(beta/alpha), capped at 0.5

    rho is measured from benchmarks. C is preferred when V*(C) < V*(B, rho).
    For balanced alpha=beta=0.5 the crossover is rho_c ≈ 1 (any interference tips to C).

Usage:
    python3 optimize.py
"""

import numpy as np
from scipy.optimize import minimize_scalar, brentq


# ---------------------------------------------------------------------------
# Approach C objective
# ---------------------------------------------------------------------------

def obj_C(T: float, alpha: float, beta: float) -> float:
    if T <= 0:
        return float("inf")
    return alpha * (1 + T) + beta * (1 + T) / T


def solve_C(alpha: float, beta: float) -> dict:
    result = minimize_scalar(obj_C, bounds=(1e-4, 0.5), method="bounded", args=(alpha, beta))
    T = result.x
    f = T / (1 + T)
    return {"T": T, "f": f, "WA": 1 + T, "SA": (1 + T) / T, "V": result.fun}


def closed_form_C(alpha: float, beta: float) -> dict:
    if alpha == 0:
        T = 0.5
    elif beta == 0:
        T = 1e-4
    else:
        T = min(np.sqrt(beta / alpha), 0.5)
    f = T / (1 + T)
    return {"T": T, "f": f, "WA": 1 + T, "SA": (1 + T) / T}


# ---------------------------------------------------------------------------
# Approach B objective (with interference factor rho)
# ---------------------------------------------------------------------------

def obj_B(T: float, alpha: float, beta: float, rho: float) -> float:
    if T <= 0:
        return float("inf")
    return alpha * (1 + T) * rho + beta / T


def solve_B(alpha: float, beta: float, rho: float) -> dict:
    result = minimize_scalar(obj_B, bounds=(1e-4, 0.5), method="bounded",
                             args=(alpha, beta, rho))
    T = result.x
    return {"T": T, "WA_eff": (1 + T) * rho, "WA": 1 + T,
            "SA": 1 / T, "V": result.fun, "rho": rho}


# ---------------------------------------------------------------------------
# Crossover: find rho_c where V*(B, rho_c) == V*(C)
# ---------------------------------------------------------------------------

def crossover_rho(alpha: float, beta: float) -> float:
    V_C = solve_C(alpha, beta)["V"]

    def gap(rho):
        return solve_B(alpha, beta, rho)["V"] - V_C

    if gap(1.0) >= 0:
        return 1.0
    try:
        return brentq(gap, 1e-4, 100.0)
    except ValueError:
        return float("nan")


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

if __name__ == "__main__":
    print("=" * 70)
    print("RFC3 Storage Parameter Optimization")
    print("=" * 70)

    # 1. Approach C: numerical vs closed-form
    print("\n--- Approach C: Numerical vs Closed-Form ---\n")
    print(f"{'alpha':>6} {'beta':>6} | {'T_num':>6} {'T_cf':>6} | "
          f"{'f':>6} | {'WA':>5} {'SA':>5}")
    print("-" * 55)
    for alpha, beta in [(0.9, 0.1), (0.7, 0.3), (0.5, 0.5), (0.3, 0.7), (0.1, 0.9)]:
        num = solve_C(alpha, beta)
        cf  = closed_form_C(alpha, beta)
        print(f"{alpha:>6.1f} {beta:>6.1f} | {num['T']:>6.3f} {cf['T']:>6.3f} | "
              f"{num['f']:>6.3f} | {num['WA']:>5.2f} {num['SA']:>5.2f}")

    # 2. B vs C comparison at rho values
    print("\n--- Approach B vs C (balanced alpha=beta=0.5) ---\n")
    alpha, beta = 0.5, 0.5
    c = solve_C(alpha, beta)
    print(f"  Approach C:  T*={c['T']:.3f}  WA={c['WA']:.2f}  SA={c['SA']:.2f}  V={c['V']:.3f}")
    print()
    print(f"  {'rho':>5} | {'T_B*':>6} {'WA_eff':>7} {'SA_B':>6} {'V_B':>6} | {'winner':>8}")
    print("  " + "-" * 50)
    for rho in [1.0, 1.1, 1.2, 1.5, 2.0, 3.0]:
        b = solve_B(alpha, beta, rho)
        winner = "C" if c["V"] < b["V"] else "B"
        print(f"  {rho:>5.1f} | {b['T']:>6.3f} {b['WA_eff']:>7.2f} {b['SA']:>6.2f} "
              f"{b['V']:>6.3f} | {winner:>8}")

    # 3. Crossover rho for different alpha/beta
    print("\n--- Crossover rho_c (C preferred when rho > rho_c) ---\n")
    print(f"  {'alpha':>6} {'beta':>6} | {'rho_c':>7}")
    print("  " + "-" * 25)
    for alpha, beta in [(0.9, 0.1), (0.7, 0.3), (0.5, 0.5), (0.3, 0.7), (0.1, 0.9)]:
        rc = crossover_rho(alpha, beta)
        print(f"  {alpha:>6.1f} {beta:>6.1f} | {rc:>7.3f}")

    # 4. Pareto frontier for Approach C
    print("\n--- Approach C Pareto Frontier ---\n")
    print(f"  {'alpha':>6} {'beta':>6} | {'T*':>6} {'f*':>6} | {'WA':>5} {'SA':>5}")
    print("  " + "-" * 50)
    alphas = np.linspace(0.05, 0.95, 19)
    frontier = []
    for alpha in alphas:
        beta = 1 - alpha
        r = solve_C(alpha, beta)
        frontier.append(r)
        print(f"  {alpha:>6.2f} {beta:>6.2f} | {r['T']:>6.3f} {r['f']:>6.3f} | "
              f"{r['WA']:>5.2f} {r['SA']:>5.2f}")

    # 5. Proposed constants
    print("\n--- Proposed Constants ---\n")
    T_prop, f_prop = 0.30, 0.29
    WA_prop = 1 + T_prop
    SA_prop = (1 + T_prop) / T_prop
    opt = closed_form_C(0.5, 0.5)
    print(f"  Proposed : T={T_prop}  f={f_prop}  WA={WA_prop:.2f}  SA={SA_prop:.2f}")
    print(f"  Optimal  : T={opt['T']:.3f}  f={opt['f']:.3f}  WA={opt['WA']:.2f}  SA={opt['SA']:.2f}")
    print(f"  Gap      : WA+{WA_prop - opt['WA']:.2f}  SA+{SA_prop - opt['SA']:.2f}")
    print(f"\n  T=0.30 is conservative vs T*=0.50.")
    print(f"  After benchmarking rho, re-run to confirm approach B or C.")

    # 6. Plots
    try:
        import matplotlib.pyplot as plt

        was_C = [r["WA"] for r in frontier]
        sas_C = [r["SA"] for r in frontier]

        fig, axes = plt.subplots(1, 2, figsize=(13, 5))

        # Left: Pareto frontier
        ax = axes[0]
        ax.plot(was_C, sas_C, "o-", color="steelblue", label="Approach C frontier")
        ax.scatter([WA_prop], [SA_prop], color="red", zorder=5, s=80,
                   label=f"Proposed (T={T_prop}, f={f_prop})")
        ax.scatter([opt["WA"]], [opt["SA"]], color="green", zorder=5, s=100,
                   marker="*", label=f"Optimal C (T={opt['T']:.2f})")
        ax.set_xlabel("Write Amplification (WA)")
        ax.set_ylabel("Space Amplification (SA)")
        ax.set_title("Approach C — WA/SA Pareto Frontier")
        ax.legend()
        ax.grid(True, alpha=0.3)

        # Right: V*(B, rho) vs V*(C) for varying rho
        ax = axes[1]
        rhos = np.linspace(1.0, 4.0, 100)
        alpha, beta = 0.5, 0.5
        vB = [solve_B(alpha, beta, rho)["V"] for rho in rhos]
        vC = solve_C(alpha, beta)["V"]
        ax.plot(rhos, vB, color="orange", label="V*(B, ρ)")
        ax.axhline(vC, color="steelblue", linestyle="--", label="V*(C)")
        rc = crossover_rho(alpha, beta)
        ax.axvline(rc, color="gray", linestyle=":", label=f"ρ_c = {rc:.2f}")
        ax.fill_between(rhos, vB, vC, where=[v > vC for v in vB],
                        alpha=0.15, color="steelblue", label="C preferred region")
        ax.set_xlabel("Interference factor ρ")
        ax.set_ylabel("Optimal objective V*")
        ax.set_title("B vs C — crossover at ρ_c (α=β=0.5)")
        ax.legend()
        ax.grid(True, alpha=0.3)

        plt.tight_layout()
        plt.savefig("pareto.png", dpi=150)
        print("\nPlots saved to pareto.png")
    except ImportError:
        print("\n(matplotlib not available — skipping plots)")
