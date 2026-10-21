//! A single append-only log file plus everything its callers need to make
//! writes durable without paying one fsync per write -- bundled once here
//! instead of being wired up separately in every Bitcask-style store that
//! needs it (`location_store.rs`, `content_location_store.rs`,
//! `shard_meta_store.rs` were each repeating the same "dup a sync-only
//! file descriptor, batch fsyncs via `GroupCommit`, refresh both after
//! `compact()` replaces the file" wiring before this existed).
//!
//! Callers keep their own `RwLock<()>`-style write-ordering lock (the
//! "#155" invariant that the log and an in-memory index can never apply a
//! concurrent put/delete in different orders) -- that's specific to each
//! store's own index type, so it isn't pulled in here. This type owns
//! only the file-and-durability half of the problem.

use crate::storage::group_commit::GroupCommit;
use parking_lot::RwLock;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

pub struct DurableAppendLog {
    file: RwLock<File>,
    /// A separate, duplicated descriptor to the same inode, used only for
    /// `sync()` calls -- never for appends. Kept apart from `file` so a
    /// group-commit leader's (possibly slow) sync never has to contend
    /// with the lock callers need to append quickly and move on; see
    /// `append_line`'s doc for why that split matters.
    sync_handle: RwLock<File>,
    group_commit: GroupCommit,
}

impl DurableAppendLog {
    /// Opens (creating if needed) `path` for appending.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let sync_handle = file.try_clone()?;
        Ok(Self {
            file: RwLock::new(file),
            sync_handle: RwLock::new(sync_handle),
            group_commit: GroupCommit::new(),
        })
    }

    /// Appends `line` and returns a group-commit ticket for it. Cheap --
    /// `writeln!` plus `flush()` (a no-op on a raw `File`) -- and
    /// deliberately does *not* sync. Callers hold their own ordering lock
    /// across this call and their own index update, same as before this
    /// type existed; they call `sync_ticket` only *after* releasing that
    /// lock, so one caller's slow fsync never blocks another's append.
    pub fn append_line(&self, line: &str) -> io::Result<u64> {
        let mut f = self.file.write();
        writeln!(f, "{line}")?;
        f.flush()?;
        Ok(self.group_commit.ticket())
    }

    /// Blocks until `ticket`'s append is durable on stable storage,
    /// batched via `GroupCommit` across whichever other callers are
    /// concurrently waiting -- see that module's doc for why this is
    /// correct, not just fast.
    pub fn sync_ticket(&self, ticket: u64) -> io::Result<()> {
        let handle = self.sync_handle.read();
        self.group_commit.wait_for_durable(ticket, &*handle)
    }

    /// For `compact()`: makes `tmp_path`'s content durable, atomically
    /// renames it over `log_path`, then reopens both this log's write
    /// handle and its sync handle against the new inode -- otherwise
    /// `sync_handle` would keep fsyncing an orphaned, unlinked file
    /// nobody appends to anymore. Callers do the actual log rewrite
    /// themselves (every store's record shape is different); this just
    /// takes over once the new content is written to `tmp_path`.
    pub fn replace_with_compacted(&self, tmp_path: &Path, log_path: &Path) -> io::Result<()> {
        {
            let tmp = OpenOptions::new().write(true).open(tmp_path)?;
            // Makes the compacted content durable before the rename below
            // makes it live -- otherwise a crash between the rename and
            // this file's next unrelated writeback could leave `log_path`
            // pointing at a file missing data a confirmed-successful
            // `compact()` call claimed it had written.
            tmp.sync_all()?;
        }
        // Atomic on the same filesystem (same directory as log_path, not
        // /tmp): a crash here leaves either the old log or the new one
        // fully intact, never a half-written file.
        std::fs::rename(tmp_path, log_path)?;
        let mut file = self.file.write();
        *file = OpenOptions::new().create(true).append(true).open(log_path)?;
        *self.sync_handle.write() = file.try_clone()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};

    #[test]
    fn append_then_sync_round_trips_and_is_readable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        let log = DurableAppendLog::open(&path).unwrap();
        let ticket = log.append_line("hello").unwrap();
        log.sync_ticket(ticket).unwrap();

        let f = File::open(&path).unwrap();
        let lines: Vec<String> = BufReader::new(f).lines().collect::<io::Result<_>>().unwrap();
        assert_eq!(lines, vec!["hello".to_string()]);
    }

    #[test]
    fn replace_with_compacted_swaps_content_and_further_appends_still_work() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("log");
        let log = DurableAppendLog::open(&log_path).unwrap();
        let t1 = log.append_line("stale-entry-1").unwrap();
        log.sync_ticket(t1).unwrap();
        let t2 = log.append_line("stale-entry-2").unwrap();
        log.sync_ticket(t2).unwrap();

        let tmp_path = dir.path().join("log.compact.tmp");
        std::fs::write(&tmp_path, "compacted-entry\n").unwrap();
        log.replace_with_compacted(&tmp_path, &log_path).unwrap();

        let f = File::open(&log_path).unwrap();
        let lines: Vec<String> = BufReader::new(f).lines().collect::<io::Result<_>>().unwrap();
        assert_eq!(lines, vec!["compacted-entry".to_string()]);

        // The log handle and its sync handle must both point at the new
        // inode -- a further append must land in the post-compaction
        // file, and syncing it must not error out against a stale handle.
        let t3 = log.append_line("post-compact-entry").unwrap();
        log.sync_ticket(t3).unwrap();
        let f = File::open(&log_path).unwrap();
        let lines: Vec<String> = BufReader::new(f).lines().collect::<io::Result<_>>().unwrap();
        assert_eq!(lines, vec!["compacted-entry".to_string(), "post-compact-entry".to_string()]);
    }
}
