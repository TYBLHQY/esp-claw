//! Poll-based Wi-Fi supervisor: no blocking connection wait in the control loop.
use esp_idf_svc::{
    eventloop::{EspSubscription, EspSystemEventLoop, System},
    hal::modem::Modem,
    sntp::{EspSntp, SntpConf, SyncStatus},
    wifi::{AuthMethod, ClientConfiguration, Configuration, EspWifi, WifiEvent},
};
use qq_deepseek_core::Config;
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicU16, AtomicU32, Ordering},
        Arc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use super::AppResult;

const CONNECT_DEADLINE: Duration = Duration::from_secs(45);
const MAX_BACKOFF_SECS: u64 = 60;
const IP_REPORT_INTERVAL: Duration = Duration::from_secs(1);
const MIN_VALID_UNIX_TIME: u64 = 1_700_000_000;
const TIME_SERVERS: [&str; 4] = [
    "time1.cloud.tencent.com",
    "ntp.aliyun.com",
    "time.cloudflare.com",
    "0.pool.ntp.org",
];
const TIME_SYNC_DEADLINE: Duration = Duration::from_secs(20);
const TIME_SERVER_ROTATION_DELAY: Duration = Duration::from_secs(1);
const TIME_INIT_RETRY_DELAY: Duration = Duration::from_secs(5);

pub struct Network {
    wifi: EspWifi<'static>,
    _wifi_events: EspSubscription<'static, System>,
    disconnect_reason: Arc<AtomicU16>,
    disconnect_events: Arc<AtomicU32>,
    observed_disconnects: u32,
    enabled: bool,
    configured: bool,
    attempt: Option<Instant>,
    next_attempt: Instant,
    retries: u32,
    fault: Option<String>,
    sntp: Option<EspSntp<'static>>,
    sntp_started: Option<Instant>,
    next_time_attempt: Instant,
    time_server_index: usize,
    time_server: Option<&'static str>,
    time_sync_attempts: u32,
    time_sync_fault: Option<String>,
    reported_ip: Option<String>,
    next_ip_report: Instant,
}

impl Network {
    pub fn new(modem: Modem<'static>, events: EspSystemEventLoop) -> AppResult<Self> {
        // None deliberately disables the Wi-Fi driver's separate NVS persistence.
        let wifi = EspWifi::new(modem, events.clone(), None).map_err(|e| e.to_string())?;
        let disconnect_reason = Arc::new(AtomicU16::new(0));
        let disconnect_events = Arc::new(AtomicU32::new(0));
        let reason = disconnect_reason.clone();
        let count = disconnect_events.clone();
        let subscription = events
            .subscribe::<WifiEvent, _>(move |event| {
                if let WifiEvent::StaDisconnected(data) = event {
                    // Do not Debug-print the event: it also contains the SSID/BSSID.
                    reason.store(data.reason(), Ordering::Relaxed);
                    count.fetch_add(1, Ordering::Release);
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            wifi,
            _wifi_events: subscription,
            disconnect_reason,
            disconnect_events,
            observed_disconnects: 0,
            enabled: false,
            configured: false,
            attempt: None,
            next_attempt: Instant::now(),
            retries: 0,
            fault: None,
            sntp: None,
            sntp_started: None,
            next_time_attempt: Instant::now(),
            time_server_index: 0,
            time_server: None,
            time_sync_attempts: 0,
            time_sync_fault: None,
            reported_ip: None,
            next_ip_report: Instant::now(),
        })
    }

    pub fn configure(&mut self, config: &Config) -> AppResult<()> {
        // Manual teardown must not look like a failed in-progress connection in tick().
        self.attempt = None;
        self.reported_ip = None;
        if self.wifi.is_started().map_err(|e| e.to_string())? {
            let _ = self.wifi.disconnect();
            self.wifi.stop().map_err(|e| e.to_string())?;
        }
        let auth = if config.wifi_password.is_empty() {
            AuthMethod::None
        } else {
            AuthMethod::WPA2Personal
        };
        let client = Box::new(Configuration::Client(ClientConfiguration {
            ssid: config
                .wifi_ssid
                .as_str()
                .try_into()
                .map_err(|_| "invalid SSID length")?,
            password: config
                .wifi_password
                .as_str()
                .try_into()
                .map_err(|_| "invalid Wi-Fi password length")?,
            auth_method: auth,
            ..Default::default()
        }));
        self.wifi
            .set_configuration(&client)
            .map_err(|e| e.to_string())?;
        self.configured = true;
        self.enabled = true;
        self.attempt = None;
        self.next_attempt = Instant::now();
        self.retries = 0;
        self.reported_ip = None;
        Ok(())
    }

    pub fn enable(&mut self, enabled: bool) -> AppResult<()> {
        if enabled && !self.configured {
            return Err("Wi-Fi has no imported configuration".into());
        }
        self.enabled = enabled;
        self.attempt = None;
        self.next_attempt = Instant::now();
        if !enabled {
            self.reported_ip = None;
            self.wifi.disconnect().map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub fn tick(&mut self) {
        if !self.enabled {
            return;
        }
        let disconnects = self.disconnect_events.load(Ordering::Acquire);
        let disconnected = disconnects != self.observed_disconnects;
        self.observed_disconnects = disconnects;
        if self.online() {
            self.attempt = None;
            self.retries = 0;
            self.fault = None;
            let now = Instant::now();
            self.tick_time(now);
            if self.reported_ip.is_none() || now >= self.next_ip_report {
                self.next_ip_report = now + IP_REPORT_INTERVAL;
                let ip = self
                    .wifi
                    .sta_netif()
                    .get_ip_info()
                    .ok()
                    .map(|info| info.ip.to_string());
                if ip != self.reported_ip {
                    super::console::event(&json!({"wifi":"online","ip":ip}));
                    self.reported_ip = ip;
                }
            }
            return;
        }
        if disconnected && (self.attempt.is_some() || self.reported_ip.is_some()) {
            self.attempt = None;
            self.reported_ip = None;
            self.backoff(format!(
                "station disconnected reason={}",
                self.disconnect_reason.load(Ordering::Relaxed)
            ));
            return;
        }
        self.reported_ip = None;
        if self
            .attempt
            .is_some_and(|start| start.elapsed() < CONNECT_DEADLINE)
        {
            return;
        }
        if self.attempt.take().is_some() {
            let _ = self.wifi.disconnect();
            self.backoff("connection or DHCP deadline exceeded".into());
        }
        if Instant::now() < self.next_attempt {
            return;
        }
        let started = match self.wifi.is_started() {
            Ok(v) => v,
            Err(e) => {
                self.backoff(e.to_string());
                return;
            }
        };
        if !started {
            if let Err(e) = self.wifi.start() {
                self.backoff(e.to_string());
                return;
            }
            // ESP event-loop status is updated asynchronously after start().
            self.next_attempt = Instant::now() + Duration::from_millis(150);
            return;
        }
        // Drain already-delivered events from a previous/manual disconnect before a new attempt.
        self.observed_disconnects = self.disconnect_events.load(Ordering::Acquire);
        match self.wifi.connect() {
            Ok(()) => self.attempt = Some(Instant::now()),
            Err(e) => self.backoff(e.to_string()),
        }
    }

    fn tick_time(&mut self, now: Instant) {
        if let Some(sntp) = self.sntp.as_ref() {
            let clock_valid = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .is_ok_and(|elapsed| elapsed.as_secs() > MIN_VALID_UNIX_TIME);
            if sntp.get_sync_status() == SyncStatus::Completed || clock_valid {
                // Leave the successful SDK owner alive for periodic synchronization.
                self.sntp_started = None;
                self.time_sync_fault = None;
                return;
            }
            if self
                .sntp_started
                .is_some_and(|started| now.saturating_duration_since(started) < TIME_SYNC_DEADLINE)
            {
                return;
            }
            // EspSntp::drop stops its SDK service and releases the singleton
            // before any replacement can be constructed on a later tick.
            drop(self.sntp.take());
            self.sntp_started = None;
            self.time_server_index = (self.time_server_index + 1) % TIME_SERVERS.len();
            self.next_time_attempt = now + TIME_SERVER_ROTATION_DELAY;
            self.time_sync_fault = Some("time_server_deadline".into());
            return;
        }
        if now < self.next_time_attempt {
            return;
        }
        let mut config = SntpConf::default();
        // The SDK fixes this array's length at build time (currently one).
        // Populate its actual slots, rather than assuming four are available.
        for (slot, server) in config.servers.iter_mut().enumerate() {
            *server = TIME_SERVERS[(self.time_server_index + slot) % TIME_SERVERS.len()];
        }
        self.time_server = Some(TIME_SERVERS[self.time_server_index]);
        self.time_sync_attempts = self.time_sync_attempts.saturating_add(1);
        match EspSntp::new(&config) {
            Ok(sntp) => {
                self.sntp = Some(sntp);
                self.sntp_started = Some(now);
            }
            Err(error) => {
                self.time_sync_fault = Some(format!("time_init code={}", error.code()));
                self.time_server_index = (self.time_server_index + 1) % TIME_SERVERS.len();
                self.next_time_attempt = now + TIME_INIT_RETRY_DELAY;
            }
        }
    }

    fn backoff(&mut self, error: String) {
        self.retries = self.retries.saturating_add(1);
        let secs = (1_u64 << self.retries.min(6)).min(MAX_BACKOFF_SECS);
        self.next_attempt = Instant::now() + Duration::from_secs(secs);
        self.fault = Some(error);
        super::console::event(&json!({"wifi":"retry","delay_seconds":secs,"attempt":self.retries}));
    }

    pub fn online(&self) -> bool {
        self.enabled
            && self.wifi.is_connected().unwrap_or(false)
            && self.wifi.sta_netif().is_up().unwrap_or(false)
            && self
                .wifi
                .sta_netif()
                .get_ip_info()
                .is_ok_and(|info| !info.ip.is_unspecified())
    }

    pub fn status(&self) -> Value {
        let disconnects = self.disconnect_events.load(Ordering::Acquire);
        let reason = (disconnects != 0).then(|| self.disconnect_reason.load(Ordering::Relaxed));
        let unix_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        json!({"enabled":self.enabled,"configured":self.configured,"online":self.online(),
            "ip":self.wifi.sta_netif().get_ip_info().ok().map(|info|info.ip.to_string()),
            "time_synchronized":unix_time > MIN_VALID_UNIX_TIME,"unix_time":unix_time,
            "time_server":self.time_server,"time_sync_attempts":self.time_sync_attempts,
            "time_sync_fault":self.time_sync_fault,
            "station_netif_up":self.wifi.sta_netif().is_up().unwrap_or(false),
            "last_disconnect_reason":reason,"disconnect_events":disconnects,
            "connect_deadline_seconds":CONNECT_DEADLINE.as_secs(),
            "attempt_elapsed_seconds":self.attempt.map(|start|start.elapsed().as_secs()),
            "retries":self.retries,"fault":self.fault,"credentials_in_driver_nvs":false})
    }

    /// Allocation-free readiness check for the main control loop.
    pub fn ready_for_tls(&self) -> bool {
        self.online()
            && SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .is_ok_and(|elapsed| elapsed.as_secs() > MIN_VALID_UNIX_TIME)
    }
}
