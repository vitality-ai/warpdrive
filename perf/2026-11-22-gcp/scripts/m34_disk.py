#!/usr/bin/env python3
"""Microbenchmarks 3 and 4, on a node's NVMe (ext4): m34_disk.py <dir> <out json>.
3: a segment roll: create a file, fsync it, fsync the directory. 4: copying survivors between two files with
copy_file_range against pread+pwrite, at 2 KiB, 64 KiB and 1 MiB per shard, then one fsync of the target."""
import json, os, sys, time, statistics as st
d, out = sys.argv[1], sys.argv[2]; os.makedirs(d, exist_ok=True); res = {}
lat = []
dfd = os.open(d, os.O_RDONLY)
for i in range(300):
    t0 = time.perf_counter()
    fd = os.open(f"{d}/roll{i:04d}.seg", os.O_CREAT | os.O_RDWR, 0o644); os.fsync(fd); os.fsync(dfd); os.close(fd)
    lat.append((time.perf_counter() - t0) * 1e3)
lat.sort(); res["segment_roll_ms"] = {"p50": round(lat[150], 3), "p99": round(lat[296], 3), "mean": round(st.mean(lat), 3)}
for f in os.listdir(d):
    if f.startswith("roll"): os.unlink(f"{d}/{f}")
src = f"{d}/src.seg"
with open(src, "wb") as f: f.write(os.urandom(256 << 20)); f.flush(); os.fsync(f.fileno())
os.system("sync; echo 3 | sudo tee /proc/sys/vm/drop_caches > /dev/null")
for size in (2048, 65536, 1 << 20):
    for mode in ("copy_file_range", "pread+pwrite"):
        n = min((128 << 20) // size, 20000); dst = f"{d}/dst-{mode}-{size}.seg"
        s = os.open(src, os.O_RDONLY); t = os.open(dst, os.O_CREAT | os.O_RDWR | os.O_TRUNC, 0o644)
        t0 = time.perf_counter()
        for i in range(n):
            off = (i * 7919 * size) % ((256 << 20) - size)
            if mode == "copy_file_range":
                left, o_in, o_out = size, off, i * size
                while left:
                    c = os.copy_file_range(s, t, left, o_in, o_out); left -= c; o_in += c; o_out += c
            else:
                os.pwrite(t, os.pread(s, size, off), i * size)
        os.fsync(t); el = time.perf_counter() - t0
        os.close(s); os.close(t); os.unlink(dst)
        res[f"copy_{mode}_{size}"] = {"shards": n, "MB_per_s": round(n * size / el / 1e6, 1), "us_per_shard": round(el / n * 1e6, 1)}
os.unlink(src); os.close(dfd)
json.dump(res, open(out, "w"), indent=1); print(json.dumps(res))
