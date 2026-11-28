#!/usr/bin/env python3
"""derive3.py <run dir> [<duration s>]: zs3.sh runs. Per run and phase: ops/s, p50, network bytes sent by all nodes
per op (netmon.sh) and mean CPU % per node (soak_sampler.sh), into derived.csv. Then the mean, min and max over
runs of each (stage, build, data, arm, size, clients, phase) into means.csv. Tags are
<stage>-<build>-<data>-<arm>-<size>-c<clients>-r<rep>, and builds are wd-<name>."""
import csv, glob, os, sys
from collections import defaultdict
d = sys.argv[1]; dur = int(sys.argv[2]) if len(sys.argv) > 2 else 60; raw = os.path.join(d, "raw"); rows = []
for times in sorted(glob.glob(f"{raw}/*-times.txt")):
    tag = os.path.basename(times)[:-len("-times.txt")]
    p = tag.split("-")
    stage, build, data, arm, size, clients, rep = p[0], p[1] + "-" + p[2], p[3], p[4], int(p[5]), int(p[6][1:]), int(p[7][1:])
    t0, t1 = map(int, open(times).read().split())
    res = {l.split(",")[0]: l.strip().split(",") for l in open(f"{raw}/{tag}.csv") if l.startswith(("put", "get"))}
    for ph, (a, b, ca, cb) in {"put": (t0, t0 + dur, t0 + 5, t0 + dur), "get": (t0 + dur, t1, t0 + dur + 5, t1)}.items():
        if ph not in res:
            continue
        tx, cpu = 0, []
        for i in range(5):
            net = [list(map(int, l.split()[:3])) for l in open(f"{raw}/{tag}-net{i}.txt") if len(l.split()) >= 3]
            w = [r for r in net if a <= r[0] <= b]
            if len(w) > 1:
                tx += w[-1][2] - w[0][2]
            v = [int(r.split(",")[7]) for r in open(f"{raw}/{tag}-node{i}.csv") if r[0].isdigit() and ca <= int(r.split(",")[0]) <= cb]
            cpu.append(round(sum(v) / len(v)) if v else "")
        ops, rate, p50 = int(res[ph][3]), float(res[ph][5]), int(res[ph][7])
        rows.append([stage, build, data, arm, size, clients, rep, ph, ops, rate, p50, round(tx / max(ops, 1) / 1024, 1), *cpu])
head = ["stage", "build", "data", "arm", "size", "clients", "rep", "phase", "ops", "ops_per_s", "p50_us", "net_kb_sent_per_op", "cpu0", "cpu1", "cpu2", "cpu3", "cpu4"]
with open(os.path.join(d, "derived.csv"), "w", newline="") as f:
    w = csv.writer(f); w.writerow(head); w.writerows(rows)
groups = defaultdict(list)
for r in rows:
    groups[tuple(r[:6]) + (r[7],)].append(r)
with open(os.path.join(d, "means.csv"), "w", newline="") as f:
    w = csv.writer(f)
    w.writerow(["stage", "build", "data", "arm", "size", "clients", "phase", "runs", "ops_per_s_mean", "ops_per_s_min", "ops_per_s_max", "p50_us_mean", "net_kb_sent_per_op", "cpu0_mean"])
    for k in sorted(groups, key=lambda k: (k[0], k[2], k[4], k[5], k[6], k[1], k[3])):
        g = groups[k]; rate = [r[9] for r in g]; c0 = [r[12] for r in g if r[12] != ""]
        w.writerow([*k, len(g), round(sum(rate) / len(g), 1), min(rate), max(rate), round(sum(r[10] for r in g) / len(g)),
                    round(sum(r[11] for r in g) / len(g), 1), round(sum(c0) / len(c0)) if c0 else ""])
print(open(os.path.join(d, "means.csv")).read())
