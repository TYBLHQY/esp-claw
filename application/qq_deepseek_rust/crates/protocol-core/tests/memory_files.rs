#[path = "../../../src/platform/memory_files.rs"]
mod files;
use qq_deepseek_core::memory::{self, MemoryError, MemoryTurn, SessionMemory};
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
            "qqds-memory-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&p).unwrap();
        Self(p)
    }
    fn slot(&self, key: &str, slot: usize) -> PathBuf {
        self.0
            .join("memory")
            .join(format!("{}-{slot}.mem", files::identifier(key).unwrap()))
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn data(key: &str) -> SessionMemory {
    let mut d = SessionMemory::empty(key);
    d.summary = "摘要 Résumé".into();
    d.facts.push("User likes concise answers".into());
    d.turns.push(MemoryTurn {
        user: "你好".into(),
        assistant: "你好！".into(),
    });
    d
}
#[test]
fn restart_load_and_optimistic_revision_conflict() {
    let dir = Directory::new();
    let d = data("u:alice");
    assert_eq!(files::load(&dir.0, &d.key).unwrap().revision, 0);
    assert_eq!(files::save(&dir.0, 0, &d), Ok(1));
    assert_eq!(files::save(&dir.0, 0, &d), Err(MemoryError::Conflict));
    let reloaded = files::load(&dir.0, &d.key).unwrap();
    assert_eq!(reloaded.revision, 1);
    assert_eq!(reloaded.data, Some(d.clone()));
    assert_eq!(files::save(&dir.0, 1, &d), Ok(2));
    assert_eq!(files::load(&dir.0, &d.key).unwrap().revision, 2);
}
#[test]
fn torn_new_slot_falls_back_and_both_bad_are_preserved() {
    let dir = Directory::new();
    let d = data("u:alice");
    files::save(&dir.0, 0, &d).unwrap();
    let mut next = d.clone();
    next.summary = "new summary".into();
    files::save(&dir.0, 1, &next).unwrap();
    let newest = dir.slot(&d.key, 1);
    let bytes = fs::read(&newest).unwrap();
    fs::write(&newest, &bytes[..bytes.len() / 2]).unwrap();
    let old = files::load(&dir.0, &d.key).unwrap();
    assert_eq!(old.revision, 1);
    assert_eq!(old.data, Some(d.clone()));
    fs::write(dir.slot(&d.key, 0), b"corrupt").unwrap();
    assert_eq!(files::load(&dir.0, &d.key), Err(MemoryError::Corrupt));
    assert_eq!(files::save(&dir.0, 0, &d), Err(MemoryError::Corrupt));
    assert_eq!(fs::read(newest).unwrap(), bytes[..bytes.len() / 2]);
}
#[test]
fn key_collision_is_never_overwritten() {
    let dir = Directory::new();
    let d = data("u:alice");
    files::save(&dir.0, 0, &d).unwrap();
    let other = data("u:bob");
    let bytes = memory::encode(&other, 2).unwrap();
    fs::write(dir.slot(&d.key, 1), &bytes).unwrap();
    assert_eq!(files::load(&dir.0, &d.key), Err(MemoryError::Conflict));
    assert_eq!(files::save(&dir.0, 1, &d), Err(MemoryError::Conflict));
    assert_eq!(fs::read(dir.slot(&d.key, 1)).unwrap(), bytes);
}
#[test]
fn reset_tombstone_keeps_facts_and_rejects_old_save() {
    let dir = Directory::new();
    let d = data("u:alice");
    files::save(&dir.0, 0, &d).unwrap();
    assert_eq!(files::reset(&dir.0, &d.key, true), Ok(3));
    let reset = files::load(&dir.0, &d.key).unwrap().data.unwrap();
    assert!(reset.turns.is_empty());
    assert!(reset.summary.is_empty());
    assert_eq!(reset.facts, d.facts);
    assert_eq!(files::save(&dir.0, 1, &d), Err(MemoryError::Conflict));
    assert_eq!(files::reset(&dir.0, &d.key, false), Ok(5));
    assert!(files::load(&dir.0, &d.key)
        .unwrap()
        .data
        .unwrap()
        .facts
        .is_empty());
    fs::write(dir.slot(&d.key, 0), b"corrupt latest tombstone").unwrap();
    let fallback = files::load(&dir.0, &d.key).unwrap().data.unwrap();
    assert!(fallback.facts.is_empty() && fallback.turns.is_empty() && fallback.summary.is_empty());
}
#[test]
fn persistent_session_limit_never_evicts_existing_files() {
    let dir = Directory::new();
    for n in 0..memory::MAX_PERSISTED_SESSIONS {
        files::save(&dir.0, 0, &data(&format!("u:user{n}"))).unwrap();
    }
    assert_eq!(
        files::save(&dir.0, 0, &data("u:overflow")),
        Err(MemoryError::Capacity)
    );
    assert_eq!(files::save(&dir.0, 1, &data("u:user0")), Ok(2));
    assert_eq!(files::load(&dir.0, "u:user1").unwrap().revision, 1);
}
#[test]
fn path_and_symlink_targets_are_rejected() {
    let dir = Directory::new();
    assert_eq!(files::load(&dir.0, "u:../bad"), Err(MemoryError::Invalid));
    #[cfg(unix)]
    {
        let outside = Directory::new();
        std::os::unix::fs::symlink(&outside.0, dir.0.join("memory")).unwrap();
        assert_eq!(
            files::save(&dir.0, 0, &data("u:alice")),
            Err(MemoryError::Corrupt)
        );
        assert_eq!(fs::read_dir(&outside.0).unwrap().count(), 0);
        fs::remove_file(dir.0.join("memory")).unwrap();
        fs::create_dir(dir.0.join("memory")).unwrap();
        let victim = outside.0.join("victim");
        fs::write(&victim, b"preserve").unwrap();
        std::os::unix::fs::symlink(&victim, dir.slot("u:alice", 0)).unwrap();
        assert_eq!(
            files::save(&dir.0, 0, &data("u:alice")),
            Err(MemoryError::Corrupt)
        );
        assert_eq!(fs::read(victim).unwrap(), b"preserve");
    }
}
