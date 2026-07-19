# Slab Co-location — Design Notes & Observations

## Why 1 GB raw data → 14 GB in warpdrive

### Short answer
Neon's WAL-based storage architecture has a 10–15× write amplification factor relative to raw database size. This is expected and well-understood.

### Detailed explanation

**Raw TPC-C data (scale=10, tables=1):**
| Table | Rows | Raw size |
|-------|------|----------|
| stock1 | 1,000,001 | ~800 MB |
| order_line1 | 3,001,242 | ~300 MB |
| customer1 | 300,000 | ~150 MB |
| orders1 + history1 | 600,000 | ~60 MB |
| **Total** | **~5.2M rows** | **~1.3 GB** |

**Neon storage amplification factors:**

1. **WAL overhead (~2×)**: Every heap page write also generates a WAL record. A 1 GB database requires ~2 GB of WAL to create.

2. **Delta layers**: The pageserver transforms WAL into immutable delta layer files, one set per checkpoint interval (default: every 256 MB of WAL). Delta layers cover all page changes in a WAL range.

3. **Image layers (~3–5×)**: At each checkpoint, the pageserver creates image layers — full snapshots of all live pages at that LSN. With a final database size of ~5 GB and multiple checkpoints during the 2.5-hour load, each image layer set is ~5 GB.

4. **Accumulation (no GC during load)**: Old layers are not compacted or GC'd during a bulk load. All delta + image layers from all checkpoints accumulate. With 2–3 checkpoint cycles during the scale=10 prepare, total layer storage = initial image + delta₁ + image₁ + delta₂ + image₂ ≈ 14 GB.

**Measured:**
- Raw data: ~1.3 GB
- Objects in warpdrive: 2,033
- Total layer data: 15.7 GB (183,442 extents, 0 overlaps)
- Slab file on disk: 14 GB (sparse file; 23 GB logical span, 7.5 GB holes from slot boundaries)

---

## Slab batch OOM crash — root cause & fix

### Problem
The original `warpd_slab_batch_get` handler in `warpd.rs` loaded **all matching objects into a single `Vec<u8>` in memory** before sending the response:

```rust
let mut body: Vec<u8> = Vec::new();
for (key, extents) in &objects {
    let data = storage.read_object(...)?;  // reads full layer into RAM
    body.extend_from_slice(&data);          // accumulates ALL layers
}
HttpResponse::Ok().body(body)              // sends all at once
```

With 15.7 GB of layer data and only 6.2 GB of available RAM, this OOM-killed warpdrive every time the pageserver fired a slab pre-warm (`prefetch_slab_layers`) on cold restart.

### Fix (streaming)
Replaced with a `futures::stream::StreamExt::flat_map` based streaming response that reads one object at a time:

```rust
let object_stream = stream::iter(objects)
    .flat_map(|(key, extents)| {
        let data = storage.read_object(...)?;  // one layer at a time
        stream::iter(vec![header_chunk, data_chunk, crlf_chunk])
    })
    .chain(stream::once(async { closing_boundary }));

HttpResponse::Ok().streaming(object_stream)
```

Each layer file is read from disk, yielded to the HTTP response, and freed before the next one is read. Peak RAM = size of one layer file (max ~265 MB) rather than all 15.7 GB.

---

## `STORAGE_DIRECTORY` warning flood

Each call to `StorageService::new()` inside the slab batch loop triggered a `warn!("Storage directory not defined in environment")` — once per layer object. With 2,033 objects this produced thousands of log lines per cold-start.

**Fix:** Downgraded to `debug!` in `local_store.rs`. The warning is non-actionable: warpdrive correctly defaults to `./storage` when the env var is unset, and the storage works fine.

---

## Slab pre-warm disabled for T=1,4,8,16 benchmark run

Even with streaming, downloading 15.7 GB of layers at cold-start would take longer than the TPC-C warm-up window (~5 s before sysbench starts). The slab pre-warm is most valuable for small databases (< 1 GB) where all layers fit comfortably in RAM and can be downloaded in seconds.

For this benchmark:
- `WARPD_SLAB_BASE_URL` is **not** set in `run_scaling_slab.py`'s `restart_pageserver`
- The slab **co-location** benefit is still present: layers with the same timeline hint are physically adjacent in `neon.bin`, reducing random-read scatter during on-demand downloads
- Batch pre-warm can be re-enabled for smaller databases or after adding a size cap / partial pre-warm feature

---

## Slab slot allocation and file sparsity

- `DEFAULT_SLAB_WINDOW = 4 MB`
- All 2,033 objects share hint = timeline_id, so all land in consecutive 4 MB slots
- For each object: if it fits in the current slot's remaining space → appended in-place; otherwise → new slot allocated
- Large objects (> 4 MB) always get a new slot; their data spans multiple consecutive slots in the file
- File is sparse: slot boundaries that happen to be "between" large object writes contain no allocated disk blocks
- Logical file span: 23.2 GB; actual allocated: 14 GB (7.5 GB of sparse holes)
