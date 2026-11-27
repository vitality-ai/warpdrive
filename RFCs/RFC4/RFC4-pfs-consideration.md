# RFC 4 Addendum: Parallel Filesystem as Fault Tolerance

**Status:** Consideration  
**Date:** June 2026  
**Parent:** [RFC 4 — Fault Tolerance](RFC4-fault-tolerance.md)

---

## The Idea

Instead of building replication into Warpdrive, mount all nodes on a shared parallel filesystem (PFS). The PFS handles data durability and presents a single coherent POSIX namespace to every node. Warpdrive nodes become stateless compute — they read and write exactly as they do today, and the PFS ensures every node sees the same files with the same content.

---

## Why NFS and GlusterFS Don't Work Here

The obvious first instinct is to put storage on NFS or GlusterFS. This fails for metadata.

SQLite's writer-exclusion relies on POSIX `fcntl` advisory locks. Standard network filesystems (NFS v3/v4, GlusterFS, Samba) do not reliably propagate `fcntl` locks across hosts. Two Warpdrive nodes writing to the same SQLite file on NFS can both believe they hold the write lock simultaneously — the result is database corruption under any concurrent load. SQLite's own documentation warns against this explicitly.

Parallel filesystems (Lustre, GPFS/IBM Spectrum Scale, BeeGFS, DAOS) implement full POSIX semantics including correct cross-node `fcntl` locks. SQLite works on these.

---

## What Still Needs Coordination: Segment Ownership

Even with a coherent PFS, two nodes must not append to the same segment file simultaneously. Concurrent appends at the same offset corrupt the segment — POSIX does not guarantee atomicity of multi-byte appends across hosts even on a PFS.

**Fix: static segment ownership partitioning.**

Each node writes only to segments it owns, namespaced by node ID in the filename:

```
storage/<tenant>/<bucket>/<node-id>-<uuid>.seg
```

Reads are unaffected — any node can read any segment since the PFS provides a single namespace. The SQLite `objects` table already stores `(file_id, offset, length)` per extent, so reads resolve to the correct file regardless of which node wrote it.

**This is the only Warpdrive code change required**: one config value (node ID), used when naming new segment files. Everything else — auth, handlers, metadata writes, reads — is unchanged.

---

## SQLite Single-Writer Ceiling

Multiple Warpdrive nodes sharing one SQLite file means all metadata writes queue behind SQLite's internal writer lock. This is a **performance ceiling, not a correctness issue** — the database stays consistent, but write throughput is bounded.

For most workloads metadata writes are not the bottleneck. If they become one:

| Mitigation | Complexity | Notes |
|------------|-----------|-------|
| Accept it | None | Valid for low-to-medium write rates |
| WAL mode | Low | One writer + concurrent readers; verify WAL works on your PFS before committing |
| Replace SQLite with PostgreSQL | Medium | Removes ceiling entirely; adds ops dependency |

---

## PFS Options

### Self-Hosted

| System | License | Maturity | Notes |
|--------|---------|---------|-------|
| **BeeGFS** | SSPL (free for non-SaaS use) | Production | Easiest to operate of the self-hosted options; good performance; active community |
| **Lustre** | GPL | Very mature | Industry standard in HPC; complex to self-host; deep feature set |
| **GPFS / IBM Spectrum Scale** | Commercial | Very mature | Excellent POSIX compliance; enterprise support; significant license cost |
| **DAOS** | Apache 2.0 | Newer | Designed for NVMe and persistent memory; high throughput; smaller community |

Minimum self-hosted HA cluster: 3 metadata servers + 3 storage servers (can be colocated on the same 3 machines). Expect 1–2 weeks initial setup and ongoing expertise to operate.

### Managed (Cloud)

| Service | Provider | Approximate cost |
|---------|----------|-----------------|
| **FSx for Lustre** | AWS | ~$0.14–0.29 / GB-month (persistent storage) |
| **FSx for OpenZFS** | AWS | ~$0.09 / GB-month; simpler but single-AZ |
| **Filestore** | GCP | ~$0.20–0.30 / GB-month (High Scale tier) |
| **Azure HPC Cache** | Azure | ~$0.15–0.25 / GB-month |

Managed options eliminate all ops burden. Available in minutes. No expertise required.

---

## Cost Comparison

At 100 TB usable capacity:

| Dimension | Self-hosted PFS (BeeGFS) | Managed PFS (FSx Lustre) | RFC4-A Quorum |
|-----------|------------------------|------------------------|--------------|
| Warpdrive dev effort | 1–2 days | 1–2 days | 4–6 weeks |
| Infrastructure ops burden | High | None | Low |
| Monthly infra cost | ~$2–5k (amortized HW + power) | ~$14–29k | ~$1–3k (3× VMs + disk) |
| External dependencies | PFS cluster (you operate) | Cloud vendor | None |
| Failure tolerance | PFS-defined (typically 1–2 node) | SLA-backed (99.9%+) | 1 of 3 nodes |
| Metadata write ceiling | SQLite single-writer | Same | Per-leader throughput |
| Multi-region | No (PFS is single-datacenter) | No | Yes (quorum across AZs/regions) |
| Horizontal scale | Add PFS storage nodes | Pay more | Add Warpdrive + quorum nodes |

---

## When to Use PFS

| Scenario | Verdict |
|----------|---------|
| Already on AWS with budget, want fastest path | Use FSx for Lustre — zero dev, operational from day one |
| On-prem HPC or research environment with an existing PFS | Mount it; 2-day Warpdrive change; done |
| Greenfield, no existing infrastructure, cost-sensitive | RFC4-A quorum — no external dependencies, lower cost |
| Need active-active multi-region | RFC4-A quorum — PFS is inherently single-datacenter |
| Metadata write throughput is the known bottleneck | Neither — fix SQLite first (WAL or PostgreSQL) |

---

## Open Questions

1. **WAL mode on PFS**: must be verified against the specific PFS before committing. Test under concurrent write load; fall back to journal mode if WAL behaves incorrectly.
2. **Throughput threshold**: at what metadata write rate does the SQLite single-writer ceiling become a real problem? Define this before choosing PFS over quorum.
3. **Vendor lock-in**: managed PFS ties the deployment to a cloud provider. Acceptable if already committed to that cloud; a concern for portable deployments.
4. **If PFS is chosen and metadata throughput later hits the ceiling**: does the team migrate to PostgreSQL, or to RFC4-A quorum? Decide the escalation path upfront to avoid being cornered.
