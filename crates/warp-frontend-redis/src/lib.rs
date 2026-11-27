//! A Redis-protocol front end over WarpDrive's object service.
//!
//! It depends only on `warp-frontend`: the server hands it an
//! [`ObjectService`] and a [`RedisConfig`]. The server starts it when
//! `WARPDRIVE_REDIS_PORT` is set ([`RedisConfig::from_env`]). It speaks RESP2 and takes the
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
//! PING, QUIT and HELLO, checked by the object service. An open port
//! would bypass every S3 permission check. `WARPDRIVE_REDIS_AUTH=off` drops
//! that, for benchmarks against servers without auth, but only on a port
//! bound to loopback: the node won't start the front end on any other
//! address without auth.
//!
//! Not supported: expiry (EX/PX, EXPIRE, TTL, HSETEX, HEXPIRE), which needs a
//! per-key expiry WarpDrive doesn't keep, and RESP3.

mod resp;

use resp::{read_command, Reply};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncWriteExt, BufReader, BufWriter};
use tokio::net::{TcpListener, TcpStream};
use warp_frontend::{FrontEnd, ObjectError, ObjectService, Principal};

/// The largest value a connection that hasn't authenticated may send, so an
/// unauthenticated client can't make the server buffer a huge argument.
const PRE_AUTH_MAX_ARG: usize = 4096;
/// The largest value an authenticated client may send.
const MAX_ARG: usize = 512 * 1024 * 1024;

/// How the Redis front end listens and where plain keys go.
#[derive(Debug, Clone)]
pub struct RedisConfig {
    pub addr: SocketAddr,
    /// The bucket plain (non-hash) keys live in.
    pub default_bucket: String,
    /// Skip AUTH: connections run as `no_auth_as`. Allowed only on loopback.
    pub no_auth_as: Option<Principal>,
    /// Worker threads, each a single-threaded runtime accepting from the
    /// same listener.
    pub workers: usize,
}

impl RedisConfig {
    /// From `WARPDRIVE_REDIS_PORT` (unset: `None`, the front end is off),
    /// `WARPDRIVE_REDIS_BUCKET` (default `redis`), `WARPDRIVE_REDIS_AUTH=off`
    /// and `WARPDRIVE_REDIS_BIND_ALL`. A single node (`loopback_only`)
    /// listens on loopback unless `WARPDRIVE_REDIS_BIND_ALL` is set.
    /// `no_auth_user` is who connections run as with auth off.
    pub fn from_env(loopback_only: bool, no_auth_user: &str) -> Option<Self> {
        let port = std::env::var("WARPDRIVE_REDIS_PORT").ok().and_then(|p| p.trim().parse::<u16>().ok())?;
        let ip = if loopback_only && std::env::var("WARPDRIVE_REDIS_BIND_ALL").is_err() { "127.0.0.1" } else { "0.0.0.0" };
        let addr: SocketAddr = format!("{ip}:{port}").parse().expect("invalid Redis bind address");
        let no_auth = std::env::var("WARPDRIVE_REDIS_AUTH").is_ok_and(|v| v.trim().eq_ignore_ascii_case("off"));
        let default_bucket = std::env::var("WARPDRIVE_REDIS_BUCKET").ok().filter(|b| !b.trim().is_empty()).unwrap_or_else(|| "redis".into());
        Some(RedisConfig {
            addr,
            default_bucket,
            no_auth_as: no_auth.then(|| Principal { user_id: no_auth_user.to_string() }),
            workers: std::thread::available_parallelism().map_or(4, |n| n.get()),
        })
    }
}

/// The Redis front end, ready to start.
pub struct Redis {
    pub config: RedisConfig,
}

impl FrontEnd for Redis {
    fn name(&self) -> &str {
        "redis"
    }

    fn start(self: Box<Self>, objects: Arc<dyn ObjectService>) -> Result<(), String> {
        let cfg = Arc::new(self.config);
        let addr = cfg.addr;
        if cfg.no_auth_as.is_some() && !addr.ip().is_loopback() {
            return Err(format!("WARPDRIVE_REDIS_AUTH=off is allowed only on loopback, and this node would listen on {addr}"));
        }
        if cfg.no_auth_as.is_some() {
            log::warn!("Redis front end has no AUTH (WARPDRIVE_REDIS_AUTH=off): anyone on this machine can read and write every bucket through it");
        }
        let listener = std::net::TcpListener::bind(addr)
            .and_then(|l| l.set_nonblocking(true).map(|_| l))
            .map_err(|e| format!("can't listen on {addr}: {e}"))?;
        log::info!("Redis front end listening on {addr} with {} workers (plain keys in bucket '{}')", cfg.workers, cfg.default_bucket);
        for i in 0..cfg.workers {
            let Ok(l) = listener.try_clone() else { continue };
            let (cfg, objects) = (Arc::clone(&cfg), Arc::clone(&objects));
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
                                    let session = Session { objects: Arc::clone(&objects), cfg: Arc::clone(&cfg), who: cfg.no_auth_as.clone() };
                                    tokio::task::spawn_local(connection(stream, session));
                                }
                                Err(e) => log::warn!("Redis front end: accept failed: {e}"),
                            }
                        }
                    });
                })
                .map_err(|e| format!("spawn Redis worker: {e}"))?;
        }
        Ok(())
    }
}

/// One connection's state.
struct Session {
    objects: Arc<dyn ObjectService>,
    cfg: Arc<RedisConfig>,
    /// Who the connection runs as, once authenticated.
    who: Option<Principal>,
}

async fn connection(stream: TcpStream, mut s: Session) {
    let (r, w) = stream.into_split();
    let mut r = BufReader::with_capacity(128 * 1024, r);
    let mut w = BufWriter::with_capacity(128 * 1024, w);
    loop {
        let limit = if s.who.is_some() { MAX_ARG } else { PRE_AUTH_MAX_ARG };
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
        let reply = match run(&name, &args[1..], &mut s).await {
            Ok(r) | Err(r) => r,
        };
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

fn err(e: ObjectError) -> Reply {
    let msg = match e {
        ObjectError::NoSuchBucket(b) => format!("no such bucket '{b}' (create it over S3 first)"),
        other => other.to_string(),
    };
    Reply::Error(format!("ERR {}", msg.replace(['\r', '\n'], " ")))
}

async fn run(name: &str, args: &[Vec<u8>], s: &mut Session) -> Result<Reply, Reply> {
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
            s.who = s.objects.authenticate(user, pass);
            return if s.who.is_some() { Ok(Reply::ok()) } else { Err(Reply::Error("WRONGPASS invalid access key or secret key".into())) };
        }
        _ => {}
    }
    let Some(who) = s.who.clone() else {
        return Err(Reply::Error("NOAUTH Authentication required.".into()));
    };
    let o = &s.objects;
    let bucket = s.cfg.default_bucket.as_str();
    match name {
        "GET" => {
            arity(args, 1, name)?;
            Ok(o.get(&who, bucket, &text(&args[0])?).await.map_err(err)?.map_or(Reply::Nil, Reply::Bulk))
        }
        "SET" => {
            if args.len() != 2 {
                return Err(Reply::Error("ERR SET takes <key> <value> (expiry options aren't supported)".into()));
            }
            o.put(&who, bucket, &text(&args[0])?, args[1].clone()).await.map_err(err)?;
            Ok(Reply::ok())
        }
        "DEL" => {
            if args.is_empty() {
                return Err(Reply::Error("ERR wrong number of arguments for 'del' command".into()));
            }
            let mut n = 0;
            for k in args {
                n += o.delete(&who, bucket, &text(k)?).await.map_err(err)? as i64;
            }
            Ok(Reply::Int(n))
        }
        "EXISTS" => {
            if args.is_empty() {
                return Err(Reply::Error("ERR wrong number of arguments for 'exists' command".into()));
            }
            let mut n = 0;
            for k in args {
                n += o.exists(&who, bucket, &text(k)?).map_err(err)? as i64;
            }
            Ok(Reply::Int(n))
        }
        "HGET" => {
            arity(args, 2, name)?;
            Ok(o.get(&who, &text(&args[0])?, &text(&args[1])?).await.map_err(err)?.map_or(Reply::Nil, Reply::Bulk))
        }
        "HSET" => {
            if args.len() < 3 || args.len() % 2 == 0 {
                return Err(Reply::Error("ERR wrong number of arguments for 'hset' command".into()));
            }
            let h = text(&args[0])?;
            let mut added = 0;
            for pair in args[1..].chunks(2) {
                let field = text(&pair[0])?;
                // HSET replies with how many fields are new.
                let existed = o.exists(&who, &h, &field).map_err(err)?;
                o.put(&who, &h, &field, pair[1].clone()).await.map_err(err)?;
                added += !existed as i64;
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
                n += o.delete(&who, &h, &text(f)?).await.map_err(err)? as i64;
            }
            Ok(Reply::Int(n))
        }
        "HEXISTS" => {
            arity(args, 2, name)?;
            Ok(Reply::Int(o.exists(&who, &text(&args[0])?, &text(&args[1])?).map_err(err)? as i64))
        }
        _ => Err(Reply::Error(format!("ERR unknown command '{}'", name.to_lowercase()))),
    }
}
