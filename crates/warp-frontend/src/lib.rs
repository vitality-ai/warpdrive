//! The seam between WarpDrive's object layer and its front ends.
//!
//! WarpDrive has three layers:
//!
//! - Front ends: protocols (Redis today, S3 next, others later), each in its
//!   own crate, depending on this crate only.
//! - The object layer: buckets, keys, versions, ETags and checksums. The
//!   server implements [`ObjectService`] with it.
//! - The engine: moves bytes (erasure-coded blobs) and keeps replicated rows
//!   ordered and durable, with no knowledge of objects.
//!
//! The server starts each configured [`FrontEnd`] with the object layer. A
//! front end can be added, removed or tested against a fake service without
//! touching the layers below, and they can change without touching it.
//!
//! Every value written through here is an ordinary object: what one front
//! end writes, the others read.

use async_trait::async_trait;
use std::fmt;
use std::sync::Arc;

/// Why an object call failed, in terms a front end can map to its protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectError {
    /// The bucket doesn't exist. Front ends don't create buckets.
    NoSuchBucket(String),
    /// The key is empty or otherwise not a valid object key.
    InvalidKey(String),
    /// Anything else, already logged by the service.
    Internal(String),
}

impl fmt::Display for ObjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ObjectError::NoSuchBucket(b) => write!(f, "no such bucket '{b}'"),
            ObjectError::InvalidKey(m) => write!(f, "invalid key: {m}"),
            ObjectError::Internal(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for ObjectError {}

pub type ObjectResult<T> = Result<T, ObjectError>;

/// Who a request runs as, once a front end has authenticated it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub user_id: String,
}

/// What the engine offers every front end: authenticate a client, then put,
/// get, delete and look up whole objects. Every write is durable when its
/// call returns.
///
/// The futures need not be `Send`: the server's object calls run on
/// single-threaded runtimes, so a front end drives them on one (a
/// `LocalSet`, as the Redis front end does).
#[async_trait(?Send)]
pub trait ObjectService: Send + Sync {
    /// Checks an access key and secret. `None` means the credentials are wrong.
    fn authenticate(&self, access_key: &[u8], secret: &[u8]) -> Option<Principal>;

    /// Writes `value` as `bucket/key`, replacing any current object.
    async fn put(&self, who: &Principal, bucket: &str, key: &str, value: Vec<u8>) -> ObjectResult<()>;

    /// The object's bytes, or `None` if there is no such object.
    async fn get(&self, who: &Principal, bucket: &str, key: &str) -> ObjectResult<Option<Vec<u8>>>;

    /// Deletes the object (a delete marker on a versioned bucket). Returns
    /// whether it existed.
    async fn delete(&self, who: &Principal, bucket: &str, key: &str) -> ObjectResult<bool>;

    /// Whether the object exists.
    fn exists(&self, who: &Principal, bucket: &str, key: &str) -> ObjectResult<bool>;
}

/// A protocol served over an [`ObjectService`].
pub trait FrontEnd: Send {
    /// A short name for logs ("redis", "s3").
    fn name(&self) -> &str;

    /// Starts serving. It returns once the front end is listening, and runs
    /// on its own threads or tasks from then on.
    fn start(self: Box<Self>, objects: Arc<dyn ObjectService>) -> Result<(), String>;
}
