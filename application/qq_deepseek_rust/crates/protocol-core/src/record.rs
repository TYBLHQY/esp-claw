//! Integrity envelope for independent NVS configuration slots.
//!
//! CRC detects accidental corruption; it does not encrypt or authenticate.
//! The device storage owner decides which validated generation is active.

use core::fmt;
use std::io::{self, Write};

use crate::config::{Config, ConfigError, MAX_CONFIG_BYTES};

pub const MAX_RECORD_BYTES: usize = 6144;
pub const RECORD_HEADER_BYTES: usize = 12;
const MAGIC: &[u8; 4] = b"QD01";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordError {
    TooLarge,
    InvalidHeader,
    InvalidLength,
    ChecksumMismatch,
    InvalidConfig(ConfigError),
    AllocationFailed,
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => f.write_str("config_record_too_large"),
            Self::InvalidHeader => f.write_str("invalid_config_record_header"),
            Self::InvalidLength => f.write_str("invalid_config_record_length"),
            Self::ChecksumMismatch => f.write_str("config_record_checksum_mismatch"),
            Self::InvalidConfig(error) => write!(f, "config_record_{error}"),
            Self::AllocationFailed => f.write_str("config_record_allocation_failed"),
        }
    }
}

impl std::error::Error for RecordError {}

pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

struct BoundedPayload {
    bytes: Vec<u8>,
    allocation_failed: bool,
}

impl Write for BoundedPayload {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let next_len = self.bytes.len().checked_add(buf.len());
        if next_len.is_none_or(|len| len > MAX_CONFIG_BYTES) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "config_too_large",
            ));
        }
        if self.bytes.try_reserve(buf.len()).is_err() {
            self.allocation_failed = true;
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "allocation_failed",
            ));
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn encode_config(config: &Config) -> Result<Vec<u8>, RecordError> {
    config.validate().map_err(RecordError::InvalidConfig)?;
    let mut payload = BoundedPayload {
        bytes: Vec::new(),
        allocation_failed: false,
    };
    if serde_json::to_writer(&mut payload, config).is_err() {
        return Err(if payload.allocation_failed {
            RecordError::AllocationFailed
        } else {
            RecordError::TooLarge
        });
    }
    let total_len = RECORD_HEADER_BYTES + payload.bytes.len();
    if total_len > MAX_RECORD_BYTES {
        return Err(RecordError::TooLarge);
    }
    let mut record = Vec::new();
    record
        .try_reserve_exact(total_len)
        .map_err(|_| RecordError::AllocationFailed)?;
    record.extend_from_slice(MAGIC);
    record.extend_from_slice(&(payload.bytes.len() as u32).to_le_bytes());
    record.extend_from_slice(&crc32(&payload.bytes).to_le_bytes());
    record.extend_from_slice(&payload.bytes);
    Ok(record)
}

pub fn decode_config(record: &[u8]) -> Result<Box<Config>, RecordError> {
    if record.len() > MAX_RECORD_BYTES {
        return Err(RecordError::TooLarge);
    }
    if record.len() < RECORD_HEADER_BYTES {
        return Err(RecordError::InvalidLength);
    }
    if &record[..4] != MAGIC {
        return Err(RecordError::InvalidHeader);
    }
    let payload_len = u32::from_le_bytes(
        record[4..8]
            .try_into()
            .map_err(|_| RecordError::InvalidHeader)?,
    ) as usize;
    if payload_len > MAX_CONFIG_BYTES || record.len() - RECORD_HEADER_BYTES != payload_len {
        return Err(RecordError::InvalidLength);
    }
    let expected_crc = u32::from_le_bytes(
        record[8..12]
            .try_into()
            .map_err(|_| RecordError::InvalidHeader)?,
    );
    let payload = &record[RECORD_HEADER_BYTES..];
    if crc32(payload) != expected_crc {
        return Err(RecordError::ChecksumMismatch);
    }
    Config::from_json_boxed(payload).map_err(RecordError::InvalidConfig)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CONFIG_VERSION;

    fn config() -> Config {
        Config {
            version: CONFIG_VERSION,
            generation: 42,
            wifi_ssid: "test-network".into(),
            wifi_password: "fake-passphrase".into(),
            deepseek_key: String::new(),
            deepseek_model: String::new(),
            qq_app_id: String::new(),
            qq_app_secret: String::new(),
            allow_users: vec![],
            allow_groups: vec![],
        }
    }

    #[test]
    fn preserves_generation_and_detects_every_truncation() {
        let record = encode_config(&config()).unwrap();
        assert_eq!(decode_config(&record).unwrap().generation, 42);
        for length in 0..record.len() {
            assert!(decode_config(&record[..length]).is_err());
        }
    }

    #[test]
    fn detects_bit_flip_header_and_length_corruption() {
        let record = encode_config(&config()).unwrap();
        let mut damaged = record.clone();
        damaged[RECORD_HEADER_BYTES + 5] ^= 1;
        assert!(matches!(
            decode_config(&damaged),
            Err(RecordError::ChecksumMismatch)
        ));
        let mut damaged = record.clone();
        damaged[0] ^= 1;
        assert!(matches!(
            decode_config(&damaged),
            Err(RecordError::InvalidHeader)
        ));
        let mut damaged = record;
        damaged[4] ^= 1;
        assert!(matches!(
            decode_config(&damaged),
            Err(RecordError::InvalidLength)
        ));
    }

    #[test]
    fn refuses_invalid_versions_and_large_input_before_parse() {
        let mut config = config();
        config.version = 0;
        assert!(matches!(
            encode_config(&config),
            Err(RecordError::InvalidConfig(ConfigError::UnsupportedVersion))
        ));
        let large = vec![0; MAX_RECORD_BYTES + 1];
        assert!(matches!(decode_config(&large), Err(RecordError::TooLarge)));
    }

    #[test]
    fn crc_matches_known_standard_vector() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }
}
