//! redis_bench <host:port> <hash> <concurrency> <duration_secs> <payload_bytes> [<user> <password>]
//!
//! The Redis counterpart of `s3_bench`: an HSET phase, then an HGET phase
//! over the keys it wrote, `concurrency` connections each with one command
//! in flight. It speaks plain RESP2, so it runs against any Redis-protocol
//! server (WarpDrive's front end, Redis, or another). AUTH is sent only when
//! credentials are given.
//!
//! `S3_BENCH_KEYS=n` cycles writes over n keys, as `s3_bench` does.
//! Output: `phase,payload,concurrency,count,errors,ops_per_s,mean_us,p50_us,p90_us,p99_us`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

struct Conn {
    r: BufReader<tokio::net::tcp::OwnedReadHalf>,
    w: tokio::net::tcp::OwnedWriteHalf,
}

impl Conn {
    async fn open(addr: &str, auth: Option<&(String, String)>) -> std::io::Result<Self> {
        let s = TcpStream::connect(addr).await?;
        s.set_nodelay(true)?;
        let (r, w) = s.into_split();
        let mut c = Conn { r: BufReader::with_capacity(256 * 1024, r), w };
        if let Some((u, p)) = auth {
            let reply = c.call(&[b"AUTH", u.as_bytes(), p.as_bytes()]).await?;
            if reply.is_err() {
                return Err(std::io::Error::other("AUTH refused"));
            }
        }
        Ok(c)
    }

    /// Sends one command and reads its reply: `Ok(bulk or None)`, or `Err(error line)`.
    async fn call(&mut self, args: &[&[u8]]) -> std::io::Result<Result<Option<Vec<u8>>, String>> {
        let mut out = format!("*{}\r\n", args.len()).into_bytes();
        for a in args {
            out.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
            out.extend_from_slice(a);
            out.extend_from_slice(b"\r\n");
        }
        self.w.write_all(&out).await?;
        let mut line = Vec::new();
        self.r.read_until(b'\n', &mut line).await?;
        if line.len() < 3 {
            return Err(std::io::Error::other("connection closed"));
        }
        let body = &line[1..line.len() - 2];
        Ok(match line[0] {
            b'$' => {
                let n: i64 = std::str::from_utf8(body).ok().and_then(|s| s.parse().ok()).unwrap_or(-1);
                if n < 0 {
                    Ok(None)
                } else {
                    let mut v = vec![0u8; n as usize + 2];
                    self.r.read_exact(&mut v).await?;
                    v.truncate(n as usize);
                    Ok(Some(v))
                }
            }
            b'-' => Err(String::from_utf8_lossy(body).into_owned()),
            _ => Ok(Some(body.to_vec())),
        })
    }
}

#[derive(Default)]
struct PhaseResult {
    latencies_us: Vec<u64>,
    errors: u64,
    error_sample: Option<String>,
}

fn pct(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() { return 0; }
    sorted[(((sorted.len() - 1) as f64) * p).round() as usize]
}

fn report(phase: &str, payload: usize, concurrency: usize, duration: Duration, mut r: PhaseResult) {
    r.latencies_us.sort_unstable();
    let n = r.latencies_us.len();
    let mean = if n == 0 { 0 } else { r.latencies_us.iter().sum::<u64>() / n as u64 };
    println!(
        "{phase},{payload},{concurrency},{n},{},{:.1},{mean},{},{},{}",
        r.errors,
        n as f64 / duration.as_secs_f64(),
        pct(&r.latencies_us, 0.50),
        pct(&r.latencies_us, 0.90),
        pct(&r.latencies_us, 0.99)
    );
    if let Some(s) = r.error_sample {
        eprintln!("{phase} error sample: {s}");
    }
}

#[allow(clippy::too_many_arguments)]
async fn phase(
    write: bool, addr: Arc<String>, auth: Arc<Option<(String, String)>>, hash: Arc<String>, concurrency: usize,
    duration: Duration, payload: Arc<Vec<u8>>, written: Arc<AtomicU64>, keys: Option<u64>,
) -> PhaseResult {
    let stop = Arc::new(AtomicBool::new(false));
    let next = Arc::new(AtomicU64::new(0));
    let mut tasks = Vec::new();
    for _ in 0..concurrency {
        let (addr, auth, hash, payload, written, stop, next) =
            (addr.clone(), auth.clone(), hash.clone(), payload.clone(), written.clone(), stop.clone(), next.clone());
        tasks.push(tokio::spawn(async move {
            let mut res = PhaseResult::default();
            let mut conn = match Conn::open(&addr, auth.as_ref().as_ref()).await {
                Ok(c) => c,
                Err(e) => {
                    res.errors += 1;
                    res.error_sample = Some(e.to_string());
                    return res;
                }
            };
            while !stop.load(Ordering::Relaxed) {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let key = if write {
                    format!("k{}", keys.map_or(i, |n| i % n))
                } else {
                    let w = written.load(Ordering::Relaxed).max(1);
                    format!("k{}", i % w)
                };
                let t0 = Instant::now();
                let r = if write {
                    conn.call(&[b"HSET", hash.as_bytes(), key.as_bytes(), &payload]).await
                } else {
                    conn.call(&[b"HGET", hash.as_bytes(), key.as_bytes()]).await
                };
                match r {
                    Ok(Ok(v)) if write || v.as_ref().is_some_and(|v| v.len() == payload.len()) => {
                        res.latencies_us.push(t0.elapsed().as_micros() as u64);
                    }
                    Ok(Ok(_)) => {
                        res.errors += 1;
                        res.error_sample.get_or_insert_with(|| "HGET returned the wrong length or nil".into());
                    }
                    Ok(Err(e)) => {
                        res.errors += 1;
                        res.error_sample.get_or_insert(e);
                    }
                    Err(e) => {
                        res.errors += 1;
                        res.error_sample.get_or_insert(e.to_string());
                        break;
                    }
                }
            }
            res
        }));
    }
    tokio::time::sleep(duration).await;
    stop.store(true, Ordering::Relaxed);
    let mut all = PhaseResult::default();
    for t in tasks {
        let r = t.await.unwrap();
        all.latencies_us.extend(r.latencies_us);
        all.errors += r.errors;
        if all.error_sample.is_none() {
            all.error_sample = r.error_sample;
        }
    }
    if write {
        let n = next.load(Ordering::Relaxed);
        written.store(keys.map_or(n, |k| n.min(k)), Ordering::Relaxed);
    }
    all
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 6 && args.len() != 8 {
        eprintln!("usage: redis_bench <host:port> <hash> <concurrency> <duration_secs> <payload_bytes> [<user> <password>]");
        std::process::exit(1);
    }
    let addr = Arc::new(args[1].clone());
    let hash = Arc::new(args[2].clone());
    let concurrency: usize = args[3].parse().unwrap();
    let duration = Duration::from_secs(args[4].parse().unwrap());
    let payload_bytes: usize = args[5].parse().unwrap();
    let auth = Arc::new((args.len() == 8).then(|| (args[6].clone(), args[7].clone())));
    let keys = std::env::var("S3_BENCH_KEYS").ok().and_then(|v| v.parse().ok()).filter(|n: &u64| *n > 0);
    let payload = Arc::new(vec![0xCDu8; payload_bytes]);
    let written = Arc::new(AtomicU64::new(0));

    let put = phase(true, addr.clone(), auth.clone(), hash.clone(), concurrency, duration, payload.clone(), written.clone(), keys).await;
    report("put", payload_bytes, concurrency, duration, put);
    let get = phase(false, addr, auth, hash, concurrency, duration, payload, written, keys).await;
    report("get", payload_bytes, concurrency, duration, get);
}
