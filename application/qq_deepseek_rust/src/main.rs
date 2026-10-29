#[cfg(feature = "bot")]
mod bot;
mod management;
mod platform;
#[cfg(feature = "bot")]
mod services;
mod web;

use esp_idf_svc::{eventloop::EspSystemEventLoop, hal::peripherals::Peripherals, log::EspLogger};
use platform::{
    console::Input, diagnostics, led::Led, network::Network, storage::Store, AppResult,
};
use qq_deepseek_core::{parse_command, Command, Config, LedCommand, Rgb, WifiCommand};
use serde_json::{json, Value};
use std::{
    io::Write,
    sync::mpsc::RecvTimeoutError,
    time::{Duration, Instant},
};

struct Device {
    boot_id: String,
    uart_replays: u32,
    uart_cached: Option<CachedReply>,
    console_overruns: std::sync::Arc<std::sync::atomic::AtomicU32>,
    watchdog_registered: bool,
    hardware_error: Option<String>,
    led: Option<Led>,
    led_error: Option<String>,
    store: Option<Store>,
    storage_error: Option<String>,
    network: Option<Network>,
    network_error: Option<String>,
    sd: Option<platform::sdcard::SdCard>,
    sd_error: Option<String>,
    web_status: Value,
    web_pairing_token: Option<String>,
    reconfigure_network: Option<Instant>,
    #[cfg(feature = "bot")]
    services: services::Services,
}
struct CachedReply {
    id: String,
    request_crc: u32,
    frame: Vec<u8>,
}

impl Device {
    fn status(&self) -> Value {
        let mut status = json!({"hardware":diagnostics::status(), "hardware_error": self.hardware_error,
            "console":{"protocol":3,"boot_id":self.boot_id,"response_replays":self.uart_replays},
            "led":self.led.as_ref().map(Led::status),"led_error":self.led_error,
            "nvs":self.store.as_ref().map(Store::status),"nvs_error":self.storage_error,
            "wifi":self.network.as_ref().map(Network::status),"wifi_error":self.network_error,
            "watchdog_registered":self.watchdog_registered,
            "sd":self.sd.as_ref().map(|sd|sd.status()),"sd_error":self.sd_error,"web":self.web_status,
            "console_queue_overruns":self.console_overruns.load(std::sync::atomic::Ordering::Relaxed)});
        #[cfg(feature = "bot")]
        {
            status["services"] = self.services.status();
        }
        #[cfg(not(feature = "bot"))]
        {
            status["services"] = json!({"enabled_in_build":false});
        }
        status
    }

    fn execute_line(&mut self, text: &str) -> AppResult<Value> {
        if text == "web pairing" {
            return Ok(
                json!({"admin_token":self.web_pairing_token.as_ref().ok_or("等待 Wi-Fi 连接和网页服务就绪")?,"keep_private":true}),
            );
        }
        if text.starts_with("sd ") {
            return self
                .sd
                .as_ref()
                .ok_or("SD worker unavailable")?
                .command(text);
        }
        if text == "console stress" {
            platform::console::stress()?;
            return Ok(json!({"started":true,"diagnostic":"serial_log_framing"}));
        }
        #[cfg(feature = "bot")]
        if text.starts_with("bot ") || text.starts_with("deepseek ") || text.starts_with("memory ")
        {
            let ready = self.hardware_error.is_none()
                && self.network.as_ref().is_some_and(Network::ready_for_tls);
            return self.services.command(
                text,
                self.store.as_ref().ok_or("NVS unavailable")?,
                ready,
            );
        }
        parse_command(text)
            .map_err(|e| e.to_string())
            .and_then(|c| self.execute(c))
    }

    #[cfg(feature = "bot")]
    fn poll_services(&mut self) {
        let ready = self.hardware_error.is_none()
            && self.network.as_ref().is_some_and(Network::ready_for_tls);
        self.services
            .poll(self.store.as_ref().and_then(Store::current), ready);
        for _ in 0..4 {
            let Some(request) = self.services.control() else {
                break;
            };
            let result = match request.command {
                bot::DeviceCommand::Status => {
                    Ok(qq_deepseek_core::privacy::qq_status(self.status()))
                }
                bot::DeviceCommand::Led(led) => self.execute(Command::Led(led)),
            };
            let response = match result {
                Ok(value) => bot::ControlReply {
                    success: true,
                    text: value.to_string(),
                },
                Err(error) => bot::ControlReply {
                    success: false,
                    text: error,
                },
            };
            let _ = request.respond_to.try_send(response);
        }
    }

    fn execute(&mut self, command: Command<'_>) -> AppResult<Value> {
        match command {
            Command::Status => Ok(self.status()),
            Command::SelfTest => diagnostics::self_test(),
            Command::Led(command) => {
                let led = self.led.as_mut().ok_or("LED driver unavailable")?;
                let color = match command {
                    LedCommand::On => Some(Rgb::ON),
                    LedCommand::Off => Some(Rgb::OFF),
                    LedCommand::Rgb(rgb) => Some(rgb),
                    LedCommand::Status => None,
                };
                if let Some(color) = color {
                    led.set(color)?;
                }
                Ok(led.status())
            }
            Command::Wifi(command) => {
                if matches!(command, WifiCommand::Connect) && self.hardware_error.is_some() {
                    return Err("hardware self-test failed; network start inhibited".into());
                }
                let network = self.network.as_mut().ok_or("Wi-Fi driver unavailable")?;
                match command {
                    WifiCommand::Status => (),
                    WifiCommand::Connect => network.enable(true)?,
                    WifiCommand::Disconnect => network.enable(false)?,
                }
                Ok(network.status())
            }
            Command::NvsTest => self.store.as_ref().ok_or("NVS unavailable")?.test(),
            Command::ConfigImport(raw) => {
                #[cfg(feature = "bot")]
                if self.services.busy_for_configuration() {
                    return Err("stop bot before replacing configuration".into());
                }
                let config = Config::from_json_boxed(raw.as_bytes()).map_err(|e| e.to_string())?;
                let store = self
                    .store
                    .as_mut()
                    .ok_or("NVS unavailable; data was not erased")?;
                let status = store.import(config)?;
                if self.hardware_error.is_none() {
                    if let (Some(network), Some(config)) = (&mut self.network, store.current()) {
                        if let Err(error) = network.configure(&config) {
                            self.network_error = Some(error);
                            return Ok(
                                json!({"persisted":true,"nvs":status,"wifi_error":self.network_error}),
                            );
                        }
                    }
                }
                Ok(json!({"persisted":true,"nvs":status}))
            }
            Command::Help => Ok(json!({"commands":["status","selftest","led on|off|status",
                "led rgb R G B (0..32)","wifi status|connect|disconnect","nvs test",
                "config import JSON (local UART only, never echoed)","reboot"]})),
            Command::Reboot => Ok(json!({"rebooting":true})),
        }
    }
}

fn handle_line(device: &mut Device, line: &str) {
    let (id, text) = if let Some(rest) = line.strip_prefix('@') {
        match rest.split_once(' ') {
            Some((id, text))
                if id.len() <= 32 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') =>
            {
                (Some(id), text)
            }
            _ => {
                platform::console::reply(&json!({"ok":false,"error":"invalid_request_id"}));
                return;
            }
        }
    } else {
        (None, line)
    };
    let request_crc = qq_deepseek_core::record::crc32(text.as_bytes());
    let has_checksum = text.starts_with('#');
    let text = if let Some(checked) = text.strip_prefix('#') {
        match checked.split_once(' ') {
            Some((sum, command))
                if sum.len() == 8
                    && u32::from_str_radix(sum, 16).ok()
                        == Some(qq_deepseek_core::record::crc32(command.as_bytes())) =>
            {
                command
            }
            _ => {
                platform::console::reply(
                    &json!({"id":id,"ok":false,"error":"request_checksum_mismatch"}),
                );
                return;
            }
        }
    } else {
        text
    };
    let has_boot_guard = text.starts_with('!');
    let text = if let Some(guarded) = text.strip_prefix('!') {
        match guarded.split_once(' ') {
            Some((boot, text)) if boot == device.boot_id => text,
            _ => {
                platform::console::reply(
                    &json!({"id":id,"ok":false,"error":"device_restarted_request_not_replayed"}),
                );
                return;
            }
        }
    } else {
        text
    };
    if !qq_deepseek_core::uart::authorized(text, id.is_some(), has_checksum, has_boot_guard) {
        platform::console::reply(
            &json!({"id":id,"ok":false,"error":"checksummed_boot_guard_required_use_boardctl"}),
        );
        return;
    }
    if let Some(id) = id {
        if let Some(cached) = &device.uart_cached {
            if cached.id == id {
                if cached.request_crc == request_crc {
                    device.uart_replays = device.uart_replays.saturating_add(1);
                    platform::console::write_frame(&cached.frame);
                } else {
                    platform::console::reply(
                        &json!({"id":id,"ok":false,"error":"request_id_conflict"}),
                    );
                }
                return;
            }
        }
    }
    let result = device.execute_line(text);
    let reboot = result
        .as_ref()
        .ok()
        .is_some_and(|v| v.get("rebooting") == Some(&Value::Bool(true)));
    let reply = match result {
        Ok(value) => json!({"id":id,"ok":true,"result":value}),
        Err(error) => json!({"id":id,"ok":false,"error":error}),
    };
    if let Some(frame) = platform::console::frame("REPLY", &reply) {
        if let Some(id) = id {
            device.uart_cached = Some(CachedReply {
                id: id.to_owned(),
                request_crc,
                frame,
            });
            platform::console::write_frame(&device.uart_cached.as_ref().unwrap().frame);
        } else {
            platform::console::write_frame(&frame);
        }
    }
    if reboot {
        let _ = std::io::stdout().flush();
        std::thread::sleep(Duration::from_millis(150));
        unsafe { esp_idf_svc::sys::esp_restart() };
    }
}

fn main() {
    esp_idf_svc::sys::link_patches();
    EspLogger::initialize_default();
    let inputs = match platform::console::start() {
        Ok(rx) => rx,
        Err(error) => {
            log::error!("Console unavailable: {error}");
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    };
    let (led, led_error) = match Led::new() {
        Ok(v) => (Some(v), None),
        Err(e) => (None, Some(e)),
    };
    let (store, storage_error) = match Store::open() {
        Ok(v) => (Some(v), None),
        Err(e) => (None, Some(e)),
    };
    let network = Peripherals::take()
        .map_err(|e| e.to_string())
        .and_then(|p| {
            let events = EspSystemEventLoop::take().map_err(|e| e.to_string())?;
            Network::new(p.modem, events)
        });
    let (network, network_error) = match network {
        Ok(v) => (Some(v), None),
        Err(e) => (None, Some(e)),
    };
    let selftest = diagnostics::self_test();
    let hardware_error = selftest.as_ref().err().cloned();
    #[cfg(feature = "bot")]
    let desired_mode = store.as_ref().and_then(|s| s.bot_mode().ok()).unwrap_or(0);
    let (sd, sd_error) = match platform::sdcard::SdCard::start() {
        Ok(sd) => (Some(sd), None),
        Err(error) => (None, Some(error)),
    };
    #[cfg(feature = "bot")]
    let memory_client = sd.as_ref().map(|sd| sd.memory_client());
    let mut device = Box::new(Device {
        boot_id: format!(
            "{:08x}{:08x}",
            unsafe { esp_idf_svc::sys::esp_random() },
            unsafe { esp_idf_svc::sys::esp_random() }
        ),
        uart_replays: 0,
        uart_cached: None,
        console_overruns: inputs.overruns.clone(),
        watchdog_registered: false,
        hardware_error,
        led,
        led_error,
        store,
        storage_error,
        network,
        network_error,
        sd,
        sd_error,
        web_status: json!({"enabled":false,"token_configured":false}),
        web_pairing_token: None,
        reconfigure_network: None,
        #[cfg(feature = "bot")]
        services: services::Services::new(desired_mode, memory_client),
    });
    let mut management = management::Management::new();
    let mut sd_boot_journaled = false;
    if device.hardware_error.is_none() {
        if let Some(sd) = &device.sd {
            let _ = sd.command("sd mount");
        }
        if let (Some(network), Some(config)) = (
            &mut device.network,
            device.store.as_ref().and_then(Store::current),
        ) {
            if let Err(e) = network.configure(&config) {
                device.network_error = Some(e);
            }
        }
    }
    platform::console::event(
        &json!({"boot":"ready","selftest":selftest.ok(),"hardware_error":device.hardware_error,
        "nvs":device.store.as_ref().map(Store::status),"led_error":device.led_error,
        "nvs_error":device.storage_error,"wifi_error":device.network_error}),
    );
    // Feed only after a full control-loop iteration. A stuck SDK call must not
    // be hidden by an independent task that unconditionally feeds for us.
    let watchdog_add = unsafe { esp_idf_svc::sys::esp_task_wdt_add(std::ptr::null_mut()) };
    let mut watchdog = unsafe { esp_idf_svc::sys::esp_task_wdt_status(std::ptr::null_mut()) }
        == esp_idf_svc::sys::ESP_OK;
    device.watchdog_registered = watchdog;
    platform::console::event(
        &json!({"watchdog_registered":watchdog,"registration_result":watchdog_add}),
    );
    loop {
        if device
            .reconfigure_network
            .is_some_and(|at| Instant::now() >= at)
        {
            device.reconfigure_network = None;
            if let (Some(network), Some(config)) = (
                &mut device.network,
                device.store.as_ref().and_then(Store::current),
            ) {
                device.network_error = network.configure(&config).err();
            }
        }
        if let Some(network) = &mut device.network {
            network.tick();
        }
        management.poll(&mut device);
        if !sd_boot_journaled {
            if let Some(sd) = &device.sd {
                let status = sd.status();
                if status["available"] == true && status["busy"] == false {
                    if sd.journal(platform::sdcard::JournalEvent::Boot).is_ok() {
                        sd_boot_journaled = true;
                    }
                }
            }
        }
        #[cfg(feature = "bot")]
        device.poll_services();
        match inputs.receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(Input::Line(mut line)) => {
                handle_line(&mut device, &line);
                // Do not retain raw imported JSON in the input buffer.
                unsafe {
                    line.as_bytes_mut().fill(0);
                }
            }
            Ok(Input::Invalid) => platform::console::reply(
                &json!({"ok":false,"error":"invalid_or_overlong_uart_input"}),
            ),
            Err(RecvTimeoutError::Timeout) => (),
            Err(RecvTimeoutError::Disconnected) => {
                log::error!("Console input task stopped");
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        if watchdog && unsafe { esp_idf_svc::sys::esp_task_wdt_reset() } != esp_idf_svc::sys::ESP_OK
        {
            log::error!("Control-loop watchdog registration lost");
            watchdog = false;
            device.watchdog_registered = false;
        }
    }
}
