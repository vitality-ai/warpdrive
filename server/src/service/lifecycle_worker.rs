//! Applies bucket lifecycle rules (`s3::lifecycle`): expires current
//! versions, removes expired delete markers and noncurrent versions, and
//! aborts incomplete multipart uploads.
//!
//! One node runs it: on a cluster the first peer in sorted order, so nodes
//! don't race each other to delete the same objects; on a single node, that
//! node. It runs hourly, or every second when lifecycle days are shortened
//! for tests (`WARPDRIVE_LIFECYCLE_DEBUG_DAY_SECS`).
//!
//! Every deletion goes through the same metadata calls a client's DELETE
//! does, so a versioned bucket gets a delete marker and, on a cluster, the
//! dead data is freed by gc/ rows like any other delete. Before expiring a
//! current version it re-reads the object and skips it if it changed since
//! the listing, and it never deletes a version under retention or legal hold.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::Utc;

use crate::metadata::sqlite_store::VersionRow;
use crate::s3::lifecycle::{self, day, expires_at, parse_time, Config};
use crate::service::metadata_service::{metadata_store, MetadataService};

/// Hourly in production. With shortened test days, a tenth of a day (at
/// least 100 ms), so an expiry lands well inside the next day.
fn tick() -> Duration {
    if lifecycle::debug_days() {
        (day().to_std().unwrap_or(Duration::from_secs(1)) / 10).max(Duration::from_millis(100))
    } else {
        Duration::from_secs(3600)
    }
}

/// Whether this node runs the expirer.
fn is_lifecycle_node() -> bool {
    match crate::cluster::bridge::state() {
        None => true,
        Some(state) => {
            let mut peers: Vec<String> = state.membership.peers().into_iter().map(|p| p.trim().trim_end_matches('/').to_string()).collect();
            peers.sort();
            match (peers.first(), crate::cluster::coordinator::self_addr()) {
                (Some(first), Some(me)) => first == me.trim().trim_end_matches('/'),
                // Not a configured cluster member: a single node.
                (None, _) | (_, None) => true,
            }
        }
    }
}

/// Starts the expirer thread.
pub fn start() {
    std::thread::Builder::new()
        .name("wd-lifecycle".into())
        .spawn(|| loop {
            std::thread::sleep(tick());
            if !is_lifecycle_node() {
                continue;
            }
            match metadata_store().list_lifecycle_buckets() {
                Ok(buckets) => {
                    for (bucket, owner, xml) in buckets {
                        match lifecycle::parse(&xml) {
                            Ok(cfg) => {
                                let n = apply(&owner, &bucket, &cfg);
                                if n > 0 {
                                    log::info!("lifecycle: bucket {bucket}: {n} actions");
                                }
                            }
                            Err(e) => log::warn!("lifecycle: bucket {bucket}: stored rules don't parse: {e:?}"),
                        }
                    }
                }
                Err(e) => log::warn!("lifecycle: listing buckets with rules failed: {e}"),
            }
        })
        .expect("failed to start the lifecycle thread");
}

/// Every version in a bucket, by key, newest first.
fn versions(db: &MetadataService, bucket: &str) -> BTreeMap<String, Vec<VersionRow>> {
    let mut out: BTreeMap<String, Vec<VersionRow>> = BTreeMap::new();
    let (mut km, mut vm) = (String::new(), String::new());
    loop {
        let Ok((rows, truncated, next_km, next_vm)) = db.list_object_versions_full(bucket, "", &km, &vm, 1000) else { break };
        for r in rows {
            out.entry(r.key.clone()).or_default().push(r);
        }
        if !truncated || (next_km == km && next_vm == vm) {
            break;
        }
        (km, vm) = (next_km, next_vm);
    }
    for vs in out.values_mut() {
        // Latest first, then by time, newest first.
        vs.sort_by(|a, b| b.is_latest.cmp(&a.is_latest).then_with(|| b.last_modified.cmp(&a.last_modified)));
    }
    out
}

fn locked(db: &MetadataService, bucket: &str, key: &str, vid: &str) -> bool {
    db.check_object_lock_protection(bucket, key, vid, false).map(|(r, h)| r || h).unwrap_or(true)
}

/// Applies `cfg` to one bucket. Returns how many actions it took.
pub fn apply(owner: &str, bucket: &str, cfg: &Config) -> usize {
    let Ok(db) = MetadataService::new(owner) else { return 0 };
    let rules: Vec<_> = cfg.rules.iter().filter(|r| r.enabled).collect();
    if rules.is_empty() {
        return 0;
    }
    let now = Utc::now();
    let versioned = !db.get_versioning_state(bucket).unwrap_or_default().eq_ignore_ascii_case("disabled");
    let needs_tags = rules.iter().any(|r| !r.filter.tags.is_empty());
    let mut actions = 0;

    for (key, vs) in versions(&db, bucket) {
        let Some(latest) = vs.first().filter(|v| v.is_latest) else { continue };
        let tags = if needs_tags { db.get_object_tags(bucket, &key).unwrap_or_default() } else { Vec::new() };
        let mut expired_current = false;
        for r in rules.iter().filter(|r| r.matches(&key, &tags, latest.size)) {
            // The current version.
            if let Some(e) = &r.expiration {
                if !expired_current && !latest.is_delete_marker {
                    let due = parse_time(&latest.last_modified).and_then(|c| expires_at(e, c)).is_some_and(|at| at <= now);
                    // Re-read: skip an object that changed since the listing.
                    let unchanged = db.get_object_full(bucket, &key).ok().and_then(|m| m.last_modified).is_some_and(|lm| lm == latest.last_modified);
                    if due && unchanged {
                        let done = if versioned { db.delete_object_v2(bucket, &key).is_ok() } else { db.delete_metadata(bucket, &key).is_ok() };
                        if done {
                            actions += 1;
                            expired_current = true;
                        }
                    }
                }
                // A delete marker with nothing behind it: removed when the
                // rule asks for it, or, as in S3, when an expiration by days
                // applies and the marker is that old.
                if latest.is_delete_marker && vs.len() == 1 {
                    let by_days = e.days.is_some_and(|d| parse_time(&latest.last_modified).is_some_and(|t| t + day() * d as i32 <= now));
                    if (e.expired_object_delete_marker == Some(true) || by_days) && db.delete_specific_version(bucket, &key, &latest.version_id).is_ok() {
                        actions += 1;
                    }
                }
            }
            // Noncurrent versions: noncurrent since the next newer version
            // was written. The newest `newer_noncurrent` are kept.
            if let Some(days) = r.noncurrent_days {
                let keep = r.newer_noncurrent.unwrap_or(0) as usize;
                for (i, v) in vs.iter().enumerate().skip(1) {
                    if i - 1 < keep {
                        continue;
                    }
                    let since = parse_time(&vs[i - 1].last_modified);
                    let due = since.is_some_and(|s| s + day() * days as i32 <= now);
                    if due && !locked(&db, bucket, &key, &v.version_id) && db.delete_specific_version(bucket, &key, &v.version_id).is_ok() {
                        actions += 1;
                    }
                }
            }
        }
    }

    // Incomplete multipart uploads.
    let abort: Vec<_> = rules.iter().filter_map(|r| r.abort_mpu_days.map(|d| (r, d))).collect();
    if !abort.is_empty() {
        for u in db.list_bucket_multipart_uploads(bucket).unwrap_or_default() {
            if u.status == "completed" {
                continue;
            }
            let Some(started) = parse_time(&u.initiated_at) else { continue };
            let due = abort.iter().any(|(r, d)| u.key.starts_with(&r.filter.prefix) && started + day() * *d as i32 <= now);
            if due && db.delete_parts_for_upload(&u.upload_id).is_ok() && db.delete_multipart_upload(&u.upload_id).is_ok() {
                actions += 1;
            }
        }
    }
    actions
}
