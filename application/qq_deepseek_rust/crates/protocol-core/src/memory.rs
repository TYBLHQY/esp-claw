//! Bounded conversation memory records. Storage ownership stays in the SD worker.
use serde::{Deserialize, Serialize};
use std::fmt;

pub const MEMORY_VERSION: u8 = 1;
pub const MAX_MEMORY_KEY_BYTES: usize = 256;
pub const MAX_MEMORY_TURNS: usize = 8;
pub const MAX_SUMMARY_BYTES: usize = 4096;
pub const MAX_MEMORY_FACTS: usize = 16;
pub const MAX_FACT_BYTES: usize = 512;
pub const MAX_TURNS_BYTES: usize = 32 * 1024;
pub const MAX_PAYLOAD_BYTES: usize = 48 * 1024;
pub const MAX_PERSISTED_SESSIONS: usize = 32;
const MAGIC: &[u8; 8] = b"QQMEM001";
const HEADER_BYTES: usize = 24;
pub const MAX_RECORD_BYTES: usize = MAX_PAYLOAD_BYTES + HEADER_BYTES;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryTurn {
    pub user: String,
    pub assistant: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionMemory {
    pub version: u8,
    pub key: String,
    pub summary: String,
    pub facts: Vec<String>,
    pub turns: Vec<MemoryTurn>,
    pub summarized_turns: u64,
    pub updated_unix: u64,
}
impl SessionMemory {
    pub fn empty(key: &str) -> Self {
        Self {
            version: MEMORY_VERSION,
            key: key.to_owned(),
            summary: String::new(),
            facts: Vec::new(),
            turns: Vec::new(),
            summarized_turns: 0,
            updated_unix: 0,
        }
    }

    pub fn validate(&self) -> Result<(), MemoryError> {
        if self.version != MEMORY_VERSION || !valid_key(&self.key) {
            return Err(MemoryError::Invalid);
        }
        if self.summary.len() > MAX_SUMMARY_BYTES
            || self.facts.len() > MAX_MEMORY_FACTS
            || self.turns.len() > MAX_MEMORY_TURNS
            || self
                .facts
                .iter()
                .any(|v| v.trim().is_empty() || v.len() > MAX_FACT_BYTES)
            || self
                .turns
                .iter()
                .any(|v| v.user.len() > 8 * 1024 || v.assistant.len() > 16 * 1024)
        {
            return Err(MemoryError::Capacity);
        }
        let turns = serde_json::to_vec(&self.turns).map_err(|_| MemoryError::Invalid)?;
        if turns.len() > MAX_TURNS_BYTES {
            return Err(MemoryError::Capacity);
        }
        let bytes = serde_json::to_vec(self).map_err(|_| MemoryError::Invalid)?;
        if bytes.len() > MAX_PAYLOAD_BYTES {
            return Err(MemoryError::Capacity);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedMemory {
    pub revision: u64,
    pub data: Option<SessionMemory>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MemoryError {
    Unavailable,
    Busy,
    Corrupt,
    Conflict,
    Invalid,
    Capacity,
    Io {
        stage: &'static str,
        code: i32,
    },
    /// The operation may have completed. Reload before issuing a new mutation.
    Timeout,
}
impl fmt::Display for MemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => f.write_str("memory storage unavailable"),
            Self::Busy => f.write_str("memory storage busy"),
            Self::Corrupt => f.write_str("memory record corrupt; existing files preserved"),
            Self::Conflict => f.write_str("memory revision or key conflict; reload required"),
            Self::Invalid => f.write_str("invalid memory record"),
            Self::Capacity => f.write_str("memory capacity limit"),
            Self::Io { stage, code } => write!(f, "memory I/O stage={stage} code={code}"),
            Self::Timeout => {
                f.write_str("memory operation deadline; result unknown; reload after SD recovery")
            }
        }
    }
}
impl std::error::Error for MemoryError {}

pub fn valid_key(key: &str) -> bool {
    if key.len() > MAX_MEMORY_KEY_BYTES {
        return false;
    }
    let valid_id = |id: &str| {
        !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|v| v.is_ascii_alphanumeric() || matches!(v, b'_' | b'-'))
    };
    if let Some(id) = key.strip_prefix("u:") {
        valid_id(id)
    } else if let Some(rest) = key.strip_prefix("g:") {
        rest.split_once(':')
            .is_some_and(|(group, author)| valid_id(group) && valid_id(author))
    } else {
        false
    }
}

fn checksum(revision: &[u8], payload: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for byte in revision.iter().chain(payload) {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320_u32 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

pub fn encode(data: &SessionMemory, revision: u64) -> Result<Vec<u8>, MemoryError> {
    data.validate()?;
    if revision == 0 {
        return Err(MemoryError::Invalid);
    }
    let payload = serde_json::to_vec(data).map_err(|_| MemoryError::Invalid)?;
    let revision = revision.to_le_bytes();
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(HEADER_BYTES + payload.len())
        .map_err(|_| MemoryError::Capacity)?;
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&revision);
    bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&checksum(&revision, &payload).to_le_bytes());
    bytes.extend_from_slice(&payload);
    Ok(bytes)
}

pub fn decode(bytes: &[u8], key: &str) -> Result<(u64, SessionMemory), MemoryError> {
    if !valid_key(key) {
        return Err(MemoryError::Invalid);
    }
    if bytes.len() < HEADER_BYTES || bytes.len() > MAX_RECORD_BYTES || &bytes[..8] != MAGIC {
        return Err(MemoryError::Corrupt);
    }
    let revision = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    let length = u32::from_le_bytes(bytes[16..20].try_into().unwrap()) as usize;
    let crc = u32::from_le_bytes(bytes[20..24].try_into().unwrap());
    if revision == 0
        || length > MAX_PAYLOAD_BYTES
        || bytes.len() != HEADER_BYTES + length
        || checksum(&bytes[8..16], &bytes[HEADER_BYTES..]) != crc
    {
        return Err(MemoryError::Corrupt);
    }
    let data: SessionMemory =
        serde_json::from_slice(&bytes[HEADER_BYTES..]).map_err(|_| MemoryError::Corrupt)?;
    data.validate().map_err(|_| MemoryError::Corrupt)?;
    if data.key != key {
        return Err(MemoryError::Conflict);
    }
    Ok((revision, data))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn utf8_roundtrip_and_revision_are_checked() {
        let mut data = SessionMemory::empty("g:group:user");
        data.summary = "记得用户偏好中文。Résumé in French.".into();
        data.facts.push("Name: Alice".into());
        data.turns.push(MemoryTurn {
            user: "你好".into(),
            assistant: "Hello!".into(),
        });
        let bytes = encode(&data, 7).unwrap();
        assert_eq!(decode(&bytes, &data.key), Ok((7, data.clone())));
        for end in 0..bytes.len() {
            assert_eq!(decode(&bytes[..end], &data.key), Err(MemoryError::Corrupt));
        }
        let mut bad = bytes.clone();
        bad[8] ^= 1;
        assert_eq!(decode(&bad, &data.key), Err(MemoryError::Corrupt));
        let mut bad = bytes;
        bad[30] ^= 1;
        assert_eq!(decode(&bad, &data.key), Err(MemoryError::Corrupt));
    }
    #[test]
    fn invalid_keys_and_capacity_do_not_reach_storage() {
        for key in [
            "",
            "../../key",
            "u:../bad",
            "g:group:user:extra",
            "g::user",
            "u:用户",
        ] {
            assert!(!valid_key(key));
        }
        let mut data = SessionMemory::empty("u:owner");
        data.summary = "a".repeat(MAX_SUMMARY_BYTES + 1);
        assert_eq!(data.validate(), Err(MemoryError::Capacity));
        data.summary.clear();
        data.facts = vec!["fact".into(); MAX_MEMORY_FACTS + 1];
        assert_eq!(data.validate(), Err(MemoryError::Capacity));
        data.facts.clear();
        data.turns = vec![
            MemoryTurn {
                user: "u".into(),
                assistant: "a".into()
            };
            MAX_MEMORY_TURNS + 1
        ];
        assert_eq!(data.validate(), Err(MemoryError::Capacity));
    }
    #[test]
    fn valid_record_for_another_key_is_never_accepted() {
        let data = SessionMemory::empty("u:other");
        assert_eq!(
            decode(&encode(&data, 1).unwrap(), "u:owner"),
            Err(MemoryError::Conflict)
        );
    }
}
