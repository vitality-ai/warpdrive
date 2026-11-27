//! The Redis front end against an in-memory object service: no server, no
//! engine. Shows the front end depends on the `warp-frontend` seam only.

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use warp_frontend::{FrontEnd, ObjectError, ObjectResult, ObjectService, Principal};
use warp_frontend_redis::{Redis, RedisConfig};

#[derive(Default)]
struct Fake {
    buckets: Vec<String>,
    objects: Mutex<HashMap<(String, String), Vec<u8>>>,
}

impl Fake {
    fn check(&self, who: &Principal, bucket: &str) -> ObjectResult<()> {
        assert_eq!(who.user_id, "alice", "calls run as the authenticated principal");
        if self.buckets.iter().any(|b| b == bucket) { Ok(()) } else { Err(ObjectError::NoSuchBucket(bucket.into())) }
    }
}

#[async_trait(?Send)]
impl ObjectService for Fake {
    fn authenticate(&self, a: &[u8], s: &[u8]) -> Option<Principal> {
        (a == b"ak" && s == b"sk").then(|| Principal { user_id: "alice".into() })
    }
    async fn put(&self, who: &Principal, b: &str, k: &str, v: Vec<u8>) -> ObjectResult<()> {
        self.check(who, b)?;
        self.objects.lock().unwrap().insert((b.into(), k.into()), v);
        Ok(())
    }
    async fn get(&self, who: &Principal, b: &str, k: &str) -> ObjectResult<Option<Vec<u8>>> {
        self.check(who, b)?;
        Ok(self.objects.lock().unwrap().get(&(b.into(), k.into())).cloned())
    }
    async fn delete(&self, who: &Principal, b: &str, k: &str) -> ObjectResult<bool> {
        self.check(who, b)?;
        Ok(self.objects.lock().unwrap().remove(&(b.into(), k.into())).is_some())
    }
    fn exists(&self, who: &Principal, b: &str, k: &str) -> ObjectResult<bool> {
        self.check(who, b)?;
        Ok(self.objects.lock().unwrap().contains_key(&(b.into(), k.into())))
    }
}

fn cmd(args: &[&[u8]]) -> Vec<u8> {
    let mut out = format!("*{}\r\n", args.len()).into_bytes();
    for a in args {
        out.extend(format!("${}\r\n", a.len()).bytes());
        out.extend_from_slice(a);
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// Sends one command and reads until a full reply is in (replies here are short).
async fn send(c: &mut TcpStream, args: &[&[u8]]) -> String {
    c.write_all(&cmd(args)).await.unwrap();
    let mut buf = vec![0u8; 4096];
    let mut got = Vec::new();
    loop {
        let n = c.read(&mut buf).await.unwrap();
        got.extend_from_slice(&buf[..n]);
        let s = String::from_utf8_lossy(&got).to_string();
        let complete = match s.as_bytes().first() {
            Some(b'$') if !s.starts_with("$-1") => s.matches("\r\n").count() >= 2,
            _ => s.ends_with("\r\n"),
        };
        if complete {
            return s;
        }
    }
}

#[tokio::test]
async fn redis_commands_go_through_the_object_service() {
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let fake = Arc::new(Fake { buckets: vec!["redis".into(), "prefs".into()], ..Default::default() });
    let config = RedisConfig { addr: format!("127.0.0.1:{port}").parse().unwrap(), default_bucket: "redis".into(), no_auth_as: None, workers: 1 };
    Box::new(Redis { config }).start(fake.clone()).unwrap();

    let mut c = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    assert_eq!(send(&mut c, &[b"PING"]).await, "+PONG\r\n");
    assert!(send(&mut c, &[b"GET", b"k"]).await.starts_with("-NOAUTH"));
    assert!(send(&mut c, &[b"AUTH", b"ak", b"wrong"]).await.starts_with("-WRONGPASS"));
    assert_eq!(send(&mut c, &[b"AUTH", b"ak", b"sk"]).await, "+OK\r\n");

    assert_eq!(send(&mut c, &[b"SET", b"k", b"v1"]).await, "+OK\r\n");
    assert_eq!(send(&mut c, &[b"GET", b"k"]).await, "$2\r\nv1\r\n");
    assert_eq!(fake.objects.lock().unwrap().get(&("redis".into(), "k".into())).unwrap(), b"v1");
    assert_eq!(send(&mut c, &[b"EXISTS", b"k", b"nope"]).await, ":1\r\n");
    assert_eq!(send(&mut c, &[b"DEL", b"k"]).await, ":1\r\n");
    assert_eq!(send(&mut c, &[b"GET", b"k"]).await, "$-1\r\n");

    assert_eq!(send(&mut c, &[b"HSET", b"prefs", b"u1", b"a", b"u2", b"b"]).await, ":2\r\n");
    assert_eq!(send(&mut c, &[b"HSET", b"prefs", b"u1", b"c"]).await, ":0\r\n", "an existing field isn't new");
    assert_eq!(send(&mut c, &[b"HGET", b"prefs", b"u1"]).await, "$1\r\nc\r\n");
    assert_eq!(send(&mut c, &[b"HEXISTS", b"prefs", b"u2"]).await, ":1\r\n");
    assert_eq!(send(&mut c, &[b"HDEL", b"prefs", b"u2", b"u9"]).await, ":1\r\n");
    assert!(send(&mut c, &[b"HGET", b"nobucket", b"x"]).await.starts_with("-ERR no such bucket 'nobucket'"));
}

#[test]
fn no_auth_is_refused_off_loopback() {
    let config = RedisConfig {
        addr: "0.0.0.0:0".parse().unwrap(),
        default_bucket: "redis".into(),
        no_auth_as: Some(Principal { user_id: "alice".into() }),
        workers: 1,
    };
    let err = Box::new(Redis { config }).start(Arc::new(Fake::default())).unwrap_err();
    assert!(err.contains("only on loopback"), "{err}");
}
