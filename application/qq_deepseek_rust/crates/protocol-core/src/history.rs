//! Fixed-size SD history window and bounded keyword ranking; no resident index.
use crate::{
    memory::{valid_key, MemoryError, MemoryTurn},
    record::crc32,
};
use serde::{Deserialize, Serialize};

pub const HISTORY_SLOTS: u64 = 32;
pub const USER_BYTES: usize = 1024;
pub const ASSISTANT_BYTES: usize = 2048;
pub const MAX_RECORD_BYTES: usize = 36 + 256 + USER_BYTES + ASSISTANT_BYTES;
pub const MAX_HITS: usize = 3;
pub const SNIPPET_BYTES: usize = 768;
const MAGIC: &[u8; 8] = b"QQHIST01";

pub fn clip(text: &str, bytes: usize) -> &str {
    let mut end = bytes.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchivedTurn {
    pub sequence: u64,
    pub unix: u64,
    pub turn: MemoryTurn,
    pub truncated: bool,
}
pub fn encode(
    key: &str,
    sequence: u64,
    unix: u64,
    turn: &MemoryTurn,
) -> Result<Vec<u8>, MemoryError> {
    if !valid_key(key) || sequence == 0 {
        return Err(MemoryError::Invalid);
    }
    let user = clip(&turn.user, USER_BYTES);
    let assistant = clip(&turn.assistant, ASSISTANT_BYTES);
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(36 + key.len() + user.len() + assistant.len())
        .map_err(|_| MemoryError::Capacity)?;
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&sequence.to_le_bytes());
    bytes.extend_from_slice(&unix.to_le_bytes());
    for n in [
        key.len(),
        user.len(),
        assistant.len(),
        usize::from(user.len() < turn.user.len() || assistant.len() < turn.assistant.len()),
    ] {
        bytes.extend_from_slice(&(n as u16).to_le_bytes());
    }
    bytes.extend_from_slice(key.as_bytes());
    bytes.extend_from_slice(user.as_bytes());
    bytes.extend_from_slice(assistant.as_bytes());
    let crc = crc32(&bytes);
    bytes.extend_from_slice(&crc.to_le_bytes());
    Ok(bytes)
}
pub fn decode(bytes: &[u8], key: &str) -> Result<ArchivedTurn, MemoryError> {
    if bytes.len() < 36 || bytes.len() > MAX_RECORD_BYTES || &bytes[..8] != MAGIC {
        return Err(MemoryError::Corrupt);
    }
    let size = bytes.len();
    if crc32(&bytes[..size - 4]) != u32::from_le_bytes(bytes[size - 4..].try_into().unwrap()) {
        return Err(MemoryError::Corrupt);
    }
    let sequence = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    let unix = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
    let n = |at| u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap()) as usize;
    let (k, u, a, flags) = (n(24), n(26), n(28), n(30));
    if sequence == 0
        || k > 256
        || u > USER_BYTES
        || a > ASSISTANT_BYTES
        || flags > 1
        || size != 36 + k + u + a
    {
        return Err(MemoryError::Corrupt);
    }
    let stored_key = std::str::from_utf8(&bytes[32..32 + k]).map_err(|_| MemoryError::Corrupt)?;
    if stored_key != key {
        return Err(MemoryError::Conflict);
    }
    Ok(ArchivedTurn {
        sequence,
        unix,
        truncated: flags != 0,
        turn: MemoryTurn {
            user: std::str::from_utf8(&bytes[32 + k..32 + k + u])
                .map_err(|_| MemoryError::Corrupt)?
                .into(),
            assistant: std::str::from_utf8(&bytes[32 + k + u..size - 4])
                .map_err(|_| MemoryError::Corrupt)?
                .into(),
        },
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Index {
    pub key: String,
    pub revision: u64,
    pub next: u64,
    pub floor: u64,
    pub source_revision: u64,
}
impl Index {
    pub fn empty(key: &str) -> Self {
        Self {
            key: key.into(),
            revision: 0,
            next: 1,
            floor: 1,
            source_revision: 0,
        }
    }
    pub fn validate(&self, key: &str) -> Result<(), MemoryError> {
        if self.key != key {
            return Err(MemoryError::Conflict);
        }
        if !valid_key(key) || self.next == 0 || self.floor == 0 || self.floor > self.next {
            return Err(MemoryError::Corrupt);
        }
        Ok(())
    }
}
pub fn validate_query(query: &str) -> Result<(), MemoryError> {
    if query.trim().is_empty() || query.chars().count() > 128 || query.chars().any(char::is_control)
    {
        Err(MemoryError::Invalid)
    } else {
        Ok(())
    }
}
#[derive(Debug, Serialize)]
pub struct Hit {
    pub sequence: u64,
    pub unix: u64,
    pub score: usize,
    pub snippet: String,
    pub truncated: bool,
}
#[derive(Default, Debug, Serialize)]
pub struct SearchResult {
    pub hits: Vec<Hit>,
    pub scanned: usize,
    pub damaged: usize,
    pub window_turns: u64,
}
pub struct Ranker {
    terms: Vec<String>,
    result: SearchResult,
}
impl Ranker {
    pub fn new(query: &str) -> Result<Self, MemoryError> {
        validate_query(query)?;
        let mut terms = Vec::new();
        // English words plus overlapping CJK pairs. Query and index remain tiny.
        for word in query.split(|c: char| !c.is_alphanumeric()) {
            let word = word.to_lowercase();
            if word.is_ascii() {
                if !word.is_empty() {
                    terms.push(clip(&word, 64).to_owned());
                }
            } else {
                let chars: Vec<char> = word.chars().take(129).collect();
                if chars.len() == 1 {
                    terms.push(word);
                } else {
                    for pair in chars.windows(2) {
                        terms.push(pair.iter().collect());
                        if terms.len() >= 16 {
                            break;
                        }
                    }
                }
            }
            if terms.len() >= 16 {
                break;
            }
        }
        terms.truncate(16);
        terms.sort();
        terms.dedup();
        if terms.is_empty() {
            return Err(MemoryError::Invalid);
        }
        Ok(Self {
            terms,
            result: SearchResult {
                window_turns: HISTORY_SLOTS,
                ..SearchResult::default()
            },
        })
    }
    pub fn damaged(&mut self) {
        self.result.damaged += 1;
    }
    pub fn consider(&mut self, record: ArchivedTurn) {
        self.result.scanned += 1;
        let text = format!(
            "User: {}\nAssistant: {}",
            record.turn.user, record.turn.assistant
        );
        let lower = text.to_lowercase();
        let score = self
            .terms
            .iter()
            .filter(|q| lower.contains(q.as_str()))
            .count();
        if score == 0 {
            return;
        }
        // Find in original text to keep UTF-8 offsets valid after case folding.
        let position = self
            .terms
            .iter()
            .filter_map(|q| text.find(q))
            .min()
            .unwrap_or(0);
        let mut start = position.saturating_sub(96);
        while !text.is_char_boundary(start) {
            start -= 1;
        }
        let snippet = clip(&text[start..], SNIPPET_BYTES).to_owned();
        let hit = Hit {
            sequence: record.sequence,
            unix: record.unix,
            score,
            truncated: record.truncated || start > 0 || start + snippet.len() < text.len(),
            snippet,
        };
        self.result.hits.push(hit);
        self.result
            .hits
            .sort_by(|a, b| b.score.cmp(&a.score).then(b.sequence.cmp(&a.sequence)));
        self.result.hits.truncate(MAX_HITS);
    }
    pub fn finish(self) -> SearchResult {
        self.result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_utf8_crc_and_key_isolation() {
        let turn = MemoryTurn {
            user: "彗星".repeat(1000),
            assistant: "秘密资料".repeat(2000),
        };
        let wire = encode("u:alice", 1, 3, &turn).unwrap();
        assert!(wire.len() <= MAX_RECORD_BYTES);
        let value = decode(&wire, "u:alice").unwrap();
        assert!(value.truncated);
        assert!(value.turn.user.len() <= USER_BYTES);
        assert_eq!(decode(&wire, "u:bob"), Err(MemoryError::Conflict));
        for n in 0..wire.len() {
            assert!(decode(&wire[..n], "u:alice").is_err());
        }
    }
    #[test]
    fn ranks_multilingual_keywords_without_loading_full_history() {
        let mut rank = Ranker::new("彗星 cedar").unwrap();
        for sequence in 1..=40 {
            rank.consider(ArchivedTurn {
                sequence,
                unix: 0,
                truncated: false,
                turn: MemoryTurn {
                    user: if sequence % 2 == 0 {
                        "彗星 Cedar 方案"
                    } else {
                        "普通聊天"
                    }
                    .into(),
                    assistant: "ok".into(),
                },
            });
        }
        let r = rank.finish();
        assert_eq!(r.scanned, 40);
        assert_eq!(r.hits.len(), MAX_HITS);
        assert_eq!(r.hits[0].sequence, 40);
        assert!(r.hits.iter().all(|h| h.snippet.len() <= SNIPPET_BYTES));
    }
}
