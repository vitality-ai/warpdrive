import sys, glob, os, re, collections, statistics as st, csv
R=sys.argv[1]; out=sys.argv[2] if len(sys.argv)>2 else None; rows=[]; skipped=0
def load(d,f):
    global skipped
    o={}
    for l in open(f"{d}/{f}"):
        parts=l.strip().rsplit("|",4)
        if len(parts)!=5 or not all(x.isdigit() for x in parts[1:4]): skipped+=1; continue
        n,v,nv,c,tid=parts; n=n.split("|")[0]; o[tid]=(n.rstrip("0123456789-") or n,int(v)+int(nv),int(c))
    return o
for d in sorted(glob.glob(f"{R}/r*_*")):
    m=re.match(r"r(\d+)_(\w+)_(\d)c",os.path.basename(d)); run,arm,cores=int(m[1]),m[2],int(m[3])
    b={l.split(",")[0]:l.strip().split(",") for l in open(f"{d}/bench.csv") if l[:3] in("put","get")}
    p,g=b["put"],b["get"]; po,go=int(p[3]),int(g[3])
    a,e,z=load(d,"threads-start.txt"),load(d,"threads-put_end.txt"),load(d,"threads-get_end.txt")
    def per(x,y,ops):
        cs=collections.Counter(); cpu=collections.Counter()
        for t,(n,c,u) in y.items(): q=x.get(t,(n,0,0)); cs[n]+=c-q[1]; cpu[n]+=u-q[2]
        return sum(cs.values())/ops, sum(cpu.values())*10000/ops, {k:(round(cs[k]/ops,1), round(cpu[k]*10000/ops)) for k in ("wd-bridge","wd-bridge-rt","tokio-rt-worker","actix-rt")}
    pcs,pcpu,ppool=per(a,e,po); gcs,gcpu,_=per(e,z,go)
    rows.append(dict(run=run,arm=arm,node0_cores=cores,put_per_s=float(p[5]),get_per_s=float(g[5]),put_p99_ms=int(p[9])/1000,get_p99_ms=int(g[9])/1000,
        put_cpu_us=round(pcpu),get_cpu_us=round(gcpu),put_switches=round(pcs,1),get_switches=round(gcs,1),
        bridge_pool_sw=ppool["wd-bridge"][0],bridge_pool_us=ppool["wd-bridge"][1],bridge_rt_sw=ppool["wd-bridge-rt"][0],bridge_rt_us=ppool["wd-bridge-rt"][1],
        http_worker_sw=ppool["actix-rt"][0],http_worker_us=ppool["actix-rt"][1],errors=int(p[4])+int(g[4]),node0_panics=open(f"{d}/node0.panics").read().strip()))
if out:
    w=csv.DictWriter(open(out,"w",newline=""),fieldnames=list(rows[0])); w.writeheader(); w.writerows(sorted(rows,key=lambda r:r["run"]))
print(f"(skipped {skipped} partial counter lines)")
for r in sorted(rows,key=lambda r:(r['node0_cores'],r['arm'],r['run'])):
    print(f"{r['arm']:5} {r['node0_cores']}c r{r['run']} PUT {r['put_per_s']:6.0f} GET {r['get_per_s']:6.0f} cpu/PUT {r['put_cpu_us']:5} sw/PUT {r['put_switches']:4} | bridge pool {r['bridge_pool_sw']} sw {r['bridge_pool_us']}us, bridge rt {r['bridge_rt_sw']} sw {r['bridge_rt_us']}us, http {r['http_worker_sw']} sw {r['http_worker_us']}us | err {r['errors']} panics {r['node0_panics']}")
g=collections.defaultdict(list)
for r in rows: g[(r['node0_cores'],r['arm'])].append(r)
f=lambda L,k: st.mean(x[k] for x in L)
for c in (2,4):
    a,b=g[(c,'sync')],g[(c,'async')]
    print(f"{c} cores: PUT {f(a,'put_per_s'):.0f} -> {f(b,'put_per_s'):.0f} ({100*(f(b,'put_per_s')/f(a,'put_per_s')-1):+.1f}%), GET {f(a,'get_per_s'):.0f} -> {f(b,'get_per_s'):.0f} ({100*(f(b,'get_per_s')/f(a,'get_per_s')-1):+.1f}%), cpu/PUT {f(a,'put_cpu_us'):.0f} -> {f(b,'put_cpu_us'):.0f} us, sw/PUT {f(a,'put_switches'):.1f} -> {f(b,'put_switches'):.1f}, PUT p99 {f(a,'put_p99_ms'):.0f} -> {f(b,'put_p99_ms'):.0f} ms | spread sync {[round(x['put_per_s']) for x in a]} async {[round(x['put_per_s']) for x in b]}")
