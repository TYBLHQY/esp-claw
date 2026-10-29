//! Only the SD worker calls this module. Fixed ring, mirrored reset watermark,
//! one record in memory at a time, no directory-wide chat index.
use super::memory_files::{identifier, load};
use qq_deepseek_core::{
    history::{self, Index, SearchResult, HISTORY_SLOTS},
    memory::{MemoryError, MemoryTurn},
    record::crc32,
};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
const INDEX_BYTES: usize = 1024;
const SEARCH_TIME: Duration = Duration::from_millis(1500);
fn io(stage: &'static str, e: std::io::Error) -> MemoryError {
    MemoryError::Io {
        stage,
        code: e.raw_os_error().unwrap_or(-3),
    }
}
fn directory(root: &Path, create: bool) -> Result<Option<PathBuf>, MemoryError> {
    for path in [root.to_owned(), root.join("history")] {
        match fs::symlink_metadata(&path) {
            Ok(m) if m.is_dir() && !m.file_type().is_symlink() => (),
            Ok(_) => return Err(MemoryError::Corrupt),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if !create {
                    return Ok(None);
                }
                fs::create_dir(&path).map_err(|e| io("history_directory", e))?;
            }
            Err(e) => return Err(io("history_directory", e)),
        }
    }
    Ok(Some(root.join("history")))
}
fn read(path: &Path, limit: usize) -> Result<Option<Vec<u8>>, MemoryError> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) if m.is_file() && !m.file_type().is_symlink() => m,
        Ok(_) => return Err(MemoryError::Conflict),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io("history_metadata", e)),
    };
    if meta.len() > limit as u64 {
        return Err(MemoryError::Corrupt);
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(meta.len() as usize)
        .map_err(|_| MemoryError::Capacity)?;
    File::open(path)
        .map_err(|e| io("history_open", e))?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| io("history_read", e))?;
    if bytes.len() > limit {
        return Err(MemoryError::Corrupt);
    }
    Ok(Some(bytes))
}
fn write(path: &Path, bytes: &[u8]) -> Result<(), MemoryError> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_file() && !m.file_type().is_symlink() => (),
        Ok(_) => return Err(MemoryError::Conflict),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(io("history_write_metadata", e)),
    }
    let mut f = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .map_err(|e| io("history_write_open", e))?;
    f.write_all(bytes).map_err(|e| io("history_write", e))?;
    f.sync_all().map_err(|e| io("history_sync", e))?;
    drop(f);
    if read(path, bytes.len())?.as_deref() != Some(bytes) {
        return Err(MemoryError::Io {
            stage: "history_verify",
            code: -1,
        });
    }
    Ok(())
}
fn index_path(dir: &Path, id: &str, slot: usize) -> PathBuf {
    dir.join(format!("{id}-i{slot}.idx"))
}
fn record_path(dir: &Path, id: &str, sequence: u64) -> PathBuf {
    dir.join(format!("{id}-{:02}.his", sequence % HISTORY_SLOTS))
}
fn decode_index(bytes: &[u8], key: &str) -> Result<Index, MemoryError> {
    if bytes.len() < 4 || bytes.len() > INDEX_BYTES {
        return Err(MemoryError::Corrupt);
    }
    let end = bytes.len() - 4;
    if crc32(&bytes[..end]) != u32::from_le_bytes(bytes[end..].try_into().unwrap()) {
        return Err(MemoryError::Corrupt);
    }
    let value: Index = serde_json::from_slice(&bytes[..end]).map_err(|_| MemoryError::Corrupt)?;
    value.validate(key)?;
    if value.revision == 0 {
        return Err(MemoryError::Corrupt);
    }
    Ok(value)
}
fn current(dir: &Path, id: &str, key: &str) -> Result<(Option<usize>, Index), MemoryError> {
    let mut selected: Option<(usize, Index)> = None;
    let mut exists = false;
    for slot in 0..2 {
        let bytes = match read(&index_path(dir, id, slot), INDEX_BYTES) {
            Ok(None) => continue,
            Ok(Some(bytes)) => bytes,
            Err(MemoryError::Corrupt) => {
                exists = true;
                continue;
            }
            Err(e) => return Err(e),
        };
        exists = true;
        let value = match decode_index(&bytes, key) {
            Ok(v) => v,
            Err(MemoryError::Corrupt) => continue,
            Err(e) => return Err(e),
        };
        if let Some((_, old)) = &selected {
            if old.revision == value.revision && old != &value {
                return Err(MemoryError::Corrupt);
            }
            if old.revision > value.revision {
                continue;
            }
        }
        selected = Some((slot, value));
    }
    match selected {
        Some((slot, index)) => Ok((Some(slot), index)),
        None if exists => Err(MemoryError::Corrupt),
        None => Ok((None, Index::empty(key))),
    }
}
fn commit(
    dir: &Path,
    id: &str,
    active: Option<usize>,
    index: &mut Index,
) -> Result<usize, MemoryError> {
    index.revision = index.revision.checked_add(1).ok_or(MemoryError::Capacity)?;
    let mut bytes = serde_json::to_vec(index).map_err(|_| MemoryError::Invalid)?;
    let crc = crc32(&bytes);
    bytes.extend_from_slice(&crc.to_le_bytes());
    if bytes.len() > INDEX_BYTES {
        return Err(MemoryError::Capacity);
    }
    let slot = active.map_or(0, |s| 1 - s);
    write(&index_path(dir, id, slot), &bytes)?;
    Ok(slot)
}
pub fn append(root: &Path, key: &str, revision: u64, turn: &MemoryTurn) -> Result<(), MemoryError> {
    // A queued old or uncommitted turn must not repopulate an acknowledged reset.
    let memory = load(root, key)?;
    if memory.revision != revision {
        return Err(MemoryError::Conflict);
    }
    let data = memory.data.ok_or(MemoryError::Conflict)?;
    if !data.turns.last().is_some_and(|last| {
        history::clip(&last.user, history::USER_BYTES) == turn.user
            && history::clip(&last.assistant, history::ASSISTANT_BYTES) == turn.assistant
    }) {
        return Err(MemoryError::Conflict);
    }
    let unix = data.updated_unix;
    let id = identifier(key)?;
    let dir = directory(root, true)?.ok_or(MemoryError::Unavailable)?;
    let (active, mut index) = current(&dir, &id, key)?;
    if revision <= index.source_revision {
        return if revision == index.source_revision {
            Ok(())
        } else {
            Err(MemoryError::Conflict)
        };
    }
    let bytes = history::encode(
        key,
        index.next,
        unix,
        data.turns.last().ok_or(MemoryError::Conflict)?,
    )?;
    drop(data);
    let path = record_path(&dir, &id, index.next);
    // Existing valid records must belong to this key. Torn records in our known
    // ring may be replaced; foreign/symlink paths are never overwritten.
    if let Ok(Some(old)) = read(&path, history::MAX_RECORD_BYTES) {
        if matches!(history::decode(&old, key), Err(MemoryError::Conflict)) {
            return Err(MemoryError::Conflict);
        }
    } else if let Err(e) = read(&path, history::MAX_RECORD_BYTES) {
        if e != MemoryError::Corrupt {
            return Err(e);
        }
    }
    write(&path, &bytes)?;
    index.next = index.next.checked_add(1).ok_or(MemoryError::Capacity)?;
    index.source_revision = revision;
    commit(&dir, &id, active, &mut index)?;
    Ok(())
}
pub fn search(root: &Path, key: &str, query: &str) -> Result<SearchResult, MemoryError> {
    let mut rank = history::Ranker::new(query)?;
    let id = identifier(key)?;
    let Some(dir) = directory(root, false)? else {
        return Ok(rank.finish());
    };
    let (_, index) = current(&dir, &id, key)?;
    let deadline = Instant::now() + SEARCH_TIME;
    for seq in (index.floor.max(index.next.saturating_sub(HISTORY_SLOTS))..index.next).rev() {
        if Instant::now() >= deadline {
            return Err(MemoryError::Timeout);
        }
        let record = match read(&record_path(&dir, &id, seq), history::MAX_RECORD_BYTES) {
            Ok(Some(bytes)) => history::decode(&bytes, key),
            Ok(None) | Err(MemoryError::Corrupt) => {
                rank.damaged();
                continue;
            }
            Err(e) => return Err(e),
        };
        match record {
            Ok(record) if record.sequence == seq => rank.consider(record),
            Err(MemoryError::Conflict) => return Err(MemoryError::Conflict),
            _ => rank.damaged(),
        }
    }
    Ok(rank.finish())
}
pub fn clear(root: &Path, key: &str) -> Result<(), MemoryError> {
    let id = identifier(key)?;
    let Some(dir) = directory(root, false)? else {
        return Ok(());
    };
    let (active, mut index) = current(&dir, &id, key)?;
    index.floor = index.next;
    let slot = commit(&dir, &id, active, &mut index)?;
    commit(&dir, &id, Some(slot), &mut index)?;
    // The watermark is durable in both slots before removing old records.
    for n in 0..HISTORY_SLOTS {
        let path = record_path(&dir, &id, n);
        if let Some(bytes) = read(&path, history::MAX_RECORD_BYTES)? {
            history::decode(&bytes, key)?;
            fs::remove_file(&path).map_err(|e| io("history_delete", e))?;
        }
    }
    Ok(())
}
