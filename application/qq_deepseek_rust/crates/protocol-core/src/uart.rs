//! Only side-effect-free queries may survive a damaged or legacy UART envelope.
pub fn authorized(command: &str, request_id: bool, checksum: bool, boot_guard: bool) -> bool {
    matches!(
        command.trim(),
        "status"
            | "help"
            | "led status"
            | "wifi status"
            | "bot status"
            | "deepseek status"
            | "sd status"
    ) || (request_id && checksum && boot_guard)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mutations_never_downgrade_to_unframed_execution() {
        for command in [
            "nvs test",
            "reboot",
            "led on",
            "config import {}",
            "bot notify-owner test",
        ] {
            for (id, crc, boot) in [
                (false, false, false),
                (true, false, false),
                (true, true, false),
                (true, false, true),
                (false, true, true),
            ] {
                assert!(!authorized(command, id, crc, boot), "{command}");
            }
            assert!(authorized(command, true, true, true));
        }
    }
    #[test]
    fn initial_readonly_identification_remains_available() {
        for command in [
            "status",
            "help",
            "led status",
            "wifi status",
            "bot status",
            "deepseek status",
            "sd status",
        ] {
            assert!(authorized(command, false, false, false));
        }
    }
    #[test]
    fn query_prefixes_and_additional_arguments_do_not_bypass_guards() {
        for command in [
            "status reboot",
            "led status on",
            "wifi status connect",
            "web pairing",
        ] {
            assert!(!authorized(command, true, false, false));
        }
    }
}
