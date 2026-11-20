#!/usr/bin/env python3
"""Group perf stacks (folded) into cost categories. Usage: cpu_breakdown.py <prof dir>
Writes <phase>-<node>-categories.csv and <phase>-<node>-demangled.folded, prints tables."""
import re, sys, collections, csv
P = sys.argv[1]

def demangle(sym):
    """Readable path from a Rust v0 symbol: crate::module::fn (heuristic, for labels and grouping)."""
    if not sym.startswith("_R"):
        return sym
    s = re.sub(r"(?<=[A-Z])s[0-9A-Za-z]*_", "", sym[2:])
    out, i = [], 0
    while i < len(s):
        if s[i].isdigit() and (i == 0 or not s[i - 1].isdigit()):
            j = i
            while j < len(s) and s[j].isdigit():
                j += 1
            n = int(s[i:j])
            if j < len(s) and s[j] == "_":
                j += 1
            name = s[j:j + n]
            if n and len(name) == n and re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", name):
                out.append(name); i = j + n; continue
        i += 1
    return "::".join(out) or sym

CATS = [  # (category, regex on the leaf frame, after demangling); first match wins
    ("kernel: scheduler, futex, wakeups", r"finish_task_switch|schedule|futex|try_to_wake_up|_raw_spin|wake_up|select_task|enqueue|dequeue|pick_next|update_curr|psi_|switch_|native_sched|__x64_sys_sched|ttwu|sched"),
    ("kernel: network (TCP, virtio)", r"tcp_|ip_|sock|skb|net_|virtnet|virtqueue|__dev|napi|inet|ep_poll|epoll|nf_|__netif|recv|send|detach_buf|irq|softirq"),
    ("kernel: storage (ext4, NVMe, sync)", r"nvme|ext4|jbd2|blk_|bio|iomap|filemap|fdatasync|fsync|block|page_cache|folio|writeback"),
    ("kernel: other", r"^\[k\]|_k$"),
    ("hashing: SHA-256, MD5, checksums", r"sha2|sha256|md5|hmac|checksum|crc32|blake|digest"),
    ("erasure coding", r"reed_solomon|galois|erasure|encode_shards|gf_"),
    ("JSON (serde_json)", r"serde_json|serde::"),
    ("HTTP routing (actix router, regex)", r"regex|actix_router|ResourceDef"),
    ("HTTP/1 parsing and responses (actix-http)", r"actix_http|httparse|actix_web"),
    ("gRPC to peers (tonic, h2, hyper, prost)", r"tonic|h2::|hpack|hyper|prost|tower"),
    ("malloc, free, copies", r"malloc|free|realloc|memcpy|memmove|memset|_int_|alloc::|calloc|tcache"),
    ("async runtime and locks (tokio, parking_lot)", r"tokio|parking_lot|crossbeam|futures|std::sync|std::thread|syscall|pthread|__lll|mio"),
    ("WarpDrive code", r"warp_drive"),
]

KENTRY = re.compile(r"entry_SYSCALL|^asm_|ret_from_fork|irqentry|^common_interrupt|^sysvec_|^do_syscall")
KCATS = [  # searched over the kernel part of the stack, in this order
    ("kernel: storage (ext4, NVMe, sync)", r"ext4|nvme|jbd2|fdatasync|vfs_fsync|pwrite|pread|statx|blk_|iomap|filemap_|openat|newfstat"),
    ("kernel: network (TCP, virtio)", r"tcp_|sock_|inet_|virtnet|net_rx|napi|__sys_sendto|__sys_recvfrom|sendmsg|recvmsg|skb|ip_|__netif|vring_interrupt"),
    ("kernel: scheduler, futex, wakeups", r"futex|schedule|eventfd|epoll|sched_yield|wake_up|finish_task_switch|ttwu|nanosleep|hrtimer"),
    ("kernel: memory (page faults, mmap)", r"page_fault|exc_page|mmap|munmap|brk|madvise|clear_page|mprotect"),
]
UNRESOLVED = re.compile(r"^\[.*\]$|^0x[0-9a-f]+$")

def leaf_cat(frames):
    k = next((i for i, f in enumerate(frames) if KENTRY.search(f)), None)
    if k is not None or frames[-1].endswith("_[k]"):
        kpart = ";".join(frames[k or 0:])
        for cat, rx in KCATS:
            if re.search(rx, kpart):
                return cat
        return "kernel: other"
    # User space: the leaf, or its nearest caller with a symbol when the leaf is an unresolved libc frame.
    named = [f for f in reversed(frames) if not UNRESOLVED.match(f)]
    name = demangle(named[0]) if named else ""
    for cat, rx in CATS:
        if not cat.startswith("kernel") and re.search(rx, name):
            return cat
    if re.search(r"tracing::", name):
        return "tracing spans (no output at WARN)"
    if re.search(r"clock_gettime|vdso", name):
        return "clock reads"
    if re.search(r"^http::|bytes::", name):
        return "HTTP/1 parsing and responses (actix-http)"
    return "other user space"

for ph in ("put", "get"):
    for node in ("node0", "node1"):
        path = f"{P}/{ph}-{node}.folded"
        tot = 0; cats = collections.Counter(); wd = collections.Counter()
        with open(f"{P}/{ph}-{node}-demangled.folded", "w") as out:
            for line in open(path):
                stack, n = line.rstrip("\n").rsplit(" ", 1); n = int(n)
                frames = stack.split(";")
                tot += n; cats[leaf_cat(frames)] += n
                dm = [demangle(f.replace("_[k]", "")) + ("_[k]" if f.endswith("_[k]") else "") for f in frames]
                out.write(";".join(dm) + f" {n}\n")
                # Deepest WarpDrive function on the stack: which of our code paths the sample belongs to.
                ours = [f for f in dm if f.startswith("warp_drive::")]
                if ours:
                    parts = ours[-1].split("::")
                    wd["::".join(parts[:4])] += n
        with open(f"{P}/{ph}-{node}-categories.csv", "w", newline="") as f:
            w = csv.writer(f); w.writerow(["category", "share_pct"])
            for c, n in cats.most_common():
                w.writerow([c, round(100 * n / tot, 1)])
        print(f"\n== {ph} {node}: {tot} samples")
        for c, n in cats.most_common():
            print(f"  {100*n/tot:5.1f}%  {c}")
        print("  -- deepest WarpDrive frame (inclusive of what it calls), top 12:")
        for c, n in wd.most_common(12):
            print(f"  {100*n/tot:5.1f}%  {c}")
