use core::fmt;
use serde::{Deserialize, Serialize};

pub const CONFIG_VERSION: u32 = 1;
pub const MAX_SSID_BYTES: usize = 32;
pub const MAX_WIFI_PASSWORD_BYTES: usize = 63;
pub const MAX_SECRET_BYTES: usize = 512;
pub const MAX_MODEL_BYTES: usize = 96;
pub const MAX_IDENTIFIER_BYTES: usize = 128;
pub const MAX_ALLOW_LIST_ENTRIES: usize = 64;
pub const MAX_CONFIG_BYTES: usize = 4096;

/// Every serialized field is explicit. An older/incomplete record is never
/// silently filled with defaults and used as a valid current configuration.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    pub generation: u32,
    pub wifi_ssid: String,
    pub wifi_password: String,
    pub deepseek_key: String,
    pub deepseek_model: String,
    pub qq_app_id: String,
    pub qq_app_secret: String,
    pub allow_users: Vec<String>,
    pub allow_groups: Vec<String>,
}

/// Safe to return over a diagnostics endpoint; contains no credential values.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ConfigStatus {
    pub version: u32,
    pub generation: u32,
    pub wifi_configured: bool,
    pub wifi_password_present: bool,
    pub deepseek_configured: bool,
    pub qq_configured: bool,
    pub allowed_user_count: usize,
    pub allowed_group_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigError {
    UnsupportedVersion,
    InvalidSsid,
    InvalidWifiPassword,
    InvalidDeepSeekKey,
    InvalidDeepSeekModel,
    InvalidQqAppId,
    InvalidQqSecret,
    IncompleteQqCredentials,
    TooManyAllowedIds,
    InvalidAllowedId,
    TooLarge,
    InvalidJson,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UnsupportedVersion => "unsupported_config_version",
            Self::InvalidSsid => "invalid_wifi_ssid",
            Self::InvalidWifiPassword => "invalid_wifi_password",
            Self::InvalidDeepSeekKey => "invalid_deepseek_key",
            Self::InvalidDeepSeekModel => "invalid_deepseek_model",
            Self::InvalidQqAppId => "invalid_qq_app_id",
            Self::InvalidQqSecret => "invalid_qq_app_secret",
            Self::IncompleteQqCredentials => "incomplete_qq_credentials",
            Self::TooManyAllowedIds => "too_many_allow_list_entries",
            Self::InvalidAllowedId => "invalid_allow_list_identifier",
            Self::TooLarge => "config_too_large",
            Self::InvalidJson => "invalid_config_json",
        })
    }
}

impl std::error::Error for ConfigError {}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

fn bounded_text(value: &str, max: usize) -> bool {
    value.len() <= max && !value.chars().any(char::is_control)
}

impl Config {
    pub fn from_json_boxed(bytes: &[u8]) -> Result<Box<Self>, ConfigError> {
        if bytes.len() > MAX_CONFIG_BYTES {
            return Err(ConfigError::TooLarge);
        }
        let config: Box<Self> =
            serde_json::from_slice(bytes).map_err(|_| ConfigError::InvalidJson)?;
        config.validate()?;
        Ok(config)
    }

    /// Decode only a bounded record. Parser details are intentionally not
    /// returned because serde errors can include parts of credential input.
    pub fn from_json(bytes: &[u8]) -> Result<Self, ConfigError> {
        Self::from_json_boxed(bytes).map(|config| *config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.version != CONFIG_VERSION {
            return Err(ConfigError::UnsupportedVersion);
        }
        if self.wifi_ssid.is_empty() || !bounded_text(&self.wifi_ssid, MAX_SSID_BYTES) {
            return Err(ConfigError::InvalidSsid);
        }
        // The initial version accepts open networks or WPA passphrases. The
        // distinct 64-hex-character PSK form is deliberately not accepted.
        if !bounded_text(&self.wifi_password, MAX_WIFI_PASSWORD_BYTES)
            || (!self.wifi_password.is_empty() && self.wifi_password.len() < 8)
        {
            return Err(ConfigError::InvalidWifiPassword);
        }
        if !bounded_text(&self.deepseek_key, MAX_SECRET_BYTES) {
            return Err(ConfigError::InvalidDeepSeekKey);
        }
        if !bounded_text(&self.deepseek_model, MAX_MODEL_BYTES)
            || (!self.deepseek_key.is_empty() && self.deepseek_model.is_empty())
        {
            return Err(ConfigError::InvalidDeepSeekModel);
        }
        if !bounded_text(&self.qq_app_id, MAX_IDENTIFIER_BYTES) {
            return Err(ConfigError::InvalidQqAppId);
        }
        if !bounded_text(&self.qq_app_secret, MAX_SECRET_BYTES) {
            return Err(ConfigError::InvalidQqSecret);
        }
        if self.qq_app_id.is_empty() != self.qq_app_secret.is_empty() {
            return Err(ConfigError::IncompleteQqCredentials);
        }
        if self.allow_users.len() > MAX_ALLOW_LIST_ENTRIES
            || self.allow_groups.len() > MAX_ALLOW_LIST_ENTRIES
        {
            return Err(ConfigError::TooManyAllowedIds);
        }
        for id in self.allow_users.iter().chain(self.allow_groups.iter()) {
            if id.is_empty() || !bounded_text(id, MAX_IDENTIFIER_BYTES) {
                return Err(ConfigError::InvalidAllowedId);
            }
        }
        Ok(())
    }

    pub fn status(&self) -> ConfigStatus {
        ConfigStatus {
            version: self.version,
            generation: self.generation,
            wifi_configured: !self.wifi_ssid.is_empty(),
            wifi_password_present: !self.wifi_password.is_empty(),
            deepseek_configured: self.deepseek_ready(),
            qq_configured: self.qq_ready(),
            allowed_user_count: self.allow_users.len(),
            allowed_group_count: self.allow_groups.len(),
        }
    }

    pub fn deepseek_ready(&self) -> bool {
        !self.deepseek_key.is_empty() && !self.deepseek_model.is_empty()
    }

    pub fn qq_ready(&self) -> bool {
        !self.qq_app_id.is_empty() && !self.qq_app_secret.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            version: CONFIG_VERSION,
            generation: 1,
            wifi_ssid: "test-network".into(),
            wifi_password: "fake-passphrase".into(),
            deepseek_key: "not-a-real-key".into(),
            deepseek_model: "configured-model".into(),
            qq_app_id: "123456".into(),
            qq_app_secret: "not-a-real-secret".into(),
            allow_users: vec!["user-open-id".into()],
            allow_groups: vec![],
        }
    }

    #[test]
    fn rejects_obsolete_version_and_missing_fields() {
        let mut c = config();
        c.version = 0;
        assert_eq!(c.validate(), Err(ConfigError::UnsupportedVersion));
        assert_eq!(
            Config::from_json(br#"{"version":1}"#).unwrap_err(),
            ConfigError::InvalidJson
        );
    }

    #[test]
    fn validates_wifi_in_bytes_and_passphrase_bounds() {
        let mut c = config();
        c.wifi_ssid = "界".repeat(11);
        assert_eq!(c.validate(), Err(ConfigError::InvalidSsid));
        c.wifi_ssid = "界".repeat(10);
        c.wifi_password = "short".into();
        assert_eq!(c.validate(), Err(ConfigError::InvalidWifiPassword));
        c.wifi_password.clear();
        assert!(c.validate().is_ok());
        c.wifi_password = "x".repeat(64);
        assert_eq!(c.validate(), Err(ConfigError::InvalidWifiPassword));
    }

    #[test]
    fn rejects_mixed_qq_credentials_and_control_characters() {
        let mut c = config();
        c.qq_app_secret.clear();
        assert_eq!(c.validate(), Err(ConfigError::IncompleteQqCredentials));
        c.qq_app_id.clear();
        c.deepseek_key = "bad\r\nheader".into();
        assert_eq!(c.validate(), Err(ConfigError::InvalidDeepSeekKey));
    }

    #[test]
    fn debug_and_status_do_not_expose_credentials() {
        let c = config();
        let debug = format!("{c:?}");
        let status = serde_json::to_string(&c.status()).unwrap();
        for secret in [&c.wifi_password, &c.deepseek_key, &c.qq_app_secret] {
            assert!(!debug.contains(secret));
            assert!(!status.contains(secret));
        }
    }

    #[test]
    fn bounded_json_round_trip_and_allow_list_validation() {
        let c = config();
        let encoded = serde_json::to_vec(&c).unwrap();
        let decoded = Config::from_json(&encoded).unwrap();
        assert_eq!(decoded.generation, c.generation);
        assert!(decoded.qq_ready());
        let mut oversized = encoded;
        oversized.resize(MAX_CONFIG_BYTES + 1, b' ');
        assert_eq!(
            Config::from_json(&oversized).unwrap_err(),
            ConfigError::TooLarge
        );
        let mut c = config();
        c.allow_groups.push(String::new());
        assert_eq!(c.validate(), Err(ConfigError::InvalidAllowedId));
    }
}
