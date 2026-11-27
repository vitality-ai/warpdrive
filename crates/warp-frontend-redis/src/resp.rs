//! RESP2: reading commands and writing replies.
//!
//! A command is an array of bulk strings (`*2\r\n$3\r\nGET\r\n$1\r\nk\r\n`),
//! which is what every client library sends, or an inline line
//! (`GET k\r\n`) as typed into `redis-cli` or `nc`.

use std::io::{Error, ErrorKind};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const MAX_LINE: usize = 64 * 1024;
const MAX_ARGS: usize = 1024 * 1024;

fn bad(msg: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidData, msg.into())
}

/// One CRLF-terminated line, without the CRLF. `None` at a clean EOF.
async fn line<R: AsyncBufRead + Unpin>(r: &mut R) -> Result<Option<Vec<u8>>, Error> {
    let mut buf = Vec::new();
    let n = (&mut *r).take(MAX_LINE as u64 + 2).read_until(b'\n', &mut buf).await?;
    if n == 0 {
        return Ok(None);
    }
    if !buf.ends_with(b"\r\n") {
        return Err(bad(if buf.len() > MAX_LINE { "line too long" } else { "expected CRLF" }));
    }
    buf.truncate(buf.len() - 2);
    Ok(Some(buf))
}

fn number(b: &[u8]) -> Result<i64, Error> {
    std::str::from_utf8(b).ok().and_then(|s| s.parse().ok()).ok_or_else(|| bad("invalid length"))
}

/// Reads one command. `None` at a clean EOF between commands. Bulk strings
/// longer than `max_arg` are refused.
pub async fn read_command<R: AsyncBufRead + Unpin>(r: &mut R, max_arg: usize) -> Result<Option<Vec<Vec<u8>>>, Error> {
    let Some(first) = line(r).await? else { return Ok(None) };
    let Some(count) = first.strip_prefix(b"*") else {
        // Inline command.
        return Ok(Some(first.split(|c| c.is_ascii_whitespace()).filter(|s| !s.is_empty()).map(<[u8]>::to_vec).collect()));
    };
    let count = number(count)?;
    if count <= 0 {
        return Ok(Some(Vec::new()));
    }
    if count as usize > MAX_ARGS {
        return Err(bad("too many arguments"));
    }
    let mut args = Vec::with_capacity((count as usize).min(64));
    for _ in 0..count {
        let header = line(r).await?.ok_or_else(|| bad("unexpected end of command"))?;
        let len = number(header.strip_prefix(b"$").ok_or_else(|| bad("expected '$'"))?)?;
        if len < 0 || len as usize > max_arg {
            return Err(bad("invalid bulk length"));
        }
        let mut arg = vec![0u8; len as usize + 2];
        r.read_exact(&mut arg).await?;
        if !arg.ends_with(b"\r\n") {
            return Err(bad("expected CRLF after bulk string"));
        }
        arg.truncate(len as usize);
        args.push(arg);
    }
    Ok(Some(args))
}

pub enum Reply {
    Simple(String),
    Error(String),
    Int(i64),
    Bulk(Vec<u8>),
    Nil,
    Array(Vec<Reply>),
}

impl Reply {
    pub fn ok() -> Self {
        Reply::Simple("OK".into())
    }

    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Reply::Simple(s) => out.extend_from_slice(format!("+{s}\r\n").as_bytes()),
            // An error line can't hold a newline.
            Reply::Error(s) => out.extend_from_slice(format!("-{}\r\n", s.replace(['\r', '\n'], " ")).as_bytes()),
            Reply::Int(n) => out.extend_from_slice(format!(":{n}\r\n").as_bytes()),
            Reply::Bulk(b) => {
                out.extend_from_slice(format!("${}\r\n", b.len()).as_bytes());
                out.extend_from_slice(b);
                out.extend_from_slice(b"\r\n");
            }
            Reply::Nil => out.extend_from_slice(b"$-1\r\n"),
            Reply::Array(items) => {
                out.extend_from_slice(format!("*{}\r\n", items.len()).as_bytes());
                for i in items {
                    i.encode(out);
                }
            }
        }
    }

    pub async fn write<W: AsyncWrite + Unpin>(&self, w: &mut W) -> Result<(), Error> {
        if let Reply::Bulk(b) = self {
            // Large values skip the extra copy.
            w.write_all(format!("${}\r\n", b.len()).as_bytes()).await?;
            w.write_all(b).await?;
            return w.write_all(b"\r\n").await;
        }
        let mut out = Vec::new();
        self.encode(&mut out);
        w.write_all(&out).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn parse(input: &[u8], max: usize) -> Result<Option<Vec<Vec<u8>>>, Error> {
        let mut r = tokio::io::BufReader::new(input);
        read_command(&mut r, max).await
    }

    #[tokio::test]
    async fn reads_array_and_inline_commands() {
        let a = parse(b"*3\r\n$4\r\nHSET\r\n$1\r\nh\r\n$5\r\na\r\nbc\r\n", 100).await.unwrap().unwrap();
        assert_eq!(a, vec![b"HSET".to_vec(), b"h".to_vec(), b"a\r\nbc".to_vec()], "binary-safe values");
        let i = parse(b"GET  key\r\n", 100).await.unwrap().unwrap();
        assert_eq!(i, vec![b"GET".to_vec(), b"key".to_vec()]);
        assert!(parse(b"", 100).await.unwrap().is_none(), "clean EOF");
    }

    #[tokio::test]
    async fn refuses_malformed_and_oversized_input() {
        assert!(parse(b"*1\r\n$10\r\nabc\r\n", 100).await.is_err(), "short bulk");
        assert!(parse(b"*1\r\n$3\r\nabcXY", 100).await.is_err(), "no CRLF after bulk");
        assert!(parse(b"*1\r\n$200\r\n", 100).await.is_err(), "over the size limit");
        assert!(parse(b"*1\r\n:3\r\n", 100).await.is_err(), "not a bulk string");
        assert!(parse(b"*x\r\n", 100).await.is_err(), "bad count");
    }

    #[test]
    fn encodes_replies() {
        let mut out = Vec::new();
        Reply::Array(vec![Reply::ok(), Reply::Int(3), Reply::Nil, Reply::Bulk(b"v".to_vec()), Reply::Error("a\nb".into())]).encode(&mut out);
        assert_eq!(out, b"*5\r\n+OK\r\n:3\r\n$-1\r\n$1\r\nv\r\n-a b\r\n");
    }
}
