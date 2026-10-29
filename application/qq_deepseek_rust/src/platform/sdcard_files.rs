//! Bounded filesystem operations for the SD owner. No legacy card path is modified.
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::Path,
};

pub const PROBE_BYTES: usize = 64;
pub const MAX_LIST_ENTRIES: usize = 32;
pub const MAX_NAME_BYTES: usize = 128;
pub const MAX_JOURNAL_BYTES: u64 = 64 * 1024;
const JOURNAL_NAME: &str = "events.jsonl";
const VERIFY_MISMATCH: i32 = -1;
const NOT_A_DIRECTORY: i32 = -2;
const UNKNOWN_IO: i32 = -3;

#[derive(Debug)]
pub struct Failure {
    pub stage: &'static str,
    pub code: i32,
    pub writing: bool,
}
impl Failure {
    fn io(stage: &'static str, writing: bool, error: std::io::Error) -> Self {
        Self {
            stage,
            code: error.raw_os_error().unwrap_or(UNKNOWN_IO),
            writing,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum JournalEvent {
    Boot,
}
impl JournalEvent {
    fn name(self) -> &'static str {
        match self {
            Self::Boot => "boot",
        }
    }
}

pub struct TestReport {
    pub bytes: usize,
    pub nonce: u32,
}
pub struct Entry {
    pub name: String,
    pub bytes: u64,
    pub directory: bool,
}
pub struct Listing {
    pub entries: Vec<Entry>,
    pub truncated: bool,
    pub directory_exists: bool,
}
pub struct JournalReport {
    pub bytes: u64,
    pub full: bool,
}

fn ensure_directory(root: &Path) -> Result<(), Failure> {
    match fs::create_dir(root) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let meta = fs::symlink_metadata(root)
                .map_err(|error| Failure::io("app_directory_metadata", false, error))?;
            if meta.is_dir() && !meta.file_type().is_symlink() {
                Ok(())
            } else {
                Err(Failure {
                    stage: "app_directory_type",
                    code: NOT_A_DIRECTORY,
                    writing: false,
                })
            }
        }
        Err(error) => Err(Failure::io("app_directory_create", true, error)),
    }
}

/// A unique, exclusively-created probe is the only file touched by a self-test.
/// On I/O failure it is retained for recovery; do not attempt more writes to a suspect card.
pub fn self_test(root: &Path, nonce: u32) -> Result<TestReport, Failure> {
    ensure_directory(root)?;
    let path = root.join(format!("probe-{nonce:08x}.tmp"));
    let mut data = [0u8; PROBE_BYTES];
    for (index, byte) in data.iter_mut().enumerate() {
        *byte =
            (nonce.rotate_right((index % 32) as u32) as u8) ^ (index as u8).wrapping_mul(31) ^ 0xa5;
    }
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| Failure::io("probe_create_exclusive", true, error))?;
        file.write_all(&data)
            .map_err(|error| Failure::io("probe_write", true, error))?;
        file.sync_all()
            .map_err(|error| Failure::io("probe_sync", true, error))?;
    }
    {
        let mut file =
            File::open(&path).map_err(|error| Failure::io("probe_reopen", false, error))?;
        let mut readback = [0u8; PROBE_BYTES];
        file.read_exact(&mut readback)
            .map_err(|error| Failure::io("probe_read", false, error))?;
        let mut extra = [0u8; 1];
        let eof = file
            .read(&mut extra)
            .map_err(|error| Failure::io("probe_eof", false, error))?;
        if readback != data || eof != 0 {
            return Err(Failure {
                stage: "probe_compare",
                code: VERIFY_MISMATCH,
                writing: false,
            });
        }
    }
    fs::remove_file(path).map_err(|error| Failure::io("probe_delete", true, error))?;
    Ok(TestReport {
        bytes: PROBE_BYTES,
        nonce,
    })
}

pub fn list(root: &Path) -> Result<Listing, Failure> {
    let directory = match fs::read_dir(root) {
        Ok(directory) => directory,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Listing {
                entries: Vec::new(),
                truncated: false,
                directory_exists: false,
            });
        }
        Err(error) => return Err(Failure::io("list_open", false, error)),
    };
    let mut entries = Vec::with_capacity(MAX_LIST_ENTRIES);
    let mut truncated = false;
    // Bound visited entries too: oversized/unsupported names must not let a
    // caller force an unbounded scan while we keep skipping them.
    for (visited, entry) in directory.enumerate() {
        if visited == MAX_LIST_ENTRIES {
            truncated = true;
            break;
        }
        let entry = entry.map_err(|error| Failure::io("list_next", false, error))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.len() > MAX_NAME_BYTES {
            // Never emit a partial UTF-8 name that could later be mistaken for a path.
            truncated = true;
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|error| Failure::io("list_metadata", false, error))?;
        entries.push(Entry {
            name,
            bytes: metadata.len(),
            directory: metadata.is_dir(),
        });
    }
    entries.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    Ok(Listing {
        entries,
        truncated,
        directory_exists: true,
    })
}

/// Diagnostics contain only a fixed event identifier and numeric time, never user text or keys.
/// A full journal stops growing; it is never rotated over or truncated automatically.
pub fn append_event(
    root: &Path,
    unix_seconds: u64,
    event: JournalEvent,
) -> Result<JournalReport, Failure> {
    ensure_directory(root)?;
    let path = root.join(JOURNAL_NAME);
    let current = match fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => meta.len(),
        Ok(_) => {
            return Err(Failure {
                stage: "journal_type",
                code: NOT_A_DIRECTORY,
                writing: false,
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => return Err(Failure::io("journal_metadata", false, error)),
    };
    let row = format!(
        "{{\"unix_time\":{unix_seconds},\"event\":\"{}\"}}\n",
        event.name()
    );
    if current.saturating_add(row.len() as u64) > MAX_JOURNAL_BYTES {
        return Ok(JournalReport {
            bytes: current,
            full: true,
        });
    }
    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .map_err(|error| Failure::io("journal_open", true, error))?;
    file.write_all(row.as_bytes())
        .map_err(|error| Failure::io("journal_write", true, error))?;
    file.sync_all()
        .map_err(|error| Failure::io("journal_sync", true, error))?;
    Ok(JournalReport {
        bytes: current + row.len() as u64,
        full: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    struct Directory(std::path::PathBuf);
    impl Directory {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "qqds-sd-test-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            Self(root)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn self_test_round_trip_removes_only_its_probe() {
        let outer = Directory::new();
        let app = outer.0.join("qq-deepseek");
        fs::write(outer.0.join("legacy.txt"), b"preserve me").unwrap();
        let report = self_test(&app, 0x1234_5678).unwrap();
        assert_eq!(report.bytes, 64);
        assert_eq!(report.nonce, 0x1234_5678);
        assert_eq!(
            fs::read(outer.0.join("legacy.txt")).unwrap(),
            b"preserve me"
        );
        assert_eq!(fs::read_dir(app).unwrap().count(), 0);
    }
    #[test]
    fn probe_collision_never_overwrites_existing_file() {
        let outer = Directory::new();
        fs::write(outer.0.join("probe-00000001.tmp"), b"existing").unwrap();
        let error = self_test(&outer.0, 1).err().unwrap();
        assert_eq!(error.stage, "probe_create_exclusive");
        assert_eq!(
            fs::read(outer.0.join("probe-00000001.tmp")).unwrap(),
            b"existing"
        );
    }
    #[test]
    fn bounded_listing_does_not_create_or_modify_paths() {
        let outer = Directory::new();
        let missing = outer.0.join("missing");
        assert!(!list(&missing).unwrap().directory_exists);
        assert!(!missing.exists());
        for i in 0..35 {
            fs::write(outer.0.join(format!("file-{i}")), b"x").unwrap();
        }
        let listing = list(&outer.0).unwrap();
        assert_eq!(listing.entries.len(), MAX_LIST_ENTRIES);
        assert!(listing.truncated);
    }
    #[test]
    fn journal_uses_only_fixed_events_and_stops_at_its_limit() {
        let outer = Directory::new();
        let report = append_event(&outer.0, 123, JournalEvent::Boot).unwrap();
        assert!(!report.full);
        assert_eq!(
            fs::read_to_string(outer.0.join(JOURNAL_NAME)).unwrap(),
            "{\"unix_time\":123,\"event\":\"boot\"}\n"
        );
        fs::write(
            outer.0.join(JOURNAL_NAME),
            vec![b'x'; MAX_JOURNAL_BYTES as usize],
        )
        .unwrap();
        assert!(
            append_event(&outer.0, 456, JournalEvent::Boot)
                .unwrap()
                .full
        );
        assert_eq!(
            fs::metadata(outer.0.join(JOURNAL_NAME)).unwrap().len(),
            MAX_JOURNAL_BYTES
        );
    }
    #[test]
    fn an_existing_file_cannot_be_replaced_by_the_app_directory() {
        let outer = Directory::new();
        let app = outer.0.join("qq-deepseek");
        fs::write(&app, b"leave intact").unwrap();
        assert_eq!(
            self_test(&app, 2).err().unwrap().stage,
            "app_directory_type"
        );
        assert_eq!(fs::read(app).unwrap(), b"leave intact");
    }
}
