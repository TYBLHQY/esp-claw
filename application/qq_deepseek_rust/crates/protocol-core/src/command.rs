use core::fmt;
use serde::{Deserialize, Serialize};

pub const MAX_LED_BRIGHTNESS: u8 = 32;
pub const MAX_COMMAND_BYTES: usize = 4096 + 32;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const OFF: Self = Self { r: 0, g: 0, b: 0 };
    pub const ON: Self = Self { r: 8, g: 8, b: 8 };

    pub fn try_new(r: u8, g: u8, b: u8) -> Result<Self, CommandError> {
        if r > MAX_LED_BRIGHTNESS || g > MAX_LED_BRIGHTNESS || b > MAX_LED_BRIGHTNESS {
            return Err(CommandError::BrightnessLimit);
        }
        Ok(Self { r, g, b })
    }

    pub fn validate(self) -> Result<Self, CommandError> {
        Self::try_new(self.r, self.g, self.b)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedCommand {
    On,
    Off,
    Rgb(Rgb),
    Status,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WifiCommand {
    Status,
    Connect,
    Disconnect,
}

/// Deliberately does not implement Debug: ConfigImport contains raw credentials.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Command<'a> {
    Status,
    SelfTest,
    Led(LedCommand),
    Wifi(WifiCommand),
    NvsTest,
    Reboot,
    Help,
    ConfigImport(&'a str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandError {
    Empty,
    TooLong,
    Unknown,
    InvalidArguments,
    BrightnessLimit,
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "empty_command",
            Self::TooLong => "command_too_long",
            Self::Unknown => "unknown_command",
            Self::InvalidArguments => "invalid_command_arguments",
            Self::BrightnessLimit => "led_brightness_limit_32",
        })
    }
}

impl std::error::Error for CommandError {}

pub fn parse_command(input: &str) -> Result<Command<'_>, CommandError> {
    if input.len() > MAX_COMMAND_BYTES {
        return Err(CommandError::TooLong);
    }
    let input = input.trim();
    let input = input.strip_prefix('/').unwrap_or(input);
    if input.is_empty() {
        return Err(CommandError::Empty);
    }
    // Preserve JSON whitespace exactly. Only the local UART handler should
    // execute this command; remote bot dispatch must reject it.
    if let Some(rest) = input.strip_prefix("config") {
        if !rest.starts_with(char::is_whitespace) {
            return Err(CommandError::Unknown);
        }
        let rest = rest.trim_start();
        let json = rest
            .strip_prefix("import")
            .filter(|remaining| remaining.starts_with(char::is_whitespace))
            .ok_or(CommandError::InvalidArguments)?
            .trim_start();
        if json.is_empty() {
            return Err(CommandError::InvalidArguments);
        }
        return Ok(Command::ConfigImport(json));
    }
    let mut parts = input.split_whitespace();
    let command = match parts.next().ok_or(CommandError::Empty)? {
        "status" => Command::Status,
        "selftest" => Command::SelfTest,
        "reboot" => Command::Reboot,
        "help" => Command::Help,
        "nvs" if parts.next() == Some("test") => Command::NvsTest,
        "wifi" => Command::Wifi(match parts.next() {
            Some("status") => WifiCommand::Status,
            Some("connect") => WifiCommand::Connect,
            Some("disconnect") => WifiCommand::Disconnect,
            _ => return Err(CommandError::InvalidArguments),
        }),
        "led" => Command::Led(match parts.next() {
            Some("on") => LedCommand::On,
            Some("off") => LedCommand::Off,
            Some("status") => LedCommand::Status,
            Some("rgb") => {
                let mut channel = || {
                    parts
                        .next()
                        .ok_or(CommandError::InvalidArguments)?
                        .parse::<u8>()
                        .map_err(|_| CommandError::InvalidArguments)
                };
                LedCommand::Rgb(Rgb::try_new(channel()?, channel()?, channel()?)?)
            }
            _ => return Err(CommandError::InvalidArguments),
        }),
        "nvs" => return Err(CommandError::InvalidArguments),
        _ => return Err(CommandError::Unknown),
    };
    if parts.next().is_some() {
        return Err(CommandError::InvalidArguments);
    }
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rgb_and_rejects_unsafe_brightness() {
        assert!(matches!(
            parse_command(" /led rgb 3 12 32\r\n"),
            Ok(Command::Led(LedCommand::Rgb(Rgb { r: 3, g: 12, b: 32 })))
        ));
        assert!(matches!(
            parse_command("led rgb 33 0 0"),
            Err(CommandError::BrightnessLimit)
        ));
        assert!(matches!(
            parse_command("led rgb 256 0 0"),
            Err(CommandError::InvalidArguments)
        ));
        assert!(matches!(
            parse_command("led rgb 1 2 3 trailing"),
            Err(CommandError::InvalidArguments)
        ));
    }

    #[test]
    fn rejects_command_prefixes_and_extra_arguments() {
        assert!(matches!(
            parse_command("ledoff"),
            Err(CommandError::Unknown)
        ));
        assert!(matches!(
            parse_command("status extra"),
            Err(CommandError::InvalidArguments)
        ));
        assert!(matches!(
            parse_command("nvs"),
            Err(CommandError::InvalidArguments)
        ));
        assert!(matches!(
            parse_command("wifi connect now"),
            Err(CommandError::InvalidArguments)
        ));
    }

    #[test]
    fn imports_json_by_borrow_without_altering_its_spaces() {
        let text = "config import {\"wifi_ssid\": \"space name\"}";
        assert!(matches!(
            parse_command(text),
            Ok(Command::ConfigImport("{\"wifi_ssid\": \"space name\"}"))
        ));
        assert!(matches!(
            parse_command("configimport secret"),
            Err(CommandError::Unknown)
        ));
        assert!(matches!(
            parse_command("config import"),
            Err(CommandError::InvalidArguments)
        ));
    }
}
