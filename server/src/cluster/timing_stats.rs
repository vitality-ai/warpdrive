//! Diagnostic phase-level timing for `cluster_put_object`, added
//! specifically to answer "where are we slow if we're not saturating
//! anywhere" — a GCP load test showed flat throughput at low CPU
//! utilization even as client concurrency increased 4x, the classic
//! signature of a concurrency limit somewhere in the pipeline rather than
//! a compute bottleneck. Atomic counters, not per-request logging, so
//! collecting this data doesn't itself perturb the measurement.

use std::time::Duration;

pub use crate::phase_timing::Phase;

#[derive(Default)]
pub struct TimingStats {
    pub encode: Phase,
    pub shard_fanout: Phase,
    pub location_replicate: Phase,
    pub total: Phase,
    /// Row writes and blob shard writes, each a round to every peer.
    pub row: Round,
    pub shard: Round,
}

/// One kind of write round to the peers: how long until a write quorum had
/// answered, how long until the last peer had, and each peer's own time.
#[derive(Default)]
pub struct Round {
    pub quorum: Phase,
    pub all: Phase,
    peers: parking_lot::Mutex<std::collections::BTreeMap<String, std::sync::Arc<Phase>>>,
}

impl Round {
    /// Records one round from each peer's `(peer, ok, time)`. `required`
    /// successes make the quorum.
    pub fn record(&self, replies: &[(&str, bool, Duration)], required: usize) {
        let mut ok: Vec<Duration> = replies.iter().filter(|r| r.1).map(|r| r.2).collect();
        ok.sort();
        if required > 0 && ok.len() >= required {
            self.quorum.record(ok[required - 1]);
        }
        if let Some(last) = replies.iter().map(|r| r.2).max() {
            self.all.record(last);
        }
        let mut peers = self.peers.lock();
        for (peer, _, d) in replies {
            match peers.get(*peer) {
                Some(p) => p.record(*d),
                None => peers.entry(peer.to_string()).or_default().record(*d),
            }
        }
    }

    fn summary(&self, name: &str) -> String {
        let mut s = format!(
            "{name}_n={} {name}_quorum_avg_ms={:.3} {name}_all_avg_ms={:.3}\n",
            self.all.count(),
            self.quorum.avg_ms(),
            self.all.avg_ms()
        );
        for (peer, p) in self.peers.lock().iter() {
            s.push_str(&format!("{name}_peer {peer} avg_ms={:.3} max_ms={:.3}\n", p.avg_ms(), p.max_ms()));
        }
        s
    }
}

impl TimingStats {
    pub fn summary(&self) -> String {
        format!(
            "n={}\n\
             encode_avg_ms={:.3} encode_max_ms={:.3}\n\
             shard_fanout_avg_ms={:.3} shard_fanout_max_ms={:.3}\n\
             location_replicate_avg_ms={:.3} location_replicate_max_ms={:.3}\n\
             total_avg_ms={:.3} total_max_ms={:.3}\n",
            self.total.count(),
            self.encode.avg_ms(),
            self.encode.max_ms(),
            self.shard_fanout.avg_ms(),
            self.shard_fanout.max_ms(),
            self.location_replicate.avg_ms(),
            self.location_replicate.max_ms(),
            self.total.avg_ms(),
            self.total.max_ms(),
        ) + &self.row.summary("row")
            + &self.shard.summary("shard")
    }
}
