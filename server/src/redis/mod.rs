//! A Redis-protocol front end over the same object store as S3.
//!
//! Off unless `WARPDRIVE_REDIS_PORT` is set. It speaks RESP2 and takes the
//! common blob-store data commands (GET, SET, DEL, EXISTS, HGET, HSET, HDEL,
//! HEXISTS), plus PING, QUIT, HELLO and AUTH.
//!
//! Mapping: a hash is a bucket and its fields are object keys, so
//! `HSET prefs user42 <json>` writes object `user42` in bucket `prefs`. Plain
//! keys go to one bucket, `WARPDRIVE_REDIS_BUCKET` (default `redis`). Values
//! are ordinary S3 objects: what Redis writes, S3 reads, and the other way
//! round. Buckets are created over S3 first. Redis can't create them.
//!
//! A connection must `AUTH <access key> <secret key>` before anything but
//! PING, QUIT and HELLO, checked against the admin credentials. An open port
//! would bypass every S3 permission check. `WARPDRIVE_REDIS_AUTH=off` drops
//! that, for benchmarks against servers without auth, but only on a port
//! bound to loopback: the node won't start the front end on any other
//! address without auth.
//!
//! Not supported: expiry (EX/PX, EXPIRE, TTL, HSETEX, HEXPIRE), which needs a
//! per-key expiry WarpDrive doesn't keep, and RESP3.

mod resp;
mod store;

use resp::{read_command, Reply};
use std::net::SocketAddr;
use tokio::io::{AsyncWriteExt, BufReader, BufWriter};
use tokio::net::{TcpListener, TcpStream};

/// The largest value a connection that hasn't authenticated may send, so an
/// unauthenticated client can't make the server buffer a huge argument.
const PRE_AUTH_MAX_ARG: usize = 4096;
/// The largest value an authenticated client may send.
const MAX_ARG: usize = 512 * 1024 * 1024;

/// Starts the front end if `WARPDRIVE_REDIS_PORT` is set. A single node
/// (`loopback_only`) listens on loopback.
///
/// The store's async calls run on single-threaded runtimes, as actix's
/// workers do, so this runs one such worker thread per core, all accepting
/// from the same listener.
pub fn serve_if_enabled(loopback_only: bool) {
    let Some(port) = std::env::var("WARPDRIVE_REDIS_PORT").ok().and_then(|p| p.trim().parse::<u16>().ok()) else {
        return;
    };
    let ip = if loopback_only && std::env::var("WARPDRIVE_REDIS_BIND_ALL").is_err() { "127.0.0.1" } else { "0.0.0.0" };
    let addr: SocketAddr = format!("{ip}:{port}").parse().expect("invalid Redis bind address");
    let no_auth = std::env::var("WARPDRIVE_REDIS_AUTH").is_ok_and(|v| v.trim().eq_ignore_ascii_case("off"));
    if no_auth && !addr.ip().is_loopback() {
        log::error!("Redis front end not started: WARPDRIVE_REDIS_AUTH=off is allowed only on loopback, and this node would listen on {addr}");
        return;
    }
    if no_auth {
        log::warn!("Redis front end has no AUTH (WARPDRIVE_REDIS_AUTH=off): anyone on this machine can read and write every bucket through it");
    }
    let listener = match std::net::TcpListener::bind(addr).and_then(|l| l.set_nonblocking(true).map(|_| l)) {
        Ok(l) => l,
        Err(e) => {
            log::error!("Redis front end: can't listen on {addr}: {e}");
            return;
        }
    };
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get());
    log::info!("Redis front end listening on {addr} with {workers} workers (plain keys in bucket '{}')", store::default_bucket());
    for i in 0..workers {
        let Ok(l) = listener.try_clone() else { continue };
        std::thread::Builder::new()
            .name(format!("wd-redis-{i}"))
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("Redis worker runtime");
                let local = tokio::task::LocalSet::new();
                local.block_on(&rt, async move {
                    let listener = TcpListener::from_std(l).expect("Redis listener");
                    loop {
                        match listener.accept().await {
                            Ok((stream, _)) => {
                                let _ = stream.set_nodelay(true);
                                tokio::task::spawn_local(connection(stream, no_auth));
                            }
                            Err(e) => log::warn!("Redis front end: accept failed: {e}"),
                        }
                    }
                });
            })
            .expect("spawn Redis worker");
    }
}

async fn connection(stream: TcpStream, no_auth: bool) {
    let (r, w) = stream.into_split();
    let mut r = BufReader::with_capacity(128 * 1024, r);
    let mut w = BufWriter::with_capacity(128 * 1024, w);
    let mut authed = no_auth;
    loop {
        let limit = if authed { MAX_ARG } else { PRE_AUTH_MAX_ARG };
        let args = match read_command(&mut r, limit).await {
            Ok(Some(a)) => a,
            Ok(None) => return,
            Err(e) => {
                let _ = Reply::Error(format!("ERR Protocol error: {e}")).write(&mut w).await;
                let _ = w.flush().await;
                return;
            }
        };
        if args.is_empty() {
            continue;
        }
        let name = String::from_utf8_lossy(&args[0]).to_ascii_uppercase();
        let quit = name == "QUIT";
        let reply = execute(&name, &args[1..], &mut authed).await;
        if reply.write(&mut w).await.is_err() {
            return;
        }
        // Flush once the client has nothing more queued, so pipelined
        // commands share one write.
        if (r.buffer().is_empty() || quit) && w.flush().await.is_err() {
            return;
        }
        if quit {
            return;
        }
    }
}

fn arity(args: &[Vec<u8>], n: usize, name: &str) -> Result<(), Reply> {
    if args.len() == n { Ok(()) } else { Err(Reply::Error(format!("ERR wrong number of arguments for '{}' command", name.to_lowercase()))) }
}

fn text(b: &[u8]) -> Result<String, Reply> {
    String::from_utf8(b.to_vec()).map_err(|_| Reply::Error("ERR keys and hash names must be UTF-8".into()))
}

async fn execute(name: &str, args: &[Vec<u8>], authed: &mut bool) -> Reply {
    match run(name, args, authed).await {
        Ok(r) | Err(r) => r,
    }
}

async fn run(name: &str, args: &[Vec<u8>], authed: &mut bool) -> Result<Reply, Reply> {
    match name {
        "PING" => return Ok(match args.first() { Some(m) => Reply::Bulk(m.clone()), None => Reply::Simple("PONG".into()) }),
        "QUIT" => return Ok(Reply::ok()),
        "HELLO" => {
            // RESP2 only. HELLO with no version, or 2, gets the server info.
            if args.first().is_some_and(|v| v.as_slice() != b"2") {
                return Err(Reply::Error("NOPROTO this server only speaks RESP2".into()));
            }
            return Ok(Reply::Array(vec![
                Reply::Bulk(b"server".to_vec()),
                Reply::Bulk(b"warpdrive".to_vec()),
                Reply::Bulk(b"proto".to_vec()),
                Reply::Int(2),
            ]));
        }
        "AUTH" => {
            let (user, pass) = match args {
                [u, p] => (u.as_slice(), p.as_slice()),
                _ => return Err(Reply::Error("ERR AUTH takes <access key> <secret key>".into())),
            };
            *authed = store::credentials_match(user, pass);
            return if *authed { Ok(Reply::ok()) } else { Err(Reply::Error("WRONGPASS invalid access key or secret key".into())) };
        }
        _ => {}
    }
    if !*authed {
        return Err(Reply::Error("NOAUTH Authentication required.".into()));
    }
    let bucket = store::default_bucket();
    match name {
        "GET" => {
            arity(args, 1, name)?;
            Ok(store::get(&bucket, &text(&args[0])?).await?.map_or(Reply::Nil, Reply::Bulk))
        }
        "SET" => {
            if args.len() != 2 {
                return Err(Reply::Error("ERR SET takes <key> <value> (expiry options aren't supported)".into()));
            }
            store::put(&bucket, &text(&args[0])?, args[1].clone(), false).await?;
            Ok(Reply::ok())
        }
        "DEL" => {
            if args.is_empty() {
                return Err(Reply::Error("ERR wrong number of arguments for 'del' command".into()));
            }
            let mut n = 0;
            for k in args {
                n += store::delete(&bucket, &text(k)?).await? as i64;
            }
            Ok(Reply::Int(n))
        }
        "EXISTS" => {
            if args.is_empty() {
                return Err(Reply::Error("ERR wrong number of arguments for 'exists' command".into()));
            }
            let mut n = 0;
            for k in args {
                n += store::exists(&bucket, &text(k)?)? as i64;
            }
            Ok(Reply::Int(n))
        }
        "HGET" => {
            arity(args, 2, name)?;
            Ok(store::get(&text(&args[0])?, &text(&args[1])?).await?.map_or(Reply::Nil, Reply::Bulk))
        }
        "HSET" => {
            if args.len() < 3 || args.len() % 2 == 0 {
                return Err(Reply::Error("ERR wrong number of arguments for 'hset' command".into()));
            }
            let h = text(&args[0])?;
            let mut added = 0;
            for pair in args[1..].chunks(2) {
                added += store::put(&h, &text(&pair[0])?, pair[1].clone(), true).await? as i64;
            }
            Ok(Reply::Int(added))
        }
        "HDEL" => {
            if args.len() < 2 {
                return Err(Reply::Error("ERR wrong number of arguments for 'hdel' command".into()));
            }
            let h = text(&args[0])?;
            let mut n = 0;
            for f in &args[1..] {
                n += store::delete(&h, &text(f)?).await? as i64;
            }
            Ok(Reply::Int(n))
        }
        "HEXISTS" => {
            arity(args, 2, name)?;
            Ok(Reply::Int(store::exists(&text(&args[0])?, &text(&args[1])?)? as i64))
        }
        _ => Err(Reply::Error(format!("ERR unknown command '{}'", name.to_lowercase()))),
    }
}
