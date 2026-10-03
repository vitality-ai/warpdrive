//! Cluster membership: a shared peer list, seeded from WARPDRIVE_PEERS and
//! grown via additive registration (SeaweedFS/MinIO-pool-style: a new node
//! joins and immediately serves new writes; existing placements are never
//! recomputed — see docs/Distributed-Engine-Plan.md). No consensus, no
//! automatic rebalancing of already-placed data.

use std::env;
use std::sync::RwLock;

use super::coordinator::{JoinRequest, JoinResponse};

pub struct Membership {
    peers: RwLock<Vec<String>>,
}

/// Client side of the join protocol — previously missing entirely: the
/// `/cluster/join` *handler* existed, but nothing ever called it or merged
/// its response, so a new node never actually learned about the cluster
/// and the cluster never actually learned about it. Call this with a
/// bootstrap peer's address and this node's own address; it announces this
/// node to the bootstrap peer, merges the returned list, then announces
/// this node to every other peer in that list too — so convergence doesn't
/// depend on gossip among the existing nodes, just on the new node doing
/// the broadcasting itself.
pub async fn join_cluster_via(bootstrap: &str, self_addr: &str, membership: &Membership) {
    // Same reasoning as ClusterState::location_http (#162): no timeout at
    // all means a black-holed bootstrap peer hangs this join attempt
    // forever instead of failing it.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .connect_timeout(std::time::Duration::from_secs(3))
        .build()
        .expect("failed to build join client");

    let peers = match announce_self(&client, bootstrap, self_addr).await {
        Ok(peers) => peers,
        Err(e) => {
            log::error!("failed to join cluster via {bootstrap}: {e}");
            return;
        }
    };
    membership.merge(&peers);
    membership.add_peer(bootstrap);

    for peer in &peers {
        if peer == self_addr || peer == bootstrap {
            continue;
        }
        if let Err(e) = announce_self(&client, peer, self_addr).await {
            log::warn!("failed to announce self to {peer} during join: {e}");
        }
    }
}

async fn announce_self(client: &reqwest::Client, peer: &str, self_addr: &str) -> Result<Vec<String>, String> {
    let url = format!("{}/cluster/join", peer.trim_end_matches('/'));
    let resp = client
        .post(&url)
        .json(&JoinRequest { peer: self_addr.to_string() })
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let body: JoinResponse = resp.json().await.map_err(|e| e.to_string())?;
    Ok(body.peers)
}

impl Membership {
    /// Seed from WARPDRIVE_PEERS=http://node1:9710,http://node2:9710,...
    /// following the existing StorageConfig::from_env pattern.
    pub fn from_env() -> Self {
        let peers = env::var("WARPDRIVE_PEERS")
            .map(|v| {
                v.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        Self {
            peers: RwLock::new(peers),
        }
    }

    pub fn peers(&self) -> Vec<String> {
        self.peers.read().unwrap().clone()
    }

    /// Add a peer if not already present. Returns the full peer list after
    /// the add, which a `/cluster/join` handler hands back to the caller so
    /// it (and, via one broadcast round, every other known peer) converges
    /// on the same set.
    pub fn add_peer(&self, peer: &str) -> Vec<String> {
        let mut peers = self.peers.write().unwrap();
        if !peers.iter().any(|p| p == peer) {
            peers.push(peer.to_string());
        }
        peers.clone()
    }

    /// Merge a peer list received from another node (e.g. the response to
    /// a `/cluster/join` call) into this node's own list.
    pub fn merge(&self, incoming: &[String]) {
        let mut peers = self.peers.write().unwrap();
        for p in incoming {
            if !peers.iter().any(|existing| existing == p) {
                peers.push(p.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_peer_is_idempotent() {
        let m = Membership {
            peers: RwLock::new(vec!["http://node0:9710".to_string()]),
        };
        m.add_peer("http://node1:9710");
        m.add_peer("http://node1:9710");
        assert_eq!(m.peers().len(), 2);
    }

    #[test]
    fn merge_dedupes() {
        let m = Membership {
            peers: RwLock::new(vec!["http://node0:9710".to_string()]),
        };
        m.merge(&[
            "http://node0:9710".to_string(),
            "http://node1:9710".to_string(),
        ]);
        assert_eq!(m.peers().len(), 2);
    }
}
