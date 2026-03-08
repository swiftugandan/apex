use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use filetime::{set_file_mtime, FileTime};
use rfbmq_core::{
    parse_file, parse_headers, Error, FsyncMode, Message, MessageId, Priority, Queue,
    ID_LEN, VERSION,
};
use tempfile::TempDir;

// ── helpers ──────────────────────────────────────────────────────────────

fn make_queue() -> (TempDir, Queue) {
    let dir = TempDir::new().unwrap();
    let mut q = Queue::init(dir.path(), false, 0).unwrap();
    q.set_fsync_mode(FsyncMode::None);
    (dir, q)
}

fn make_priority_queue() -> (TempDir, Queue) {
    let dir = TempDir::new().unwrap();
    let mut q = Queue::init(dir.path(), true, 0).unwrap();
    q.set_fsync_mode(FsyncMode::None);
    (dir, q)
}

fn push_msg(q: &Queue, body: &str) -> MessageId {
    let mut msg = Message::default();
    msg.body = body.to_string();
    q.enqueue(&mut msg).unwrap()
}

fn push_msg_with(q: &Queue, body: &str, f: impl FnOnce(&mut Message)) -> MessageId {
    let mut msg = Message::default();
    msg.body = body.to_string();
    f(&mut msg);
    q.enqueue(&mut msg).unwrap()
}

fn count_files(dir: &Path) -> usize {
    if !dir.exists() {
        return 0;
    }
    fs::read_dir(dir)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".md")
        })
        .count()
}

fn find_msg(dir: &Path, id: &MessageId) -> Option<PathBuf> {
    if !dir.exists() {
        return None;
    }
    let id_str = id.as_str();
    for entry in fs::read_dir(dir).ok()? {
        let entry = entry.ok()?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == format!("{}.md", id_str) || name.ends_with(&format!(".{}.md", id_str)) {
            return Some(entry.path());
        }
    }
    None
}

fn is_hex(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_hexdigit())
}

// ── 1. init ──────────────────────────────────────────────────────────────

#[test]
fn test_init() {
    let (dir, _q) = make_queue();
    let r = dir.path();
    assert!(r.join("pending").exists());
    assert!(r.join("processing").exists());
    assert!(r.join("done").exists());
    assert!(r.join("failed").exists());
    assert!(r.join(".tmp").exists());
    assert!(r.join(".meta").exists());
}

#[test]
fn test_init_priority() {
    let (dir, _q) = make_priority_queue();
    let r = dir.path();
    assert!(r.join("pending/0-critical").exists());
    assert!(r.join("pending/1-high").exists());
    assert!(r.join("pending/2-normal").exists());
    assert!(r.join("pending/3-low").exists());
}

#[test]
fn test_init_already_exists() {
    let (dir, _q) = make_queue();
    let err = Queue::init(dir.path(), false, 0);
    assert!(matches!(err, Err(Error::QueueExists(_))));
}

// ── 2. push ──────────────────────────────────────────────────────────────

#[test]
fn test_push() {
    let (_dir, q) = make_queue();
    let id = push_msg(&q, "# Hello World");

    assert_eq!(id.as_str().len(), ID_LEN);
    assert!(is_hex(id.as_str()));

    let f = find_msg(&q.root().join("pending"), &id).expect("file in pending");
    let msg = parse_file(&f).unwrap();
    assert_eq!(msg.header.id.as_ref().unwrap(), &id);
    assert_eq!(msg.header.priority, Priority::Normal);
    assert!(msg.body.contains("# Hello World"));

    let name = f.file_name().unwrap().to_string_lossy();
    let parts: Vec<&str> = name.strip_suffix(".md").unwrap().split('.').collect();
    assert_eq!(parts.len(), 2); // <ts>.<id>
}

#[test]
fn test_push_with_options() {
    let (_dir, q) = make_queue();
    let id = push_msg_with(&q, "# Urgent deploy", |m| {
        m.header.priority = Priority::High;
        m.header.ttl = 3600;
        m.header.correlation_id = Some("req-42".into());
        m.header.tags = vec!["urgent".into(), "deploy".into()];
        m.header.created_by = "ci-server".into();
    });

    let f = find_msg(&q.root().join("pending"), &id).unwrap();
    let msg = parse_file(&f).unwrap();
    assert_eq!(msg.header.priority, Priority::High);
    assert_eq!(msg.header.ttl, 3600);
    assert_eq!(msg.header.correlation_id.as_deref(), Some("req-42"));
    assert!(msg.header.tags.contains(&"urgent".to_string()));
    assert!(msg.header.tags.contains(&"deploy".to_string()));
    assert_eq!(msg.header.created_by, "ci-server");
}

// ── 3. depth ─────────────────────────────────────────────────────────────

#[test]
fn test_depth() {
    let (_dir, q) = make_queue();
    push_msg(&q, "A");
    push_msg(&q, "B");
    push_msg(&q, "C");
    assert_eq!(q.depth().unwrap(), 3);
}

// ── 4. parse ─────────────────────────────────────────────────────────────

#[test]
fn test_parse_file() {
    let (_dir, q) = make_queue();
    let id = push_msg(&q, "The body");
    let f = find_msg(&q.root().join("pending"), &id).unwrap();
    let msg = parse_file(&f).unwrap();
    assert_eq!(msg.header.id.as_ref().unwrap(), &id);
    assert_eq!(msg.body, "The body");
}

#[test]
fn test_parse_headers_only() {
    let (_dir, q) = make_queue();
    let id = push_msg(&q, "Body text");
    let f = find_msg(&q.root().join("pending"), &id).unwrap();
    let msg = parse_headers(&f).unwrap();
    assert_eq!(msg.header.id.as_ref().unwrap(), &id);
    assert!(msg.body.is_empty());
}

// ── 5. pop ───────────────────────────────────────────────────────────────

#[test]
fn test_pop() {
    let (_dir, q) = make_queue();
    push_msg(&q, "M");
    let claimed = q.dequeue().unwrap().expect("should pop");
    assert!(claimed.path().exists());
    assert!(claimed.path().to_string_lossy().contains("processing"));

    let name = claimed.path().file_name().unwrap().to_string_lossy();
    let parts: Vec<&str> = name.strip_suffix(".md").unwrap().split('.').collect();
    assert_eq!(parts.len(), 2);
}

#[test]
fn test_depth_includes_processing() {
    let (_dir, q) = make_queue();
    push_msg(&q, "A");
    push_msg(&q, "B");
    push_msg(&q, "C");
    let _ = q.dequeue().unwrap();
    assert_eq!(q.depth().unwrap(), 3);
}

// ── 6. ack ───────────────────────────────────────────────────────────────

#[test]
fn test_ack() {
    let (_dir, q) = make_queue();
    let id = push_msg(&q, "M");
    let claimed = q.dequeue().unwrap().unwrap();
    q.complete(&claimed).unwrap();

    assert!(!claimed.path().exists());
    assert_eq!(count_files(&q.root().join("done")), 1);

    let done_file = q.root().join("done").join(format!("{}.md", id));
    assert!(done_file.exists());
}

// ── 7. nack ──────────────────────────────────────────────────────────────

#[test]
fn test_nack_retry() {
    let (_dir, q) = make_queue();
    let id = push_msg(&q, "M");
    let claimed = q.dequeue().unwrap().unwrap();
    q.fail(&claimed).unwrap();

    assert!(!claimed.path().exists());
    assert_eq!(q.depth().unwrap(), 1);

    let f = find_msg(&q.root().join("pending"), &id).unwrap();
    let msg = parse_file(&f).unwrap();
    assert_eq!(msg.header.retry_count, 1);
}

#[test]
fn test_nack_dead_letter() {
    let (_dir, q) = make_queue();
    let id = push_msg(&q, "M");

    for _ in 0..=q.max_retries() {
        let claimed = q.dequeue().unwrap().unwrap();
        q.fail(&claimed).unwrap();
    }

    assert!(count_files(&q.root().join("failed")) >= 1);
    let failed = q.root().join("failed").join(format!("{}.md", id));
    assert!(failed.exists());
}

// ── 8. pop empty ─────────────────────────────────────────────────────────

#[test]
fn test_pop_empty() {
    let (_dir, q) = make_queue();
    assert!(q.dequeue().unwrap().is_none());
}

// ── 9. no-fsync mode ────────────────────────────────────────────────────

#[test]
fn test_no_fsync() {
    let dir = TempDir::new().unwrap();
    let mut q = Queue::init(dir.path(), false, 0).unwrap();
    q.set_fsync_mode(FsyncMode::None);
    let id = push_msg(&q, "no sync");
    assert_eq!(id.as_str().len(), ID_LEN);
    assert!(q.dequeue().unwrap().is_some());
}

// ── 10. purge ────────────────────────────────────────────────────────────

#[test]
fn test_purge() {
    let (_dir, q) = make_queue();
    let id = push_msg(&q, "M");
    let claimed = q.dequeue().unwrap().unwrap();
    q.complete(&claimed).unwrap();

    let done_file = q.root().join("done").join(format!("{}.md", id));
    set_file_mtime(&done_file, FileTime::from_unix_time(1_000_000, 0)).unwrap();

    let purged = q.purge(1).unwrap();
    assert_eq!(purged, 1);
    assert_eq!(count_files(&q.root().join("done")), 0);
}

// ── 11. priority pop order ──────────────────────────────────────────────

#[test]
fn test_priority_pop_order() {
    let (_dir, q) = make_priority_queue();

    let _low = push_msg_with(&q, "Low", |m| m.header.priority = Priority::Low);
    let crit = push_msg_with(&q, "Critical", |m| m.header.priority = Priority::Critical);
    let _norm = push_msg_with(&q, "Normal", |m| m.header.priority = Priority::Normal);

    let claimed = q.dequeue().unwrap().unwrap();
    let msg = parse_file(claimed.path()).unwrap();
    assert_eq!(msg.header.id.as_ref().unwrap(), &crit);
}

// ── 12. FIFO order ──────────────────────────────────────────────────────

#[test]
fn test_fifo_order_3() {
    let (_dir, q) = make_queue();
    let a = push_msg(&q, "A");
    thread::sleep(std::time::Duration::from_millis(10));
    let b = push_msg(&q, "B");
    thread::sleep(std::time::Duration::from_millis(10));
    let c = push_msg(&q, "C");

    let mut popped = Vec::new();
    for _ in 0..3 {
        let claimed = q.dequeue().unwrap().unwrap();
        let msg = parse_file(claimed.path()).unwrap();
        popped.push(msg.header.id.unwrap());
        q.complete(&claimed).unwrap();
    }
    assert_eq!(popped, vec![a, b, c]);
}

#[test]
fn test_fifo_order_12() {
    let (_dir, q) = make_queue();
    let mut ids = Vec::new();
    for i in 0..12 {
        ids.push(push_msg(&q, &format!("Msg {}", i)));
        thread::sleep(std::time::Duration::from_millis(5));
    }

    let mut popped = Vec::new();
    for _ in 0..12 {
        let claimed = q.dequeue().unwrap().unwrap();
        let msg = parse_file(claimed.path()).unwrap();
        popped.push(msg.header.id.unwrap());
        q.complete(&claimed).unwrap();
    }
    assert_eq!(popped, ids);
}

// ── 13. version constant ────────────────────────────────────────────────

#[test]
fn test_version_constant() {
    assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
}

// ── 14. concurrent push ─────────────────────────────────────────────────

#[test]
fn test_concurrent_push_20() {
    let dir = TempDir::new().unwrap();
    let mut q = Queue::init(dir.path(), false, 0).unwrap();
    q.set_fsync_mode(FsyncMode::None);
    let q = Arc::new(q);

    let handles: Vec<_> = (0..20)
        .map(|i| {
            let q = Arc::clone(&q);
            thread::spawn(move || push_msg(&q, &format!("msg {}", i)))
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(q.depth().unwrap(), 20);
}

#[test]
fn test_concurrent_push_50() {
    let dir = TempDir::new().unwrap();
    let mut q = Queue::init(dir.path(), false, 0).unwrap();
    q.set_fsync_mode(FsyncMode::None);
    let q = Arc::new(q);

    let handles: Vec<_> = (0..50)
        .map(|i| {
            let q = Arc::clone(&q);
            thread::spawn(move || push_msg(&q, &format!("msg {}", i)))
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(q.depth().unwrap(), 50);
    assert_eq!(count_files(&q.root().join("pending")), 50);
}

// ── 15. parser edge cases ───────────────────────────────────────────────

#[test]
fn test_parser_empty_body() {
    let (_dir, q) = make_queue();
    let id = push_msg(&q, "");
    let f = find_msg(&q.root().join("pending"), &id).unwrap();
    let msg = parse_file(&f).unwrap();
    assert!(msg.body.is_empty());
}

#[test]
fn test_parser_colon_in_body() {
    let (_dir, q) = make_queue();
    let id = push_msg(&q, "Key: Value in body\nAnother: line");
    let f = find_msg(&q.root().join("pending"), &id).unwrap();
    let msg = parse_file(&f).unwrap();
    assert!(msg.body.contains("Key: Value in body"));
    assert!(msg.body.contains("Another: line"));
}

// ── 16. batch fsync ─────────────────────────────────────────────────────

#[test]
fn test_batch_fsync() {
    let dir = TempDir::new().unwrap();
    let mut q = Queue::init(dir.path(), false, 0).unwrap();
    q.set_fsync_mode(FsyncMode::Batch);

    let id = push_msg(&q, "batch msg");
    assert_eq!(id.as_str().len(), ID_LEN);
    assert!(find_msg(&q.root().join("pending"), &id).is_some());
    q.sync().unwrap();

    let claimed = q.dequeue().unwrap().unwrap();
    q.complete(&claimed).unwrap();
}

// ── 17. meta max_pending ─────────────────────────────────────────────────

#[test]
fn test_meta_max_pending() {
    let dir = TempDir::new().unwrap();
    Queue::init(dir.path(), false, 500).unwrap();

    assert!(dir.path().join(".meta").exists());
    let content = fs::read_to_string(dir.path().join(".meta/max_pending")).unwrap();
    assert_eq!(content.trim(), "500");

    let q = Queue::open(dir.path()).unwrap();
    assert_eq!(q.max_pending(), 500);
}

// ── 18. depth error ─────────────────────────────────────────────────────

#[test]
fn test_depth_error_nonexistent() {
    let err = Queue::open(Path::new("/nonexistent/path/to/queue"));
    assert!(matches!(err, Err(Error::QueueNotFound(_))));
}

#[test]
fn test_open_nonexistent() {
    let dir = TempDir::new().unwrap();
    let err = Queue::open(&dir.path().join("nope"));
    assert!(matches!(err, Err(Error::QueueNotFound(_))));
}

// ── 19. reap ignores malformed ──────────────────────────────────────────

#[test]
fn test_reap_ignores_malformed() {
    let (_dir, mut q) = make_queue();
    q.set_lease_timeout(1);

    // Malformed file (non-numeric prefix)
    let malformed = q.root().join("processing/abc.deadbeef01234567890abcdef01234567.md");
    fs::write(&malformed, "malformed").unwrap();

    // Valid expired file — write a proper message so parse_file works in fail()
    let fake_id = "aabbccdd11223344aabbccdd11223344";
    let valid = q
        .root()
        .join(format!("processing/0000000001000000000.{}.md", fake_id));
    let content = format!(
        "Id: {}\nCreated-At: 2020-01-01T00:00:00.000000000Z\nCreated-By: test\nPriority: normal\nRetry-Count: 0\nTTL: 0\n\ntest body",
        fake_id
    );
    fs::write(&valid, &content).unwrap();

    thread::sleep(std::time::Duration::from_secs(2));
    q.reap().unwrap();

    assert!(malformed.exists(), "malformed file should be preserved");
    assert!(!valid.exists(), "valid expired file should be reaped");
}

// ── 20. stress push/pop/purge ───────────────────────────────────────────

#[test]
fn test_stress_push_pop_purge() {
    let dir = TempDir::new().unwrap();
    let mut q = Queue::init(dir.path(), false, 0).unwrap();
    q.set_fsync_mode(FsyncMode::None);
    let q = Arc::new(q);

    let handles: Vec<_> = (0..50)
        .map(|i| {
            let q = Arc::clone(&q);
            thread::spawn(move || push_msg(&q, &format!("stress {}", i)))
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }

    let mut popped = 0;
    while let Some(claimed) = q.dequeue().unwrap() {
        q.complete(&claimed).unwrap();
        popped += 1;
    }
    assert_eq!(popped, 50);

    q.reap().unwrap();

    // Set old mtime on done files so purge picks them up
    for entry in fs::read_dir(q.root().join("done")).unwrap() {
        let entry = entry.unwrap();
        set_file_mtime(entry.path(), FileTime::from_unix_time(1_000_000, 0)).unwrap();
    }
    q.purge(1).unwrap();
    assert_eq!(count_files(&q.root().join("done")), 0);
}

// ── 21. nack filename format ────────────────────────────────────────────

#[test]
fn test_nack_filename_format() {
    let (_dir, q) = make_queue();
    let id = push_msg(&q, "M");
    let claimed = q.dequeue().unwrap().unwrap();
    q.fail(&claimed).unwrap();

    let f = find_msg(&q.root().join("pending"), &id).unwrap();
    let name = f.file_name().unwrap().to_string_lossy();
    let stem = name.strip_suffix(".md").unwrap();
    let parts: Vec<&str> = stem.split('.').collect();
    assert_eq!(parts.len(), 2);
    assert!(parts[0].chars().all(|c| c.is_ascii_digit()));
    assert_eq!(parts[1].len(), ID_LEN);
    assert!(is_hex(parts[1]));
}

// ── 22. timestamp stripping lifecycle ───────────────────────────────────

#[test]
fn test_timestamp_stripping_lifecycle() {
    let (_dir, q) = make_queue();
    let id = push_msg(&q, "Lifecycle");

    // pending: <ts>.<id>.md
    let pf = find_msg(&q.root().join("pending"), &id).unwrap();
    let pname = pf.file_name().unwrap().to_string_lossy();
    assert!(pname.contains('.'));
    assert!(pname.ends_with(&format!("{}.md", id)));
    assert_ne!(pname.as_ref(), format!("{}.md", id)); // has timestamp prefix

    // pop → processing: <claim_ts>.<id>.md
    let claimed = q.dequeue().unwrap().unwrap();
    let cname = claimed.path().file_name().unwrap().to_string_lossy();
    let cparts: Vec<&str> = cname.strip_suffix(".md").unwrap().split('.').collect();
    assert_eq!(cparts.len(), 2);

    // ack → done: <id>.md
    q.complete(&claimed).unwrap();
    let dname = format!("{}.md", id);
    assert!(q.root().join("done").join(&dname).exists());

    // Push again for nack test
    let id2 = push_msg(&q, "Lifecycle2");
    let claimed2 = q.dequeue().unwrap().unwrap();
    q.fail(&claimed2).unwrap();

    // nack → pending: <ts>.<id>.md
    let nf = find_msg(&q.root().join("pending"), &id2).unwrap();
    let nname = nf.file_name().unwrap().to_string_lossy();
    assert!(nname.contains('.'));
    assert_ne!(nname.as_ref(), format!("{}.md", id2));

    // Drain anything remaining from earlier nack test
    while let Some(c) = q.dequeue().unwrap() {
        q.complete(&c).unwrap();
    }

    // dead-letter → failed: <id>.md
    // fail() does: retry_count += 1, then checks retry_count > max_retries (3).
    // So we need 4 nacks: retry goes 1,2,3,4 → 4 > 3 → dead-lettered.
    let id3 = push_msg(&q, "DeadLetter");
    for _ in 0..(q.max_retries() + 1) {
        let c = q.dequeue().unwrap().expect("should have message to dequeue");
        q.fail(&c).unwrap();
    }
    let fname = format!("{}.md", id3);
    assert!(q.root().join("failed").join(&fname).exists());
}

// ── 23. .tmp orphan cleanup ─────────────────────────────────────────────

#[test]
fn test_tmp_orphan_cleanup() {
    let (_dir, mut q) = make_queue();
    q.set_lease_timeout(60);

    let stale = q.root().join(".tmp/stale-orphan.md");
    fs::write(&stale, "stale").unwrap();
    set_file_mtime(&stale, FileTime::from_unix_time(1_000_000, 0)).unwrap();

    let fresh = q.root().join(".tmp/fresh-orphan.md");
    fs::write(&fresh, "fresh").unwrap();
    // Fresh file keeps current mtime (just written)

    q.reap().unwrap();

    assert!(!stale.exists(), "stale orphan should be deleted");
    assert!(fresh.exists(), "fresh orphan should be preserved");
}

// ── 24. concurrent pop no duplicates ────────────────────────────────────

#[test]
fn test_concurrent_pop_no_duplicates() {
    let dir = TempDir::new().unwrap();
    let mut q = Queue::init(dir.path(), false, 0).unwrap();
    q.set_fsync_mode(FsyncMode::None);

    for i in 0..20 {
        push_msg(&q, &format!("msg {}", i));
    }

    let q = Arc::new(q);
    let handles: Vec<_> = (0..10)
        .map(|_| {
            let q = Arc::clone(&q);
            thread::spawn(move || {
                let mut count = 0u32;
                while let Ok(Some(claimed)) = q.dequeue() {
                    q.complete(&claimed).unwrap();
                    count += 1;
                }
                count
            })
        })
        .collect();

    let total: u32 = handles.into_iter().map(|h| h.join().unwrap()).sum();
    assert_eq!(total, 20);
    assert_eq!(q.depth().unwrap(), 0);
}

// ── 25. concurrent push+pop ─────────────────────────────────────────────

#[test]
fn test_concurrent_push_pop() {
    let dir = TempDir::new().unwrap();
    let mut q = Queue::init(dir.path(), false, 0).unwrap();
    q.set_fsync_mode(FsyncMode::None);
    let q = Arc::new(q);

    // 5 producers push 10 each
    let producers: Vec<_> = (0..5)
        .map(|p| {
            let q = Arc::clone(&q);
            thread::spawn(move || {
                for i in 0..10 {
                    push_msg(&q, &format!("p{}m{}", p, i));
                }
            })
        })
        .collect();

    // 5 consumers
    let consumers: Vec<_> = (0..5)
        .map(|_| {
            let q = Arc::clone(&q);
            thread::spawn(move || {
                let mut count = 0u32;
                for _ in 0..200 {
                    match q.dequeue() {
                        Ok(Some(claimed)) => {
                            q.complete(&claimed).unwrap();
                            count += 1;
                        }
                        Ok(None) => thread::sleep(std::time::Duration::from_millis(5)),
                        Err(_) => {}
                    }
                }
                count
            })
        })
        .collect();

    for p in producers {
        p.join().unwrap();
    }
    let consumed: u32 = consumers.into_iter().map(|h| h.join().unwrap()).sum();
    let remaining = q.depth().unwrap();
    assert_eq!(consumed as i64 + remaining, 50);
}

// ── 26. reap respects lease timeout ─────────────────────────────────────

#[test]
fn test_reap_respects_lease_timeout() {
    let (_dir, mut q) = make_queue();
    q.set_lease_timeout(9999);

    for _ in 0..5 {
        push_msg(&q, "M");
    }
    let mut claimed = Vec::new();
    for _ in 0..5 {
        claimed.push(q.dequeue().unwrap().unwrap());
    }

    let reaped = q.reap().unwrap();
    assert_eq!(reaped, 0);
    assert_eq!(count_files(&q.root().join("processing")), 5);

    for c in &claimed {
        q.complete(c).unwrap();
    }
}

// ── 27. claim timestamp precision ───────────────────────────────────────

#[test]
fn test_claim_timestamp_precision() {
    let (_dir, q) = make_queue();
    push_msg(&q, "M");
    let claimed = q.dequeue().unwrap().unwrap();
    let name = claimed.path().file_name().unwrap().to_string_lossy();
    let stem = name.strip_suffix(".md").unwrap();
    let ts_part = &stem[..stem.find('.').unwrap()];
    assert!(ts_part.len() >= 18, "timestamp '{}' should be >= 18 digits", ts_part);
}

// ── 28. depth 100 ───────────────────────────────────────────────────────

#[test]
fn test_depth_100() {
    let dir = TempDir::new().unwrap();
    let mut q = Queue::init(dir.path(), false, 0).unwrap();
    q.set_fsync_mode(FsyncMode::None);
    let q = Arc::new(q);

    let handles: Vec<_> = (0..100)
        .map(|i| {
            let q = Arc::clone(&q);
            thread::spawn(move || push_msg(&q, &format!("msg {}", i)))
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }

    let depth = q.depth().unwrap();
    let on_disk = count_files(&q.root().join("pending"));
    assert_eq!(depth, on_disk as i64);
    assert_eq!(depth, 100);
}

// ── 29. reap 70 processing ──────────────────────────────────────────────

#[test]
fn test_reap_70_processing() {
    let (_dir, mut q) = make_queue();
    q.set_lease_timeout(1);

    for i in 0..70 {
        push_msg(&q, &format!("msg {}", i));
    }

    // Pop all 70
    let mut claimed = Vec::new();
    for _ in 0..70 {
        claimed.push(q.dequeue().unwrap().unwrap());
    }

    // Rename to old timestamps so reap picks them up
    let processing_dir = q.root().join("processing");
    for (i, c) in claimed.iter().enumerate() {
        let path = c.path();
        let name = path.file_name().unwrap().to_string_lossy();
        let id_part = &name[name.find('.').unwrap()..]; // .<id>.md
        let old_name = format!("0000000001{:09}{}", i, id_part);
        fs::rename(path, processing_dir.join(&old_name)).unwrap();
    }

    thread::sleep(std::time::Duration::from_secs(2));
    q.reap().unwrap();
    assert_eq!(count_files(&processing_dir), 0);
}

// ── 30. max_pending enforcement ─────────────────────────────────────────

#[test]
fn test_max_pending_enforcement() {
    let dir = TempDir::new().unwrap();
    let mut q = Queue::init(dir.path(), false, 3).unwrap();
    q.set_fsync_mode(FsyncMode::None);

    push_msg(&q, "A");
    push_msg(&q, "B");
    push_msg(&q, "C");

    let err = q.enqueue(&mut Message::default());
    assert!(matches!(err, Err(Error::MaxPendingExceeded(_))));
    assert_eq!(q.depth().unwrap(), 3);

    // Pop one, push should succeed again
    let claimed = q.dequeue().unwrap().unwrap();
    q.complete(&claimed).unwrap();
    push_msg(&q, "D");
}

#[test]
fn test_max_pending_unlimited() {
    let dir = TempDir::new().unwrap();
    let mut q = Queue::init(dir.path(), false, 0).unwrap();
    q.set_fsync_mode(FsyncMode::None);

    for i in 0..50 {
        push_msg(&q, &format!("msg {}", i));
    }
    assert_eq!(q.depth().unwrap(), 50);
}

// ── 31. empty purge ─────────────────────────────────────────────────────

#[test]
fn test_empty_purge() {
    let (_dir, q) = make_queue();
    assert_eq!(q.purge(0).unwrap(), 0);
}

// ── 32. nack atomicity ──────────────────────────────────────────────────

#[test]
fn test_nack_atomicity_no_tmp_leftovers() {
    let (_dir, q) = make_queue();
    push_msg(&q, "M");
    let claimed = q.dequeue().unwrap().unwrap();
    q.fail(&claimed).unwrap();
    assert_eq!(count_files(&q.root().join(".tmp")), 0);
}

// ── 33. nack no-loss ────────────────────────────────────────────────────

#[test]
fn test_nack_no_loss() {
    let (_dir, q) = make_queue();
    let a = push_msg(&q, "A");
    let b = push_msg(&q, "B");

    let claimed = q.dequeue().unwrap().unwrap();
    q.fail(&claimed).unwrap();

    let mut consumed = Vec::new();
    while let Some(c) = q.dequeue().unwrap() {
        let msg = parse_file(c.path()).unwrap();
        consumed.push(msg.header.id.clone().unwrap());
        q.complete(&c).unwrap();
    }

    consumed.sort();
    let mut expected = vec![a, b];
    expected.sort();
    assert_eq!(consumed, expected);
}

// ── 34. reap orphan detection ───────────────────────────────────────────

#[test]
fn test_reap_orphan_detection() {
    let (_dir, mut q) = make_queue();
    q.set_lease_timeout(1);

    let id = push_msg(&q, "M");
    let claimed = q.dequeue().unwrap().unwrap();

    // Simulate crash: copy processing file back to pending
    let msg = parse_file(claimed.path()).unwrap();
    let pending_name = format!("9999999999000000000.{}.md", id);
    let pending_path = q.root().join("pending").join(&pending_name);
    fs::write(&pending_path, msg.serialize().unwrap()).unwrap();

    // Rename processing to old timestamp so reap picks it up
    let old_name = format!("0000000001000000000.{}.md", id);
    let old_path = q.root().join("processing").join(&old_name);
    fs::rename(claimed.path(), &old_path).unwrap();

    thread::sleep(std::time::Duration::from_secs(2));
    q.reap().unwrap();

    assert_eq!(count_files(&q.root().join("processing")), 0, "orphan in processing removed");
    assert_eq!(count_files(&q.root().join("pending")), 1, "pending copy preserved");
}

// ── 35. concurrent pop heavy ────────────────────────────────────────────

#[test]
fn test_concurrent_pop_heavy_50_20() {
    let dir = TempDir::new().unwrap();
    let mut q = Queue::init(dir.path(), false, 0).unwrap();
    q.set_fsync_mode(FsyncMode::None);

    for i in 0..50 {
        push_msg(&q, &format!("msg {}", i));
    }

    let q = Arc::new(q);
    let handles: Vec<_> = (0..20)
        .map(|_| {
            let q = Arc::clone(&q);
            thread::spawn(move || {
                let mut count = 0u32;
                while let Ok(Some(claimed)) = q.dequeue() {
                    q.complete(&claimed).unwrap();
                    count += 1;
                }
                count
            })
        })
        .collect();

    let total: u32 = handles.into_iter().map(|h| h.join().unwrap()).sum();
    assert_eq!(total, 50);
    assert_eq!(q.depth().unwrap(), 0);
}

// ── 36. reply_to ────────────────────────────────────────────────────────

#[test]
fn test_reply_to() {
    let (_dir, q) = make_queue();

    let id1 = push_msg_with(&q, "with reply", |m| {
        m.header.reply_to = Some("/tmp/replies".into());
    });
    let f1 = find_msg(&q.root().join("pending"), &id1).unwrap();
    let msg1 = parse_file(&f1).unwrap();
    assert_eq!(msg1.header.reply_to.as_deref(), Some("/tmp/replies"));

    let id2 = push_msg(&q, "without reply");
    let f2 = find_msg(&q.root().join("pending"), &id2).unwrap();
    let msg2 = parse_file(&f2).unwrap();
    assert!(msg2.header.reply_to.is_none());
}

// ── 37. depends-on and ready ────────────────────────────────────────────

#[test]
fn test_depends_on_and_ready() {
    let (_dir, q) = make_queue();

    let a = push_msg(&q, "Task A");
    let b = push_msg_with(&q, "Task B", |m| m.header.depends_on = vec![a.clone()]);
    let c = push_msg_with(&q, "Task C", |m| {
        m.header.depends_on = vec![a.clone(), b.clone()];
    });
    let d = push_msg(&q, "Task D");

    // Initially only A and D are ready
    let ready = q.list_ready().unwrap();
    assert!(ready.contains(&a));
    assert!(ready.contains(&d));
    assert!(!ready.contains(&b));
    assert!(!ready.contains(&c));

    // Complete A → B becomes ready
    let ca = q.dequeue().unwrap().unwrap();
    q.complete(&ca).unwrap();
    let ready = q.list_ready().unwrap();
    assert!(ready.contains(&b));
    assert!(!ready.contains(&c));

    // Complete B → C becomes ready
    let cb = q.dequeue().unwrap().unwrap();
    q.complete(&cb).unwrap();
    let ready = q.list_ready().unwrap();
    assert!(ready.contains(&c));

    // Complete C and D → ready is empty
    let cc = q.dequeue().unwrap().unwrap();
    q.complete(&cc).unwrap();
    let cd = q.dequeue().unwrap().unwrap();
    q.complete(&cd).unwrap();
    let ready = q.list_ready().unwrap();
    assert!(ready.is_empty());
}

// ── 38. MessageId::generate ─────────────────────────────────────────────

#[test]
fn test_generate_id() {
    let id1 = MessageId::generate();
    let id2 = MessageId::generate();
    assert_eq!(id1.as_str().len(), ID_LEN);
    assert_eq!(id2.as_str().len(), ID_LEN);
    assert!(is_hex(id1.as_str()));
    assert!(is_hex(id2.as_str()));
    assert_ne!(id1, id2);
}

// ── 39. serialize roundtrip ─────────────────────────────────────────────

#[test]
fn test_serialize_roundtrip() {
    let (_dir, q) = make_queue();
    let id = push_msg_with(&q, "Body text\nLine 2", |m| {
        m.header.priority = Priority::High;
        m.header.ttl = 120;
        m.header.tags = vec!["a".into(), "b".into()];
        m.header.correlation_id = Some("corr-1".into());
        m.header.reply_to = Some("/reply/path".into());
        m.header.depends_on = vec!["de01de01de01de01de01de01de01de01".parse().unwrap(), "de02de02de02de02de02de02de02de02".parse().unwrap()];
        m.header.custom = vec!["key1: val1".into(), "key2: val2".into()];
    });

    let f = find_msg(&q.root().join("pending"), &id).unwrap();
    let msg = parse_file(&f).unwrap();

    assert_eq!(msg.header.id.as_ref().unwrap(), &id);
    assert_eq!(msg.header.priority, Priority::High);
    assert_eq!(msg.header.ttl, 120);
    assert_eq!(msg.header.tags, vec!["a".to_string(), "b".to_string()]);
    assert_eq!(msg.header.correlation_id.as_deref(), Some("corr-1"));
    assert_eq!(msg.header.reply_to.as_deref(), Some("/reply/path"));
    assert_eq!(msg.header.depends_on.len(), 2);
    assert_eq!(msg.header.depends_on[0].as_str(), "de01de01de01de01de01de01de01de01");
    assert_eq!(msg.header.depends_on[1].as_str(), "de02de02de02de02de02de02de02de02");
    assert!(msg.header.custom.contains(&"key1: val1".to_string()));
    assert!(msg.header.custom.contains(&"key2: val2".to_string()));
    assert_eq!(msg.body, "Body text\nLine 2");
}

// ── 40. reap with nanosecond timestamps ─────────────────────────────────

#[test]
fn test_reap_nanosecond_timestamps() {
    let (_dir, mut q) = make_queue();
    q.set_lease_timeout(1);

    let id = push_msg(&q, "M");
    let claimed = q.dequeue().unwrap().unwrap();

    // Rename to old nanosecond timestamp
    let old_name = format!("0000000001123456789.{}.md", id);
    let old_path = q.root().join("processing").join(&old_name);
    fs::rename(claimed.path(), &old_path).unwrap();

    thread::sleep(std::time::Duration::from_secs(2));
    let reaped = q.reap().unwrap();
    assert!(reaped >= 1);
    assert!(!old_path.exists());
}

// ── 41. corrupt max_pending ─────────────────────────────────────────────

#[test]
fn test_corrupt_max_pending() {
    let dir = TempDir::new().unwrap();
    Queue::init(dir.path(), false, 100).unwrap();

    fs::write(dir.path().join(".meta/max_pending"), "not-a-number").unwrap();
    let err = Queue::open(dir.path());
    assert!(matches!(err, Err(Error::Parse(_))));
}

// ── 42. custom fields ───────────────────────────────────────────────────

#[test]
fn test_custom_fields() {
    let (_dir, q) = make_queue();
    let id = push_msg_with(&q, "body", |m| {
        m.header.custom = vec!["order_id: 12345".into(), "region: us-east-1".into()];
    });
    let f = find_msg(&q.root().join("pending"), &id).unwrap();
    let msg = parse_file(&f).unwrap();
    assert!(msg.header.custom.contains(&"order_id: 12345".to_string()));
    assert!(msg.header.custom.contains(&"region: us-east-1".to_string()));
}

// ── 43. FromStr trait for Priority ──────────────────────────────────────

#[test]
fn test_priority_from_str_trait() {
    assert_eq!("critical".parse::<Priority>().unwrap(), Priority::Critical);
    assert_eq!("high".parse::<Priority>().unwrap(), Priority::High);
    assert_eq!("normal".parse::<Priority>().unwrap(), Priority::Normal);
    assert_eq!("low".parse::<Priority>().unwrap(), Priority::Low);
    assert_eq!("0".parse::<Priority>().unwrap(), Priority::Critical);
    assert!("garbage".parse::<Priority>().is_err());
}

// ── 44. MessageId FromStr validation ────────────────────────────────────

#[test]
fn test_message_id_from_str() {
    let valid = "aabbccdd11223344aabbccdd11223344";
    let id: MessageId = valid.parse().unwrap();
    assert_eq!(id.as_str(), valid);

    // Too short
    assert!("aabb".parse::<MessageId>().is_err());
    // Non-hex
    assert!("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz".parse::<MessageId>().is_err());
    // Too long
    assert!("aabbccdd11223344aabbccdd11223344aa".parse::<MessageId>().is_err());
}

// ── 45. Message FromStr ─────────────────────────────────────────────────

#[test]
fn test_message_from_str() {
    let content = "Id: aabbccdd11223344aabbccdd11223344\nCreated-At: 2024-01-01T00:00:00.000000000Z\nCreated-By: test\nPriority: normal\nRetry-Count: 0\nTTL: 0\n\nHello body";
    let msg: Message = content.parse().unwrap();
    assert_eq!(msg.header.id.as_ref().unwrap().as_str(), "aabbccdd11223344aabbccdd11223344");
    assert_eq!(msg.body, "Hello body");
}

// ── 46. ClaimedMessage from_path ────────────────────────────────────────

#[test]
fn test_claimed_message_from_path() {
    use rfbmq_core::ClaimedMessage;

    let claimed = ClaimedMessage::from_path("/some/path/0000000001000000000.aabbccdd11223344aabbccdd11223344.md").unwrap();
    assert_eq!(claimed.id().as_str(), "aabbccdd11223344aabbccdd11223344");
    assert_eq!(claimed.to_string(), "/some/path/0000000001000000000.aabbccdd11223344aabbccdd11223344.md");

    // Invalid path
    assert!(ClaimedMessage::from_path("/bad/path.txt").is_err());
}
