//! Bounded, host-testable protocols shared by the ESP firmware and provisioning.
//!
//! No GPIO, storage, executor, or network ownership belongs in this crate.

pub mod command;
pub mod config;
pub mod record;
pub mod sse;
pub mod web_tools;

pub use command::{
    parse_command, Command, CommandError, LedCommand, Rgb, WifiCommand, MAX_LED_BRIGHTNESS,
};
pub use config::{Config, ConfigError, ConfigStatus, CONFIG_VERSION};
pub use record::{decode_config, encode_config, RecordError, MAX_RECORD_BYTES};
pub use sse::{Completion, SseDecoder, SseError, SseLimits};
pub mod config_patch;
pub mod uart;

pub mod history;
pub mod memory;
pub mod memory_policy;
pub mod privacy;
pub mod tool_calls;

pub mod material;
