//! Inference-owned working memory. The SD client transfers all I/O to its sole owner.
use crate::platform::sdcard::MemoryClient;
use qq_deepseek_core::{
    memory::{MemoryError, SessionMemory, MAX_FACT_BYTES, MAX_MEMORY_FACTS},
    memory_policy,
};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicU32, Ordering},
        Mutex,
    },
};

const MAX_CACHED_SESSIONS: usize = 8;
const RAM_CACHE_BYTES: usize = 48 * 1024;

#[derive(Default)]
pub struct Stats {
    pub restored: AtomicU32,
    pub saved: AtomicU32,
    pub save_failed: AtomicU32,
    pub summary_calls: AtomicU32,
    pub summary_completed: AtomicU32,
    pub summary_failed: AtomicU32,
    pub cache_evictions: AtomicU32,
    pub unsaved_evictions: AtomicU32,
    pub history_saved: AtomicU32,
    pub history_failed: AtomicU32,
    pub tool_calls: AtomicU32,
    pub search_calls: AtomicU32,
    pub fetch_calls: AtomicU32,
    pub recall_calls: AtomicU32,
    last_error: Mutex<Option<String>>,
}
impl Stats {
    pub fn error(&self, error: MemoryError) {
        if let Ok(mut slot) = self.last_error.lock() {
            *slot = Some(error.to_string());
        }
    }
    pub fn status(&self) -> Value {
        json!({"identity":"CopilotLee","language":"auto","restored":self.restored.load(Ordering::Relaxed),
            "saved":self.saved.load(Ordering::Relaxed),"save_failed":self.save_failed.load(Ordering::Relaxed),
            "summary_calls":self.summary_calls.load(Ordering::Relaxed),"summary_completed":self.summary_completed.load(Ordering::Relaxed),
            "summary_failed":self.summary_failed.load(Ordering::Relaxed),"cache_evictions":self.cache_evictions.load(Ordering::Relaxed),
            "unsaved_evictions":self.unsaved_evictions.load(Ordering::Relaxed),"ram_cache_budget_bytes":RAM_CACHE_BYTES,
            "history_saved":self.history_saved.load(Ordering::Relaxed),"history_failed":self.history_failed.load(Ordering::Relaxed),
            "tool_calls":self.tool_calls.load(Ordering::Relaxed),
            "search_calls":self.search_calls.load(Ordering::Relaxed),"fetch_calls":self.fetch_calls.load(Ordering::Relaxed),"recall_calls":self.recall_calls.load(Ordering::Relaxed),
            "history_window_turns":qq_deepseek_core::history::HISTORY_SLOTS,"history_hits_limit":qq_deepseek_core::history::MAX_HITS,
            "last_error":self.last_error.lock().ok().and_then(|s|s.clone())})
    }
}

pub struct Entry {
    pub data: SessionMemory,
    pub revision: u64,
    pub dirty: bool,
    loaded: bool,
    writable: bool,
    pub storage_error: Option<MemoryError>,
}
pub struct Cache {
    client: Option<MemoryClient>,
    entries: VecDeque<Entry>,
}
impl Cache {
    pub fn new(client: Option<MemoryClient>) -> Self {
        Self {
            client,
            entries: VecDeque::with_capacity(MAX_CACHED_SESSIONS),
        }
    }
    pub fn take(&mut self, key: &str, stats: &Stats) -> Entry {
        let mut entry = self
            .entries
            .iter()
            .position(|e| e.data.key == key)
            .and_then(|n| self.entries.remove(n))
            .unwrap_or_else(|| Entry {
                data: SessionMemory::empty(key),
                revision: 0,
                dirty: false,
                loaded: false,
                writable: true,
                storage_error: None,
            });
        if !entry.loaded && entry.writable {
            match self
                .client
                .as_ref()
                .ok_or(MemoryError::Unavailable)
                .and_then(|c| c.load(key))
            {
                Ok(loaded) => {
                    // Never silently replace offline turns with a different stored conversation.
                    if entry.dirty && loaded.data.is_some() {
                        entry.writable = false;
                        entry.storage_error = Some(MemoryError::Conflict);
                        stats.error(MemoryError::Conflict);
                    } else {
                        if let Some(data) = loaded.data {
                            entry.data = data;
                            stats.restored.fetch_add(1, Ordering::Relaxed);
                        }
                        entry.revision = loaded.revision;
                        entry.loaded = true;
                        entry.storage_error = None;
                    }
                }
                Err(error) => {
                    entry.storage_error = Some(error);
                    stats.error(error);
                    if matches!(
                        error,
                        MemoryError::Corrupt | MemoryError::Conflict | MemoryError::Timeout
                    ) {
                        entry.writable = false;
                    }
                }
            }
        }
        entry
    }
    pub fn persist(&self, entry: &mut Entry, stats: &Stats) {
        if !entry.dirty {
            return;
        }
        let result = if !entry.loaded || !entry.writable {
            Err(entry.storage_error.unwrap_or(MemoryError::Unavailable))
        } else {
            self.client
                .as_ref()
                .ok_or(MemoryError::Unavailable)
                .and_then(|c| c.save(entry.revision, entry.data.clone()))
        };
        match result {
            Ok(revision) => {
                entry.revision = revision;
                entry.dirty = false;
                entry.storage_error = None;
                stats.saved.fetch_add(1, Ordering::Relaxed);
            }
            Err(error) => {
                entry.storage_error = Some(error);
                stats.error(error);
                stats.save_failed.fetch_add(1, Ordering::Relaxed);
                if matches!(
                    error,
                    MemoryError::Conflict | MemoryError::Corrupt | MemoryError::Timeout
                ) {
                    entry.writable = false;
                }
            }
        }
    }
    pub fn archive(&self, entry: &Entry, stats: &Stats) {
        let result = if !persisted(entry) {
            Err(entry.storage_error.unwrap_or(MemoryError::Unavailable))
        } else {
            self.client
                .as_ref()
                .ok_or(MemoryError::Unavailable)
                .and_then(|client| {
                    let turn = entry.data.turns.last().ok_or(MemoryError::Invalid)?;
                    client.archive(&entry.data.key, entry.revision, turn)
                })
        };
        match result {
            Ok(()) => {
                stats.history_saved.fetch_add(1, Ordering::Relaxed);
            }
            Err(error) => {
                stats.history_failed.fetch_add(1, Ordering::Relaxed);
                stats.error(error);
            }
        }
    }
    pub fn search_history(
        &self,
        entry: &Entry,
        query: &str,
    ) -> Result<qq_deepseek_core::history::SearchResult, MemoryError> {
        if !entry.loaded || !entry.writable {
            return Err(entry.storage_error.unwrap_or(MemoryError::Unavailable));
        }
        self.client
            .as_ref()
            .ok_or(MemoryError::Unavailable)?
            .search_history(&entry.data.key, query)
    }
    pub fn reset(&self, entry: &mut Entry, keep_facts: bool, stats: &Stats) {
        let preserve_unsaved_facts = keep_facts && entry.dirty && entry.loaded && entry.writable;
        entry.data.summary.clear();
        entry.data.turns.clear();
        entry.data.summarized_turns = 0;
        if !keep_facts {
            entry.data.facts.clear();
        }
        entry.dirty = true;
        let result = self
            .client
            .as_ref()
            .ok_or(MemoryError::Unavailable)
            .and_then(|client| {
                let revision = client.reset(&entry.data.key, keep_facts)?;
                let loaded = client.load(&entry.data.key)?;
                if loaded.revision != revision {
                    return Err(MemoryError::Conflict);
                }
                let mut data = loaded.data.ok_or(MemoryError::Corrupt)?;
                let mut revision = revision;
                if preserve_unsaved_facts && data.facts != entry.data.facts {
                    // A reset preserves disk facts, which can differ from an offline
                    // working copy. Do not declare that working copy saved or discard it.
                    data.facts = entry.data.facts.clone();
                    data.updated_unix = entry.data.updated_unix;
                    data.validate()?;
                    revision = client.save(revision, data.clone())?;
                    revision = client.save(revision, data.clone())?;
                }
                Ok((revision, data))
            });
        match result {
            Ok((revision, data)) => {
                entry.data = data;
                entry.revision = revision;
                entry.dirty = false;
                entry.loaded = true;
                entry.writable = true;
                entry.storage_error = None;
                stats.saved.fetch_add(1, Ordering::Relaxed);
            }
            Err(error) => {
                // Keep the local clear, but never claim that an unavailable card was erased.
                entry.storage_error = Some(error);
                entry.writable = false;
                stats.error(error);
                stats.save_failed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    /// Privacy-sensitive deletion must reach both slots before it is confirmed.
    pub fn persist_redundant(&self, entry: &mut Entry, stats: &Stats) {
        self.persist(entry, stats);
        if !entry.dirty && entry.loaded && entry.writable {
            entry.dirty = true;
            self.persist(entry, stats);
        }
    }
    pub fn put(&mut self, entry: Entry, stats: &Stats) {
        self.entries.push_back(entry);
        while self.entries.len() > MAX_CACHED_SESSIONS
            || (self.entries.len() > 1
                && self.entries.iter().map(entry_bytes).sum::<usize>() > RAM_CACHE_BYTES)
        {
            // Disk records are never removed by RAM eviction. Without SD the explicitly
            // bounded fallback can evict unsaved context, which is exposed in status.
            let victim = self.entries.iter().position(|e| !e.dirty).unwrap_or(0);
            if let Some(old) = self.entries.remove(victim) {
                stats.cache_evictions.fetch_add(1, Ordering::Relaxed);
                if old.dirty {
                    stats.unsaved_evictions.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}
fn entry_bytes(entry: &Entry) -> usize {
    entry.data.summary.len()
        + entry.data.facts.iter().map(String::len).sum::<usize>()
        + entry
            .data
            .turns
            .iter()
            .map(|t| t.user.len() + t.assistant.len() + 64)
            .sum::<usize>()
}
pub fn preview(entry: &Entry) -> Value {
    let mut value = memory_policy::preview(&entry.data);
    value["revision"] = entry.revision.into();
    value["persisted"] = persisted(entry).into();
    value["storage_error"] = entry.storage_error.map(|e| e.to_string()).into();
    value
}
pub fn persisted(entry: &Entry) -> bool {
    entry.revision > 0 && !entry.dirty && entry.loaded && entry.storage_error.is_none()
}
/// UART inference diagnostics return metadata, never an entire private history.
pub fn metadata(entry: &Entry) -> Value {
    json!({"revision":entry.revision,"persisted":persisted(entry),"turn_count":entry.data.turns.len(),
        "summary_bytes":entry.data.summary.len(),"facts_count":entry.data.facts.len(),
        "summarized_turns":entry.data.summarized_turns,"storage_error":entry.storage_error.map(|e|e.to_string())})
}
/// Returns None for normal model input. Memory commands never need an extra model call.
pub fn simple_command(
    cache: &Cache,
    entry: &mut Entry,
    text: &str,
    stats: &Stats,
) -> Option<Result<Value, &'static str>> {
    let text = text.trim();
    if matches!(text, "/memory" | "/memory show" | "memory show") {
        return Some(Ok(preview(entry)));
    }
    if matches!(
        text,
        "/reset" | "/memory clear" | "memory clear" | "/forget all" | "memory forget all"
    ) {
        cache.reset(entry, !text.ends_with("forget all"), stats);
        return Some(Ok(
            json!({"cleared":true,"facts_kept":!text.ends_with("forget all"),"persisted":persisted(entry),"storage_error":entry.storage_error.map(|e|e.to_string())}),
        ));
    }
    let remember = text
        .strip_prefix("/remember ")
        .or_else(|| text.strip_prefix("memory remember "));
    if let Some(fact) = remember {
        let fact = fact.trim();
        if fact.is_empty() || fact.len() > MAX_FACT_BYTES {
            return Some(Err("Memory fact must contain 1..512 UTF-8 bytes."));
        }
        if !entry.data.facts.iter().any(|f| f == fact) {
            if entry.data.facts.len() >= MAX_MEMORY_FACTS {
                return Some(Err("Memory fact limit reached; forget an item first."));
            }
            let mut candidate = entry.data.clone();
            candidate.facts.push(fact.into());
            candidate.updated_unix = super::unix_now().unwrap_or(0);
            if candidate.validate().is_err() {
                return Some(Err("Memory capacity limit; existing memory was retained."));
            }
            entry.data = candidate;
            entry.dirty = true;
            cache.persist(entry, stats);
        }
        return Some(Ok(preview(entry)));
    }
    let forget = text
        .strip_prefix("/forget ")
        .or_else(|| text.strip_prefix("memory forget "));
    if let Some(index) = forget {
        let Ok(index) = index.trim().parse::<usize>() else {
            return Some(Err("Use /forget INDEX or /forget all."));
        };
        if index == 0 || index > entry.data.facts.len() {
            return Some(Err("Memory fact index is out of range."));
        }
        entry.data.facts.remove(index - 1);
        entry.data.updated_unix = super::unix_now().unwrap_or(0);
        entry.dirty = true;
        cache.persist_redundant(entry, stats);
        return Some(Ok(preview(entry)));
    }
    None
}

pub fn is_command(text: &str) -> bool {
    let text = text.trim();
    matches!(
        text,
        "/memory" | "/memory show" | "/reset" | "/memory clear" | "/memory compact"
    ) || text.starts_with("/remember ")
        || text.starts_with("/forget ")
        || text.starts_with("/memory search ")
}
