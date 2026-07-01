// This is a simple happy-case path demo. It shows primary and replica running
// concurrently against the same Warpdrive bucket, a fault being injected to
// drop the primary, and the replica promoting itself to take over. It does not
// cover split-brain scenarios, network partitions, or multi-replica topologies.

use tidesdb::{TidesDB, Config, ColumnFamilyConfig, LogLevel, ObjectStoreConfig, S3Config};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const ENDPOINT: &str = "localhost:9710";
const BUCKET: &str = "tidesdb-replication";
const ACCESS_KEY: &str = "adminkey";
const SECRET_KEY: &str = "adminsecretkey123456";

const PRIMARY_LOCAL: &str = "./repl-primary";
const REPLICA_LOCAL: &str = "./repl-replica";
const CF: &str = "repl-cf";

fn aws(args: &[&str]) -> std::process::Output {
    Command::new("aws")
        .args(args)
        .args(["--endpoint-url", &format!("http://{}", ENDPOINT)])
        .env("AWS_ACCESS_KEY_ID", ACCESS_KEY)
        .env("AWS_SECRET_ACCESS_KEY", SECRET_KEY)
        .env("AWS_DEFAULT_REGION", "us-east-1")
        .output()
        .expect("aws cli not found")
}

fn s3() -> S3Config {
    S3Config::new(ENDPOINT, BUCKET, ACCESS_KEY, SECRET_KEY)
        .region("us-east-1")
        .use_path_style(true)
        .use_ssl(false)
}

fn rm_local(path: &str) {
    let _ = std::fs::remove_dir_all(path);
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    // Clean slate
    rm_local(PRIMARY_LOCAL);
    rm_local(REPLICA_LOCAL);
    aws(&["s3", "rb", &format!("s3://{}", BUCKET), "--force"]);
    aws(&["s3api", "create-bucket", "--bucket", BUCKET]);
    println!("==> Bucket '{}' ready", BUCKET);

    // Shared flag — main thread flips this to inject the fault
    let fault = Arc::new(AtomicBool::new(false));
    let primary_fault = fault.clone();
    let replica_fault = fault.clone();

    // ---------------------------------------------------------------
    // Primary thread — writes continuously until fault is injected
    // ---------------------------------------------------------------
    let primary_thread = thread::spawn(move || {
        let db = TidesDB::open(
            Config::new(PRIMARY_LOCAL)
                .object_store_s3(s3())
                .object_store_config(
                    ObjectStoreConfig::new()
                        .wal_sync_on_commit(true), // upload WAL on every commit so replica sees writes fast
                )
                .log_level(LogLevel::Info),
        )
        .expect("primary open failed");

        db.create_column_family(CF, ColumnFamilyConfig::default())
            .expect("create cf failed");
        let cf = db.get_column_family(CF).expect("get cf failed");

        let mut i = 0u32;
        loop {
            if primary_fault.load(Ordering::Relaxed) {
                println!("[primary] fault injected — dropping database handle (simulated crash)");
                break; // DB drops here, releasing the lease in Warpdrive
            }
            // Batch 10 keys per transaction — one WAL upload per commit
            let mut txn = db.begin_transaction().expect("txn failed");
            for j in 0..10 {
                let key = format!("key-{:04}", i + j).into_bytes();
                let val = format!("value-{:04}", i + j).into_bytes();
                txn.put(&cf, &key, &val, -1).expect("put failed");
            }
            txn.commit().expect("commit failed");
            println!("[primary] wrote keys {}-{}", i, i + 9);
            i += 10;
            thread::sleep(Duration::from_millis(100));
        }

        i // return how many keys were written before crash
    });

    // Give primary a moment to start up and write a few keys
    thread::sleep(Duration::from_millis(800));

    // ---------------------------------------------------------------
    // Replica thread — reads while primary is live, promotes on fault
    // ---------------------------------------------------------------
    let replica_thread = thread::spawn(move || {
        let db = TidesDB::open(
            Config::new(REPLICA_LOCAL)
                .object_store_s3(s3())
                .object_store_config(
                    ObjectStoreConfig::new()
                        .replica_mode(true)
                        .replica_replay_wal(true)
                        .replica_sync_interval_us(200_000), // poll Warpdrive every 200ms
                )
                .log_level(LogLevel::Info),
        )
        .expect("replica open failed");

        // Poll while primary is healthy — count visible keys each tick
        loop {
            thread::sleep(Duration::from_millis(500));

            if let Ok(cf) = db.get_column_family(CF) {
                let mut visible = 0u32;
                for i in 0u32..1000 {
                    let key = format!("key-{:04}", i).into_bytes();
                    let txn = db.begin_transaction().expect("txn failed");
                    if txn.get(&cf, &key).is_ok() { visible += 1; }
                }
                println!("[replica] visible keys: {}", visible);
            }

            if replica_fault.load(Ordering::Relaxed) {
                break;
            }
        }

        // Primary is gone — wait briefly for the lease to clear, then promote
        println!("[replica] primary fault detected — waiting for lease to clear...");
        thread::sleep(Duration::from_secs(1));

        println!("[replica] promoting to primary...");
        db.promote_to_primary().expect("promotion failed");
        println!("[replica] promoted! now accepting writes");

        let cf = db.get_column_family(CF).expect("get cf after promote failed");

        // Write new keys as the promoted primary
        for i in 0u32..5 {
            let key = format!("promoted-key-{:04}", i).into_bytes();
            let val = b"written by promoted replica".to_vec();
            let mut txn = db.begin_transaction().expect("txn failed");
            txn.put(&cf, &key, &val, -1).expect("put after promote failed");
            txn.commit().expect("commit after promote failed");
            println!("[replica] wrote promoted-key-{:04}", i);
        }

        // Read back a promoted key to confirm
        let txn = db.begin_transaction().expect("txn");
        match txn.get(&cf, b"promoted-key-0002") {
            Ok(v)  => println!("[replica] read back promoted-key-0002 = {}", String::from_utf8_lossy(&v)),
            Err(e) => println!("[replica] read err: {}", e),
        }
    });

    // ---------------------------------------------------------------
    // Fault injection — kill the primary after 3 seconds
    // ---------------------------------------------------------------
    thread::sleep(Duration::from_secs(10));
    println!("\n[fault injection] killing primary after 10 seconds\n");
    fault.store(true, Ordering::Relaxed);

    let keys_written = primary_thread.join().expect("primary thread panicked");
    replica_thread.join().expect("replica thread panicked");

    println!("\n==> Done. Primary wrote {} keys before fault. Replica took over and wrote 5 more.", keys_written);
    println!("    Warpdrive bucket '{}' was the replication channel throughout.\n", BUCKET);

    rm_local(PRIMARY_LOCAL);
    rm_local(REPLICA_LOCAL);
    Ok(())
}
