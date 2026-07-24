# Technical Roadmap

## The paper

> **WarpDrive: A Composable Storage Substrate for Disaggregated Data Systems**
> Tejus Chandrashekar. *VLDB 2026 Workshop: Fourth International Workshop on Composable Data Management Systems.*
> Accepted as a **lightning talk** at CMDS.

Link to be updated once proceedings are out.

## Motivation: object storage for systems

Object stores became the default storage layer for disaggregated systems for good reasons: durability, cost, elastic scale, and decoupling storage from compute. What comes bundled with that, though, is a narrow, opaque interface: `PUT`, `GET`, `DELETE`. That's a fine interface for application software uploading customer files and archival data, where latency and internal object layout don't matter much. It's not fine for systems software built on top of it. (This same latency gap for applications is part of why S3 itself later introduced S3 Express One Zone as a separate, low-latency storage class rather than fixing it in the base interface.) A distributed training job checkpointing sharded model state, a disaggregated database replaying a page's delta chain, an agent framework handing state off between supersteps: each ends up rebuilding some version of the same handful of concerns above that interface, namely batching many small I/Os into one, getting a consistent view across several objects, knowing which objects are actually safe to reclaim, and controlling which objects end up physically near each other. Today that work happens piecemeal, once per application, above a storage layer that has no idea any of it is happening.

The question we're chasing is what happens if the storage layer is allowed to know enough about how it's being used to offer some of this as primitives, and applications are given enough visibility into storage to actually use them: mutual visibility, rather than each side quietly compensating for the other's opacity. That's what we mean when we call this "object storage for systems": the primary consumer we're designing for is another piece of infrastructure (a database, a training loop, an agent runtime), not an application uploading and archiving customer files.

## Where we actually are (please read this as a hypothesis, not a spec)

The paper names four candidate needs, I/O batching, multi-object consistency, liveness-aware reclamation, and co-residency control, because that's what fell out of looking at three workloads: distributed AI training (IBM Vela), a disaggregated database (Neon), and an agentic execution framework (LangGraph). Three workloads is a small, non-random sample picked because we could find public detail on each, not because we believe it's representative of every disaggregated system. We don't want to bolt a permanent API surface onto four things we noticed once. Expect this list to shrink, grow, or get reorganized as we look at more workloads and get feedback from the workshop.

We also only went deep, codebase-grounded, not just architecture-diagram-grounded, on one case: Neon's open-source pageserver. The analytical model in the paper (the tradeoff between fragmented reconstruction, i.e. *k* independent `GET`s per cold read, and bundled reconstruction, i.e. an *O(k)* read-modify-write on every append because S3 objects are immutable) comes from reading that code, not from a general theory of disaggregated databases. Aurora and similar systems look structurally similar from the outside, but we haven't verified the model against them yet; that's flagged as future work, not a claim we're making today.

WarpDrive's angle on that specific tradeoff is to store independently addressable deltas in mutable, co-located slabs and retrieve them with a single batched I/O, instead of choosing between "many small immutable objects" and "one big object rewritten on every append." We're currently evaluating that prediction against a flat-object-store baseline across increasing delta-chain lengths; preliminary numbers are what get presented at the workshop, not what's written here. Early, preliminary results from a Neon + sysbench-tpcc run are up at [neon_tpcc_results.html](https://vitality-ai.github.io/warpdrive/site/characterization/neon_tpcc_results.html); treat the numbers there as a work-in-progress characterization, not a finished benchmark.

## Here's all the things we are looking at

The reference list below predates the paper by a while, and it's mostly systems papers rather than a literature review written to support a thesis. Rather than force each one into the four candidate needs above (see the caution above about over-committing to those), here's roughly how we've been using them, including where we're deliberately *not* taking a paper's conclusions on faith just because it came out of a large, well-resourced lab.

- **The opposing view: pushing complexity to the client, not the storage layer.** Muehleisen's ["case for client-side protocol redesign"](https://www.vldb.org/pvldb/vol10/p1022-muehleisen.pdf) [6] and Durner & Leis's ["exploiting cloud object storage for high-performance analytics"](https://t.co/tEAygOhPyU) [12] both make the client smarter about a dumb object store rather than the reverse. WarpDrive's premise is close to the mirror image of that. We keep these in the roadmap as the strongest counter-argument to sharpen against, not as evidence we're right; moving concerns into the storage layer only helps if it doesn't just relocate the same complexity somewhere less visible.

- **Built systems we're learning from, not copying wholesale.** [Haystack](https://www.usenix.org/legacy/event/osdi10/tech/full_papers/Beaver.pdf) [3] (Facebook's photo storage, OSDI'10) and [F4](https://www.cs.princeton.edu/~wlloyd/papers/f4-osdi14.pdf) [5] are the lineage already described in [`Technical-Architecture.md`](Technical-Architecture.md); [Tectonic](https://www.usenix.org/system/files/fast21-pan.pdf) [4] (Facebook's later exabyte-scale filesystem, FAST'21) is the same lineage taken further, consolidating many storage tenants behind one system via hash-sharded, disaggregated metadata, which is a consolidation problem adjacent to (but not the same as) the workload-awareness problem we're after. [ShardStore](https://jamesbornholt.com/papers/shardstore-sosp21.pdf) [13] (Amazon's formally-verified S3 backend) is a useful reminder of how seriously a "boring" object storage layer has to be taken once real systems depend on it, a reminder, not a design to reuse. [Megastore](https://research.google/pubs/megastore-providing-scalable-highly-available-storage-for-interactive-services/) [14] is the classic answer to multi-object consistency, but it solves it for a query language sitting on Bigtable, not for raw objects: relevant context, not a template. [Chain replication](https://static.usenix.org/events/osdi04/tech/full_papers/renesse/renesse.pdf) [15] is the standard answer for throughput-preserving replication and would matter if/when the [erasure-coding fault-tolerance work](https://github.com/cia-labs/Storage-service/issues/72) below gets designed, but it was built for a different scale and environment than ours, so we'd want to validate before assuming it transfers.

- **Application usage.** This is the part we want to lean on more, per the paper's own framing: [IBM's writeup of the infrastructure behind Vela](https://arxiv.org/abs/2407.05467) [21] and [DeepSeek's 3FS design notes](https://github.com/deepseek-ai/3FS/blob/main/docs/design_notes.md) [18] describe how large AI shops actually use storage during training, day to day. IBM's [object storage for deep learning frameworks](https://dl.acm.org/doi/pdf/10.1145/3286490.3286562) [20] is directly about the client-side batching and prefetching data loaders build over S3 today, a concrete instance of the I/O-batching pain point. [LangGraph's persistence docs](https://langchain-ai.github.io/langgraph/concepts/persistence/) [23] describe the blocking versioned reads superstep handoffs need to avoid polling, one of the direct sources for the multi-object-consistency observation. [Jack Vanlightly's analysis of Neon](https://jackvanlightly.com/analyses/2023/11/15) [22] is the closest thing we have to an outside, readable account of the pageserver internals the analytical model is grounded in.

- **Placement and hardware-aware ideas, different granularity.** Google's [Cachestack](https://www.usenix.org/system/files/atc22-yang-tzu-wei.pdf) [19] (a knapsack solver deciding SSD/HDD placement) and the [ML-for-storage-I/O-throughput](https://dl.acm.org/doi/10.1145/3568429) paper [17] are both placement/scheduling ideas that rhyme with "co-residency control," but at the granularity of whole files across storage tiers, not deltas within a single object. Worth reading for technique, not assuming it ports over 1:1 to a very different granularity. [Per-file virtualization for userspace persistent-memory filesystems](https://www.usenix.org/conference/fast23/presentation/zhong) [16] is probably the closest existing design vocabulary to "mutable co-located slabs," just applied to PM filesystems instead of object storage. [ADMS](https://www.adms-conf.org/) [11] (a VLDB-associated workshop, in the same spirit as the one this paper is going to) is where a lot of this hardware-aware, workload-specific optimization work tends to show up: worth tracking as a venue, not a single paper.

- **Tail-latency work we're reacting to.** [Pang & Wang's tail-latency reduction for storage-disaggregated databases](https://doi.org/10.1145/3786688) [24] (SIGMOD/Proc. ACM Manag. Data, 2026) measures the same delta-chain-reconstruction trap empirically and responds with Replay-as-a-Service, relocating replay compute to idle instances, leaving the object layout itself untouched. We're explicitly trying the complementary lever (changing the layout), not claiming it's a better fix, just a different one worth having data on.

- **Provenance and building blocks, not part of the systems-positioning story above.** [SeaweedFS](https://github.com/seaweedfs/seaweedfs) [9] ("started like how we started") plus [LMDB](http://www.lmdb.tech/doc/?ref=blog.meilisearch.com) [1], [skip lists](https://dl.acm.org/doi/pdf/10.1145/78973.78977) [7], and [Firebase's push-ID scheme](https://gist.github.com/mikelehen/3596a30bd69384624c11) [10] are lower-level, implementation-facing references for the metadata layer, kept here as building-block references, independent of whichever way the four-primitives question shakes out.

- **Still-open, unrelated to the paper.** The full-text-search references ([Meilisearch's writeup](https://blog.meilisearch.com/how-full-text-search-engines-work/) [2], [DIAMOND](https://diamond.cs.cmu.edu/whatisdiamond.html) [8]) back the still-unbuilt Search work below, unrelated to the VLDB paper's argument.

We're deliberately not trying to make every reference "prove" the paper's thesis. Several of them (Megastore, Cachestack, ShardStore in particular) are included *because* they solve an adjacent problem differently than we're proposing to.

## Open threads

Ongoing project work that still needs design input.

| Area | Status |
|------|--------|
| Storage: Key/Value, Files, and Blobs | Core, in active development |
| Fault tolerance: erasure coding for data replication | Seeking design contribution ([discussion](https://github.com/cia-labs/Storage-service/issues/72)) |
| User access management | [Vitality Console](https://github.com/vitality-ai/Vitality-console) |
| Search | Seeking design contribution ([discussion](https://github.com/cia-labs/Storage-service/issues/35)) |
| Availability | Seeking design contribution, no discussion thread open yet |
| Client library | S3-compatible / custom client currently Python-only ([python-sdk](https://github.com/vitality-ai/python-sdk)) |
| Compute and storage infrastructure research | [NexCSAD](https://github.com/vitality-ai/NexCSAD) |

### References

1. http://www.lmdb.tech/doc/?ref=blog.meilisearch.com
2. https://blog.meilisearch.com/how-full-text-search-engines-work/
3. https://www.usenix.org/legacy/event/osdi10/tech/full_papers/Beaver.pdf
4. https://www.usenix.org/system/files/fast21-pan.pdf
5. https://www.cs.princeton.edu/~wlloyd/papers/f4-osdi14.pdf
6. https://www.vldb.org/pvldb/vol10/p1022-muehleisen.pdf - A case for client side protocol redesign.
7. https://dl.acm.org/doi/pdf/10.1145/78973.78977 - Skip Lists - William Pugh's paper
8. https://diamond.cs.cmu.edu/whatisdiamond.html - Searching without an index, especially image data.
9. https://github.com/seaweedfs/seaweedfs - Started like how we started.
10. https://gist.github.com/mikelehen/3596a30bd69384624c11 - Firebase's push ID generation.
11. https://www.adms-conf.org/ - Hardware optimization for workflow types.
12. https://t.co/tEAygOhPyU - TUM's high-performance object storage for analytics.
13. https://jamesbornholt.com/papers/shardstore-sosp21.pdf - Amazon S3's formal verification and technical details.
14. https://research.google/pubs/megastore-providing-scalable-highly-available-storage-for-interactive-services/ - Google's Megastore.
15. https://static.usenix.org/events/osdi04/tech/full_papers/renesse/renesse.pdf - Chain replication for high availability and throughput in storage services.
16. https://www.usenix.org/conference/fast23/presentation/zhong - Per-file virtualization for userspace persistent memory filesystems.
17. https://dl.acm.org/doi/10.1145/3568429 - ML to improve storage I/O throughput for ML.
18. https://github.com/deepseek-ai/3FS/blob/main/docs/design_notes.md - DeepSeek's 3FS design.
19. https://www.usenix.org/system/files/atc22-yang-tzu-wei.pdf - Google's Cachestack, determining file placement across SSD/HDD via knapsack.
20. https://dl.acm.org/doi/pdf/10.1145/3286490.3286562 - Object storage for deep learning frameworks, IBM.
21. https://arxiv.org/abs/2407.05467 - Belgodere, Dognin, et al. The infrastructure powering IBM's gen AI model development (Vela).
22. https://jackvanlightly.com/analyses/2023/11/15 - Jack Vanlightly. Neon serverless PostgreSQL, ASDS chapter 3.
23. https://langchain-ai.github.io/langgraph/concepts/persistence/ - LangChain. LangGraph persistence documentation.
24. https://doi.org/10.1145/3786688 - Xi Pang and Jianguo Wang. Reducing tail latency in storage-disaggregated database systems (Replay-as-a-Service). Proc. ACM Manag. Data, 4(1), Article 74, 2026.
