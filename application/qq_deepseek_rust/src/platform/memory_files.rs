//! SD-owner-only two-slot memory storage. No legacy files or arbitrary paths.
use qq_deepseek_core::memory::{
    self, LoadedMemory, MemoryError, SessionMemory, MAX_PERSISTED_SESSIONS,
};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
const MEMORY_DIRECTORY: &str = "memory";
const MAX_DIRECTORY_ENTRIES: usize = MAX_PERSISTED_SESSIONS * 4;
const UNKNOWN_IO: i32 = -3;
const VERIFY_MISMATCH: i32 = -1;
fn io(stage: &'static str, error: std::io::Error) -> MemoryError {
    MemoryError::Io {
        stage,
        code: error.raw_os_error().unwrap_or(UNKNOWN_IO),
    }
}
fn directory(path: &Path, create: bool) -> Result<bool, MemoryError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => Ok(true),
        Ok(_) => Err(MemoryError::Corrupt),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if create {
                fs::create_dir(path).map_err(|e| io("memory_directory_create", e))?;
                Ok(true)
            } else {
                Ok(false)
            }
        }
        Err(e) => Err(io("memory_directory_metadata", e)),
    }
}
fn memory_directory(root: &Path, create: bool) -> Result<Option<PathBuf>, MemoryError> {
    if !directory(root, create)? {
        return Ok(None);
    }
    let path = root.join(MEMORY_DIRECTORY);
    Ok(directory(&path, create)?.then_some(path))
}
/// Stable opaque filenames; full key validation detects collisions. This is not encryption.
pub fn identifier(key: &str) -> Result<String, MemoryError> {
    if !memory::valid_key(key) {
        return Err(MemoryError::Invalid);
    }
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for b in key.bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Ok(format!(
        "m-{hash:016x}{:08x}",
        qq_deepseek_core::record::crc32(key.as_bytes())
    ))
}
fn slot_path(directory: &Path, identifier: &str, slot: usize) -> PathBuf {
    directory.join(format!("{identifier}-{slot}.mem"))
}
enum Slot {
    Missing,
    Damaged,
    Valid(u64, SessionMemory),
}
fn read_slot(path: &Path, key: &str) -> Result<Slot, MemoryError> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => meta,
        Ok(_) => return Err(MemoryError::Corrupt),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Slot::Missing),
        Err(e) => return Err(io("memory_slot_metadata", e)),
    };
    if meta.len() > memory::MAX_RECORD_BYTES as u64 {
        return Ok(Slot::Damaged);
    }
    let file = File::open(path).map_err(|e| io("memory_slot_open", e))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(meta.len() as usize)
        .map_err(|_| MemoryError::Capacity)?;
    file.take(memory::MAX_RECORD_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| io("memory_slot_read", e))?;
    match memory::decode(&bytes, key) {
        Ok((revision, data)) => Ok(Slot::Valid(revision, data)),
        Err(MemoryError::Corrupt) => Ok(Slot::Damaged),
        Err(error) => Err(error),
    }
}
struct Current {
    active: Option<usize>,
    loaded: LoadedMemory,
}
fn read_current(directory: &Path, identifier: &str, key: &str) -> Result<Current, MemoryError> {
    let left = read_slot(&slot_path(directory, identifier, 0), key)?;
    let right = read_slot(&slot_path(directory, identifier, 1), key)?;
    let selected = match (left, right) {
        (Slot::Missing, Slot::Missing) => {
            return Ok(Current {
                active: None,
                loaded: LoadedMemory {
                    revision: 0,
                    data: None,
                },
            })
        }
        (Slot::Valid(a, one), Slot::Valid(b, two)) => {
            if a == b && one != two {
                return Err(MemoryError::Corrupt);
            }
            if a >= b {
                (0, a, one)
            } else {
                (1, b, two)
            }
        }
        (Slot::Valid(revision, data), _) => (0, revision, data),
        (_, Slot::Valid(revision, data)) => (1, revision, data),
        _ => return Err(MemoryError::Corrupt),
    };
    Ok(Current {
        active: Some(selected.0),
        loaded: LoadedMemory {
            revision: selected.1,
            data: Some(selected.2),
        },
    })
}
pub fn load(root: &Path, key: &str) -> Result<LoadedMemory, MemoryError> {
    let id = identifier(key)?;
    let Some(directory) = memory_directory(root, false)? else {
        return Ok(LoadedMemory {
            revision: 0,
            data: None,
        });
    };
    Ok(read_current(&directory, &id, key)?.loaded)
}
fn capacity(directory: &Path) -> Result<(), MemoryError> {
    let entries = fs::read_dir(directory).map_err(|e| io("memory_capacity_open", e))?;
    let mut sessions = BTreeSet::new();
    for (n, entry) in entries.enumerate() {
        if n >= MAX_DIRECTORY_ENTRIES {
            return Err(MemoryError::Capacity);
        }
        let entry = entry.map_err(|e| io("memory_capacity_next", e))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(id) = name
            .strip_suffix("-0.mem")
            .or_else(|| name.strip_suffix("-1.mem"))
        {
            if id.len() == 26
                && id.starts_with("m-")
                && id[2..].bytes().all(|b| b.is_ascii_hexdigit())
            {
                sessions.insert(id.to_owned());
            }
        }
    }
    if sessions.len() >= MAX_PERSISTED_SESSIONS {
        Err(MemoryError::Capacity)
    } else {
        Ok(())
    }
}
pub fn save(root: &Path, expected_revision: u64, data: &SessionMemory) -> Result<u64, MemoryError> {
    data.validate()?;
    let id = identifier(&data.key)?;
    let directory = memory_directory(root, true)?.ok_or(MemoryError::Unavailable)?;
    let current = read_current(&directory, &id, &data.key)?;
    if current.loaded.revision != expected_revision {
        return Err(MemoryError::Conflict);
    }
    if current.active.is_none() {
        capacity(&directory)?;
    }
    let revision = expected_revision
        .checked_add(1)
        .ok_or(MemoryError::Capacity)?;
    let bytes = memory::encode(data, revision)?;
    let slot = current.active.map_or(0, |slot| 1 - slot);
    let path = slot_path(&directory, &id, slot);
    // Both slot paths were type-checked before this write. Only this worker owns the directory.
    {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .map_err(|e| io("memory_slot_write_open", e))?;
        file.write_all(&bytes)
            .map_err(|e| io("memory_slot_write", e))?;
        file.sync_all().map_err(|e| io("memory_slot_sync", e))?;
    }
    match read_slot(&path, &data.key)? {
        Slot::Valid(actual, value) if actual == revision && value == *data => Ok(revision),
        _ => Err(MemoryError::Io {
            stage: "memory_slot_verify",
            code: VERIFY_MISMATCH,
        }),
    }
}
/// Persist a tombstone. Old queued saves cannot revive an explicitly reset memory.
pub fn reset(root: &Path, key: &str, keep_facts: bool) -> Result<u64, MemoryError> {
    let previous = load(root, key)?;
    let mut next = SessionMemory::empty(key);
    if keep_facts {
        if let Some(previous) = previous.data {
            next.facts = previous.facts;
        }
    }
    next.updated_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |v| v.as_secs());
    let revision = save(root, previous.revision, &next)?;
    // Acknowledged reset must survive either individual slot becoming corrupt.
    // Mirror the tombstone into the other slot before reporting success.
    save(root, revision, &next)
}
