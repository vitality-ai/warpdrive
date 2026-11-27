#!/usr/bin/env python3
"""derive.py <run dir>: per run and phase, network bytes sent by all nodes per op and mean CPU % per node.
Reads raw/<tag>.csv (s3_bench), raw/<tag>-times.txt, raw/<tag>-net<i>.txt (netmon.sh), raw/<tag>-node<i>.csv
(soak_sampler.sh). Writes derived.csv. The PUT phase is the first 60 s after t0, GET the rest."""
import csv, glob, os, sys
d = sys.argv[1]; raw = os.path.join(d, "raw"); rows = []
for times in sorted(glob.glob(f"{raw}/*-times.txt")):
    tag = os.path.basename(times)[:-len("-times.txt")]
    data, arm, size = tag.split("-")
    t0, t1 = map(int, open(times).read().split())
    res = {l.split(",")[0]: l.strip().split(",") for l in open(f"{raw}/{tag}.csv") if l.startswith(("put", "get"))}
    for ph, (a, b, ca, cb) in {"put": (t0, t0 + 60, t0 + 5, t0 + 60), "get": (t0 + 60, t1, t0 + 65, t1)}.items():
        tx, cpu = 0, []
        for i in range(5):
            net = [list(map(int, l.split()[:3])) for l in open(f"{raw}/{tag}-net{i}.txt") if len(l.split()) >= 3]
            w = [r for r in net if a <= r[0] <= b]
            if len(w) > 1:
                tx += w[-1][2] - w[0][2]
            v = [int(r.split(",")[7]) for r in open(f"{raw}/{tag}-node{i}.csv") if ca <= int(r.split(",")[0]) <= cb]
            cpu.append(round(sum(v) / len(v)) if v else "")
        ops, rate = int(res[ph][3]), float(res[ph][5])
        rows.append([data, arm, size, ph, ops, rate, round(tx / max(ops, 1) / 1024, 1), *cpu])
with open(os.path.join(d, "derived.csv"), "w", newline="") as f:
    w = csv.writer(f); w.writerow(["data", "arm", "size", "phase", "ops", "ops_per_s", "net_kb_sent_per_op", "cpu0", "cpu1", "cpu2", "cpu3", "cpu4"]); w.writerows(rows)
