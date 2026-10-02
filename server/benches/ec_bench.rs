//! Phase 1 step 1 benchmark: standalone encode/decode throughput for
//! `ReedSolomonCoder`, before it's wired into coordinator.rs. Run with
//! `cargo bench --bench ec_bench`.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use warp_drive::cluster::ec::{ErasureCoder, ReedSolomonCoder};

// Fusion's own default erasure code (ASPLOS'25 Fig. 2): 6 data + 3 parity
// blocks = 9 total. Their paper calls this "RS(9,6)" in (n,k) = (total,
// data) notation -- the opposite of this project's (k,m) = (data,parity)
// convention, so it's k=6, m=3 here, not k=9, m=6 (that was an earlier
// mistake, caught and corrected 2026-10-02).
const RS_K: usize = 6;
const RS_M: usize = 3;

fn bench_encode(c: &mut Criterion) {
    let coder = ReedSolomonCoder::new(RS_K, RS_M).unwrap();
    let mut group = c.benchmark_group("ec_encode");
    for size_mb in [1usize, 8, 64] {
        let size = size_mb * 1024 * 1024;
        let data = vec![0xABu8; size];
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(format!("{size_mb}MB")), &data, |b, data| {
            b.iter(|| coder.encode(black_box(data)).unwrap());
        });
    }
    group.finish();
}

fn bench_decode(c: &mut Criterion) {
    let coder = ReedSolomonCoder::new(RS_K, RS_M).unwrap();
    let mut group = c.benchmark_group("ec_decode_one_missing");
    for size_mb in [1usize, 8, 64] {
        let size = size_mb * 1024 * 1024;
        let data = vec![0xABu8; size];
        let encoded = coder.encode(&data).unwrap();
        let mut shards: Vec<Option<Vec<u8>>> = encoded.shards.iter().cloned().map(Some).collect();
        shards[0] = None; // force reconstruction, not the direct-read path

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{size_mb}MB")),
            &(shards, encoded.original_len),
            |b, (shards, original_len)| {
                b.iter(|| coder.decode(black_box(shards), *original_len).unwrap());
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_encode, bench_decode);
criterion_main!(benches);
