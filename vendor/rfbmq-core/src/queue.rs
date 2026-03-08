//! Queue operations: enqueue, dequeue, complete, fail, reap, purge.
//!
//! # Concurrency model
//!
//! `Queue` is `Send + Sync` because all fields are immutable after construction
//! (except via explicit `set_*` methods which require `&mut self`). Concurrent
//! access safety is provided by filesystem atomicity: `rename(2)` is atomic on
//! POSIX systems, so multiple processes/threads can safely push and pop without
//! application-level locking. The `scan_and_claim` loop handles rename races by
//! retrying on `ENOENT`/`EEXIST`.
//!
//! # Known limitation: `fail()` atomicity gap
//!
//! `fail()` writes the updated message to its destination (pending or failed)
//! and then deletes the original from processing. If the process crashes between
//! the write and the delete, the same message will exist in both directories.
//! The `reap()` function detects these orphans (messages in processing whose ID
//! already exists in pending or failed) and removes the duplicate. Always run
//! `reap()` as part of crash recovery.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;

use crate::error::{Error, Result};
use crate::fs_utils::{
    collect_md_files, count_md_files, durable_rename, durable_write_rename, fsync_dir,
};
use crate::message::{parse_file, parse_headers};
use crate::types::{ClaimedMessage, FsyncMode, Message, MessageId, Priority, Queue, SCAN_RETRIES};

impl Queue {
    pub fn init(root: &Path, use_priority: bool, max_pending: i64) -> Result<Self> {
        if root.join("pending").exists() || root.join("processing").exists() {
            return Err(Error::QueueExists(root.to_path_buf()));
        }

        let q = Queue::new(root.to_path_buf(), use_priority, max_pending);

        crate::fs_utils::create_dir_with_mode(root, q.dir_mode())?;

        if use_priority {
            for p in &Priority::ALL {
                crate::fs_utils::create_dir_with_mode(
                    &root.join("pending").join(p.dir_prefix()),
                    q.dir_mode(),
                )?;
            }
        } else {
            crate::fs_utils::create_dir_with_mode(&root.join("pending"), q.dir_mode())?;
        }

        crate::fs_utils::create_dir_with_mode(&root.join("processing"), q.dir_mode())?;
        crate::fs_utils::create_dir_with_mode(&root.join("done"), q.dir_mode())?;
        crate::fs_utils::create_dir_with_mode(&root.join("failed"), q.dir_mode())?;
        crate::fs_utils::create_dir_with_mode(&root.join(".tmp"), q.dir_mode())?;

        let meta_dir = root.join(".meta");
        crate::fs_utils::create_dir_with_mode(&meta_dir, q.dir_mode())?;
        fs::write(meta_dir.join("max_pending"), max_pending.to_string())?;

        Ok(q)
    }

    pub fn open(root: &Path) -> Result<Self> {
        if !root.join("pending").exists() || !root.join("processing").exists() {
            return Err(Error::QueueNotFound(root.to_path_buf()));
        }

        let use_priority = root.join("pending").join("0-critical").exists();

        let max_pending = {
            let meta_path = root.join(".meta").join("max_pending");
            if meta_path.exists() {
                let s = fs::read_to_string(&meta_path)?;
                s.trim()
                    .parse::<i64>()
                    .map_err(|_| Error::Parse("invalid max_pending".into()))?
            } else {
                crate::types::DEFAULT_MAX_PENDING
            }
        };

        Ok(Queue::new(root.to_path_buf(), use_priority, max_pending))
    }

    pub fn enqueue(&self, msg: &mut Message) -> Result<MessageId> {
        let id = MessageId::generate();
        let now = Utc::now();

        msg.header.id = Some(id.clone());
        msg.header.created_at = now.format("%Y-%m-%dT%H:%M:%S%.9fZ").to_string();
        if msg.header.created_by.is_empty() {
            msg.header.created_by = format!(
                "{}@{}",
                std::process::id(),
                gethostname::gethostname().to_string_lossy()
            );
        }

        // Check max_pending soft limit
        if self.max_pending() > 0 {
            let current = self.count_pending()?;
            if current as i64 >= self.max_pending() {
                return Err(Error::MaxPendingExceeded(self.max_pending()));
            }
        }

        let buf = msg.serialize()?;

        let pending_name = format!("{}.{}.md", format_ts(now), id);

        let pending_dir = self.pending_dir_for(msg.header.priority);
        let pending_path = pending_dir.join(&pending_name);

        durable_write_rename(
            &self.root().join(".tmp"),
            &format!("{}.md", id),
            &pending_path,
            buf.as_bytes(),
            self.file_mode(),
            self.fsync_mode(),
        )?;

        Ok(id)
    }

    pub fn dequeue(&self) -> Result<Option<ClaimedMessage>> {
        if self.use_priority_dirs() {
            for p in &Priority::ALL {
                let dir = self.root().join("pending").join(p.dir_prefix());
                if let Some(claimed) = self.scan_and_claim(&dir)? {
                    return Ok(Some(claimed));
                }
            }
        } else {
            let dir = self.root().join("pending");
            if let Some(claimed) = self.scan_and_claim(&dir)? {
                return Ok(Some(claimed));
            }
        }
        Ok(None)
    }

    fn scan_and_claim(&self, pending_dir: &Path) -> Result<Option<ClaimedMessage>> {
        let processing_dir = self.root().join("processing");

        for _retry in 0..SCAN_RETRIES {
            let mut names = collect_md_files(pending_dir)?;
            if names.is_empty() {
                return Ok(None);
            }
            names.sort();

            for name in &names {
                let src = pending_dir.join(name);
                let id = match extract_id_from_pending(name) {
                    Some(id) => id,
                    None => continue,
                };

                let claim_name = format!("{}.{}.md", format_ts(Utc::now()), id);
                let dst = processing_dir.join(&claim_name);

                match fs::rename(&src, &dst) {
                    Ok(()) => {
                        fsync_dir(&processing_dir, self.fsync_mode())?;
                        fsync_dir(pending_dir, self.fsync_mode())?;
                        return Ok(Some(ClaimedMessage::from_path(dst)?));
                    }
                    Err(e)
                        if e.kind() == std::io::ErrorKind::NotFound
                            || e.kind() == std::io::ErrorKind::AlreadyExists =>
                    {
                        // Race: another consumer claimed it or dst collision
                        continue;
                    }
                    Err(e) => return Err(Error::Io(e)),
                }
            }
        }

        Ok(None)
    }

    pub fn complete(&self, claimed: &ClaimedMessage) -> Result<()> {
        let done_path = self.root().join("done").join(format!("{}.md", claimed.id()));
        durable_rename(claimed.path(), &done_path, self.fsync_mode())?;
        Ok(())
    }

    pub fn fail(&self, claimed: &ClaimedMessage) -> Result<()> {
        let mut msg = parse_file(claimed.path())?;
        msg.header.retry_count += 1;

        let id = claimed.id().clone();
        let buf = msg.serialize()?;

        if msg.header.retry_count > self.max_retries() {
            let failed_path = self.root().join("failed").join(format!("{}.md", id));
            durable_write_rename(
                &self.root().join(".tmp"),
                &format!("nack-{}.md", id),
                &failed_path,
                buf.as_bytes(),
                self.file_mode(),
                self.fsync_mode(),
            )?;
        } else {
            let enqueue_ts = created_at_to_filename_ts(&msg.header.created_at)?;
            let pending_name = format!("{}.{}.md", enqueue_ts, id);
            let pending_dir = self.pending_dir_for(msg.header.priority);
            let pending_path = pending_dir.join(&pending_name);
            durable_write_rename(
                &self.root().join(".tmp"),
                &format!("nack-{}.md", id),
                &pending_path,
                buf.as_bytes(),
                self.file_mode(),
                self.fsync_mode(),
            )?;
        }

        // Remove the claimed file from processing. If this fails (e.g. process
        // crash), reap() will detect the orphan and clean it up.
        fs::remove_file(claimed.path())?;
        if let Some(dir) = claimed.path().parent() {
            fsync_dir(dir, self.fsync_mode())?;
        }

        Ok(())
    }

    pub fn reap(&self) -> Result<u32> {
        let mut count = 0u32;
        let now = Utc::now().timestamp();
        let processing_dir = self.root().join("processing");

        let names = collect_md_files(&processing_dir)?;

        // Build a set of IDs present in pending/ to avoid O(n×m) rescanning
        let pending_ids = self.collect_pending_ids()?;

        for name in &names {
            let claim_ts = match extract_ts_from_filename(name) {
                Some(ts) => ts,
                None => continue,
            };

            if (now - claim_ts) <= self.lease_timeout() as i64 {
                continue;
            }

            let id = match extract_id_from_pending(name) {
                Some(id) => id,
                None => continue,
            };

            // Check if orphan: ID already exists in pending/ or failed/
            let is_orphan =
                pending_ids.contains(&id) || self.id_exists_in_dir("failed", &id)?;

            let claimed_path = processing_dir.join(name);
            if is_orphan {
                let _ = fs::remove_file(&claimed_path);
                count += 1;
            } else {
                let claimed = ClaimedMessage::from_path(&claimed_path)?;
                match self.fail(&claimed) {
                    Ok(()) => count += 1,
                    Err(_) => continue,
                }
            }
        }

        // Clean .tmp orphans older than lease_timeout
        let tmp_dir = self.root().join(".tmp");
        if tmp_dir.exists() {
            for entry in fs::read_dir(&tmp_dir)? {
                let entry = entry?;
                let metadata = entry.metadata()?;
                if let Ok(modified) = metadata.modified() {
                    let age = modified.elapsed().unwrap_or_default().as_secs();
                    if age > self.lease_timeout() as u64 {
                        let _ = fs::remove_file(entry.path());
                        count += 1;
                    }
                }
            }
        }

        Ok(count)
    }

    pub fn reap_ttl(&self) -> Result<u32> {
        let mut count = 0u32;
        let now = Utc::now();

        let pending_dirs = self.all_pending_dirs();
        for dir in &pending_dirs {
            let names = collect_md_files(dir)?;
            for name in &names {
                let path = dir.join(name);
                let msg = match parse_headers(&path) {
                    Ok(m) => m,
                    Err(_) => continue,
                };

                if msg.header.ttl == 0 {
                    continue;
                }

                let created = match parse_created_at(&msg.header.created_at) {
                    Some(dt) => dt,
                    None => continue,
                };

                let elapsed = (now - created).num_seconds();
                if elapsed > msg.header.ttl as i64 {
                    let id = msg.header.id.expect("parsed message must have ID");
                    let failed_path = self.root().join("failed").join(format!("{}.md", id));
                    match durable_rename(&path, &failed_path, self.fsync_mode()) {
                        Ok(()) => count += 1,
                        Err(_) => continue,
                    }
                }
            }
        }

        Ok(count)
    }

    pub fn purge(&self, max_age_seconds: u32) -> Result<u32> {
        let mut count = 0u32;
        let done_dir = self.root().join("done");

        if !done_dir.exists() {
            return Ok(0);
        }

        for entry in fs::read_dir(&done_dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".md") {
                continue;
            }
            let metadata = entry.metadata()?;
            if let Ok(modified) = metadata.modified() {
                let age = modified.elapsed().unwrap_or_default().as_secs();
                if age > max_age_seconds as u64 {
                    fs::remove_file(entry.path())?;
                    count += 1;
                }
            }
        }

        Ok(count)
    }

    pub fn sync(&self) -> Result<()> {
        if self.fsync_mode() != FsyncMode::Batch {
            return Ok(());
        }

        for dir in &self.all_pending_dirs() {
            if dir.exists() {
                crate::fs_utils::fsync_dir_force(dir)?;
            }
        }

        let dirs = ["processing", "done", "failed", ".tmp"];
        for d in &dirs {
            let p = self.root().join(d);
            if p.exists() {
                crate::fs_utils::fsync_dir_force(&p)?;
            }
        }

        Ok(())
    }

    pub fn depth(&self) -> Result<i64> {
        let mut total = self.count_pending()? as i64;
        total += count_md_files(&self.root().join("processing"))? as i64;
        Ok(total)
    }

    pub fn list_ready(&self) -> Result<Vec<MessageId>> {
        let mut ready = Vec::new();
        let pending_dirs = self.all_pending_dirs();

        // Build set of done IDs once to avoid O(n×d) filesystem syscalls
        let done_ids: HashSet<String> = collect_md_files(&self.root().join("done"))?
            .into_iter()
            .filter_map(|name| name.strip_suffix(".md").map(|s| s.to_string()))
            .collect();

        for dir in &pending_dirs {
            let names = collect_md_files(dir)?;
            for name in &names {
                let path = dir.join(name);
                let msg = match parse_headers(&path) {
                    Ok(m) => m,
                    Err(_) => continue,
                };

                if msg.header.depends_on.is_empty() {
                    if let Some(id) = msg.header.id {
                        ready.push(id);
                    }
                    continue;
                }

                let all_done = msg
                    .header
                    .depends_on
                    .iter()
                    .all(|dep_id| done_ids.contains(dep_id.as_str()));

                if all_done {
                    if let Some(id) = msg.header.id {
                        ready.push(id);
                    }
                }
            }
        }

        Ok(ready)
    }

    // ── helpers ──────────────────────────────────────────────────────────

    fn pending_dir_for(&self, priority: Priority) -> PathBuf {
        if self.use_priority_dirs() {
            self.root().join("pending").join(priority.dir_prefix())
        } else {
            self.root().join("pending")
        }
    }

    fn all_pending_dirs(&self) -> Vec<PathBuf> {
        if self.use_priority_dirs() {
            Priority::ALL
                .iter()
                .map(|p| self.root().join("pending").join(p.dir_prefix()))
                .collect()
        } else {
            vec![self.root().join("pending")]
        }
    }

    fn count_pending(&self) -> Result<usize> {
        let mut total: usize = 0;
        for dir in &self.all_pending_dirs() {
            total += count_md_files(dir)?;
        }
        Ok(total)
    }

    fn collect_pending_ids(&self) -> Result<HashSet<MessageId>> {
        let mut ids = HashSet::new();
        for dir in &self.all_pending_dirs() {
            for name in &collect_md_files(dir)? {
                if let Some(id) = extract_id_from_pending(name) {
                    ids.insert(id);
                }
            }
        }
        Ok(ids)
    }

    fn id_exists_in_dir(&self, subdir: &str, id: &MessageId) -> Result<bool> {
        let path = self.root().join(subdir).join(format!("{}.md", id));
        Ok(path.exists())
    }
}

/// Format a chrono DateTime as a zero-padded filename timestamp: `<10-digit-seconds><9-digit-nanos>`
fn format_ts(dt: chrono::DateTime<Utc>) -> String {
    format!("{:010}{:09}", dt.timestamp(), dt.timestamp_subsec_nanos())
}

/// Extract the ID from a pending/processing filename: `<ts>.<id>.md` → id
fn extract_id_from_pending(filename: &str) -> Option<MessageId> {
    let stem = filename.strip_suffix(".md")?;
    let dot_pos = stem.find('.')?;
    let id_str = &stem[dot_pos + 1..];
    id_str.parse().ok()
}

/// Extract seconds timestamp from filename: `<seconds_padded><9ns>.<id>.md`
///
/// Expects the timestamp portion to be exactly 19 digits (10 seconds + 9 nanos).
fn extract_ts_from_filename(filename: &str) -> Option<i64> {
    let stem = filename.strip_suffix(".md")?;
    let dot_pos = stem.find('.')?;
    let ts_part = &stem[..dot_pos];
    if ts_part.len() < 19 {
        return None;
    }
    let seconds_str = &ts_part[..ts_part.len() - 9];
    seconds_str.parse::<i64>().ok()
}

/// Convert Created-At ISO 8601 string to filename timestamp format (zero-padded)
fn created_at_to_filename_ts(created_at: &str) -> Result<String> {
    let dt = parse_created_at(created_at).ok_or_else(|| {
        Error::Parse(format!("cannot parse Created-At: {}", created_at))
    })?;
    Ok(format_ts(dt))
}

/// Parse a Created-At timestamp, trying RFC 3339 first, then the nanosecond format.
fn parse_created_at(created_at: &str) -> Option<chrono::DateTime<Utc>> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(created_at) {
        return Some(dt.with_timezone(&Utc));
    }
    if let Ok(ndt) =
        chrono::NaiveDateTime::parse_from_str(created_at, "%Y-%m-%dT%H:%M:%S%.fZ")
    {
        return Some(ndt.and_utc());
    }
    None
}
