#[path = "../../../src/platform/history_files.rs"]
mod history_files;
#[path = "../../../src/platform/memory_files.rs"]
mod memory_files;
use qq_deepseek_core::{
    history,
    memory::{MemoryError, MemoryTurn, SessionMemory},
};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU32, Ordering},
};
static COUNTER: AtomicU32 = AtomicU32::new(0);
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "qq-history-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&p).unwrap();
        Self(p)
    }
    fn append(&self, key: &str, user: &str) -> u64 {
        let current = memory_files::load(&self.0, key).unwrap();
        let mut d = SessionMemory::empty(key);
        let turn = MemoryTurn {
            user: user.into(),
            assistant: "Historical answer".into(),
        };
        d.turns.push(turn.clone());
        d.updated_unix = 1234;
        let revision = memory_files::save(&self.0, current.revision, &d).unwrap();
        history_files::append(&self.0, key, revision, &turn).unwrap();
        revision
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[test]
fn archive_survives_context_replacement_and_keeps_only_fixed_window() {
    let dir = Directory::new();
    for i in 0..40 {
        dir.append("u:alice", &format!("Cedar 彗星计划 {i}"));
    }
    let result = history_files::search(&dir.0, "u:alice", "Cedar 彗星").unwrap();
    assert_eq!(result.scanned, history::HISTORY_SLOTS as usize);
    assert_eq!(result.hits.len(), 3);
    assert_eq!(result.hits[0].sequence, 40);
    assert_eq!(fs::read_dir(dir.0.join("history")).unwrap().count(), 34);
    assert!(result
        .hits
        .iter()
        .all(|h| h.snippet.len() <= history::SNIPPET_BYTES));
    let r = history_files::search(&dir.0, "g:test:alice", "Cedar").unwrap();
    assert!(r.hits.is_empty());
    let r = history_files::search(&dir.0, "u:bob", "Cedar").unwrap();
    assert!(r.hits.is_empty());
}
#[test]
fn clear_is_mirrored_and_stale_append_cannot_revive_deleted_chat() {
    let dir = Directory::new();
    let revision = dir.append("u:alice", "private Cedar plan");
    let old = memory_files::load(&dir.0, "u:alice").unwrap().data.unwrap();
    history_files::clear(&dir.0, "u:alice").unwrap();
    memory_files::reset(&dir.0, "u:alice", true).unwrap();
    assert_eq!(
        history_files::append(&dir.0, "u:alice", revision, &old.turns[0]),
        Err(MemoryError::Conflict)
    );
    assert!(history_files::search(&dir.0, "u:alice", "Cedar")
        .unwrap()
        .hits
        .is_empty());
    let id = memory_files::identifier("u:alice").unwrap();
    // Either surviving manifest still carries the reset floor.
    fs::write(dir.0.join("history").join(format!("{id}-i0.idx")), b"torn").unwrap();
    assert!(history_files::search(&dir.0, "u:alice", "Cedar")
        .unwrap()
        .hits
        .is_empty());
    dir.append("u:alice", "new Juniper plan");
    assert_eq!(
        history_files::search(&dir.0, "u:alice", "Juniper")
            .unwrap()
            .hits
            .len(),
        1
    );
}
#[test]
fn damaged_record_is_reported_foreign_record_is_never_read_or_overwritten() {
    let dir = Directory::new();
    dir.append("u:alice", "Cedar plan");
    let id = memory_files::identifier("u:alice").unwrap();
    let path = dir.0.join("history").join(format!("{id}-01.his"));
    fs::write(&path, b"torn").unwrap();
    let result = history_files::search(&dir.0, "u:alice", "Cedar").unwrap();
    assert_eq!(result.damaged, 1);
    assert!(result.hits.is_empty());
    let foreign = history::encode(
        "u:bob",
        1,
        1,
        &MemoryTurn {
            user: "private".into(),
            assistant: "secret".into(),
        },
    )
    .unwrap();
    fs::write(&path, &foreign).unwrap();
    assert!(matches!(
        history_files::search(&dir.0, "u:alice", "private"),
        Err(MemoryError::Conflict)
    ));
    assert_eq!(fs::read(path).unwrap(), foreign);
}
#[cfg(unix)]
#[test]
fn symlink_directory_is_rejected_without_touching_target() {
    use std::os::unix::fs::symlink;
    let dir = Directory::new();
    let target = Directory::new();
    symlink(&target.0, dir.0.join("history")).unwrap();
    assert!(history_files::search(&dir.0, "u:alice", "Cedar").is_err());
    assert_eq!(fs::read_dir(&target.0).unwrap().count(), 0);
}
