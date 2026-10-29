//! Optional network services; device resources remain owned by the control loop.
use crate::{
    bot::{self, BotMode, BotRuntime, ControlRequest},
    platform::{storage::Store, AppResult},
};
use qq_deepseek_core::Config;
use serde_json::{json, Value};
use std::{
    sync::{
        mpsc::{sync_channel, Receiver, SyncSender},
        Arc,
    },
    time::{Duration, Instant},
};

pub struct Services {
    bot: Option<BotRuntime>,
    controls_tx: SyncSender<ControlRequest>,
    controls_rx: Receiver<ControlRequest>,
    diagnostic_rx: Option<Receiver<AppResult<Value>>>,
    notice_rx: Option<Receiver<Result<String, bot::Error>>>,
    diagnostic_result: Value,
    desired_mode: u8,
    next_start: Instant,
    fault: Option<String>,
    thread_failure: bool,
    memory: Option<crate::platform::sdcard::MemoryClient>,
    diagnostic_private: bool,
}

impl Services {
    pub fn new(desired_mode: u8, memory: Option<crate::platform::sdcard::MemoryClient>) -> Self {
        let (controls_tx, controls_rx) = sync_channel(4);
        Self {
            bot: None,
            controls_tx,
            controls_rx,
            diagnostic_rx: None,
            notice_rx: None,
            diagnostic_result: Value::Null,
            desired_mode,
            next_start: Instant::now(),
            fault: None,
            thread_failure: false,
            memory,
            diagnostic_private: false,
        }
    }

    pub fn running(&self) -> bool {
        self.bot.as_ref().is_some_and(|b| !b.is_stopped())
    }

    pub fn busy_for_configuration(&self) -> bool {
        self.running()
            || self.thread_failure
            || self.bot.as_ref().is_some_and(BotRuntime::cleanup_failed)
            || self.diagnostic_rx.is_some()
            || self.notice_rx.is_some()
    }

    pub fn status(&self) -> Value {
        json!({"bot":self.bot.as_ref().map(BotRuntime::status),"desired_mode":self.desired_mode,
            "fault":self.fault,"thread_failure_requires_reboot":self.thread_failure,"diagnostic_running":self.diagnostic_rx.is_some() || self.notice_rx.is_some(),
            "diagnostic_result":self.diagnostic_result,"memory":self.memory.as_ref().map(|m|m.status())})
    }

    pub fn control(&self) -> Option<ControlRequest> {
        self.controls_rx.try_recv().ok()
    }

    pub fn poll(&mut self, config: Option<Arc<Config>>, ready: bool) {
        if let Some(rx) = &self.notice_rx {
            let result = match rx.try_recv() {
                Ok(result) => Some(result),
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(Err(bot::Error::Thread)),
            };
            if let Some(result) = result {
                self.diagnostic_result = match result {
                    Ok(message_id) => json!({"ok":true,"result":{"qq_message_id":message_id}}),
                    Err(error) => json!({"ok":false,"error":error.to_string()}),
                };
                self.notice_rx = None;
                crate::platform::console::event(&json!({"owner_notice":self.diagnostic_result}));
            }
        }
        if self.bot.as_ref().is_some_and(BotRuntime::cleanup_failed) {
            self.thread_failure = true;
            self.fault = Some("WebSocket cleanup did not finish safely; reboot required".into());
        } else if self.desired_mode != 0 && self.bot.as_ref().is_some_and(BotRuntime::is_stopped) {
            // Reconnects belong inside the gateway worker. Unexpected total
            // retirement may indicate an SDK cleanup failure: do not pile up
            // another C client/callback set in the same process.
            self.thread_failure = true;
            self.fault = Some("bot workers retired unexpectedly; reboot required".into());
        }
        if let Some(rx) = &self.diagnostic_rx {
            let result = match rx.try_recv() {
                Ok(result) => Some(result),
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    Some(Err("diagnostic worker exited without a result".into()))
                }
            };
            if let Some(result) = result {
                self.diagnostic_result = match result {
                    Ok(value) => json!({"ok":true,"result":value}),
                    Err(error) => json!({"ok":false,"error":error}),
                };
                self.diagnostic_rx = None;
                if self.diagnostic_private {
                    crate::platform::console::event(
                        &json!({"diagnostic":{"completed":true,"private_result":true,"ok":self.diagnostic_result.get("ok")}}),
                    );
                } else {
                    crate::platform::console::event(&json!({"diagnostic":self.diagnostic_result}));
                }
            }
        }
        if self.desired_mode != 0
            && !self.thread_failure
            && !self.running()
            && ready
            && self.diagnostic_rx.is_none()
            && self.notice_rx.is_none()
            && Instant::now() >= self.next_start
        {
            if let Some(config) = config {
                if let Err(error) = self.start(config, self.desired_mode) {
                    self.fault = Some(error);
                    self.next_start = Instant::now() + Duration::from_secs(30);
                }
            }
        }
    }

    fn start(&mut self, config: Arc<Config>, mode: u8) -> AppResult<()> {
        if self.thread_failure || self.bot.as_ref().is_some_and(BotRuntime::cleanup_failed) {
            return Err("worker startup failed; reboot before another bot start".into());
        }
        if self.running() {
            return Err("bot already running; stop it before changing mode".into());
        }
        let mode = match mode {
            1 => BotMode::EchoOnly,
            2 => BotMode::DeepSeek,
            _ => return Err("invalid bot mode".into()),
        };
        self.bot = Some(
            BotRuntime::start(config, self.controls_tx.clone(), mode, self.memory.clone())
                .map_err(|error| {
                    if matches!(error, bot::Error::Thread) {
                        self.thread_failure = true;
                    }
                    error.to_string()
                })?,
        );
        self.fault = None;
        Ok(())
    }

    pub fn command(&mut self, text: &str, store: &Store, ready: bool) -> AppResult<Value> {
        if text.starts_with("memory ") {
            return self.memory_command(text, store, ready);
        }
        if let Some(name) = text.trim().strip_prefix("bot media-upload ") {
            if !ready {
                return Err("Wi-Fi and synchronized time required".into());
            }
            if self.running() || self.notice_rx.is_some() || self.diagnostic_rx.is_some() {
                return Err("stop bot and finish diagnostics before media-upload; QQ /picture works while online".into());
            }
            if name != "list" && qq_deepseek_core::material::mime(name).is_none() {
                return Err("invalid SD image filename".into());
            }
            let config = store.current().ok_or("configuration required")?;
            let sd = self.memory.clone().ok_or("SD worker unavailable")?;
            let name = name.to_owned();
            let (tx, rx) = sync_channel(1);
            std::thread::Builder::new()
                .name("media-diagnostic".into())
                .stack_size(16384)
                .spawn(move || {
                    let stop = std::sync::atomic::AtomicBool::new(false);
                    let r = bot::material_diagnostic(&config, &sd, &name, &stop)
                        .map_err(|e| e.to_string());
                    let _ = tx.try_send(r);
                })
                .map_err(|_| "media diagnostic worker unavailable")?;
            self.diagnostic_rx = Some(rx);
            self.diagnostic_private = true;
            self.diagnostic_result = Value::Null;
            return Ok(json!({"started":true,"kind":"material_upload","private_result":true}));
        }
        if let Some(content) = text.trim().strip_prefix("bot notify-owner ") {
            if !ready {
                return Err("Wi-Fi and synchronized time required".into());
            }
            if self.notice_rx.is_some() || self.diagnostic_rx.is_some() {
                return Err("another diagnostic or notification is still running".into());
            }
            let bot = self
                .bot
                .as_mut()
                .filter(|b| !b.is_stopped())
                .ok_or("bot must be running")?;
            let receiver = bot
                .notify_owner(content.to_owned())
                .map_err(|e| e.to_string())?;
            self.notice_rx = Some(receiver);
            self.diagnostic_result = Value::Null;
            return Ok(
                json!({"started":true,"kind":"one_time_owner_notice","automatic_retry":false}),
            );
        }
        for (prefix, kind) in [
            ("deepseek search ", "search"),
            ("deepseek fetch ", "fetch"),
            ("deepseek ask ", "ask"),
            ("deepseek vision ", "vision"),
            ("deepseek memory-ask ", "memory_ask"),
        ] {
            if let Some(input) = text.trim().strip_prefix(prefix) {
                return self.diagnostic(kind, input.to_owned(), store, ready);
            }
        }
        match text.trim() {
            "bot status" | "deepseek status" => Ok(self.status()),
            "bot stop" => {
                // Local shutdown must remain available when Flash cannot commit.
                self.desired_mode = 0;
                if let Some(bot) = &self.bot {
                    bot.stop();
                }
                if store.set_bot_mode(0).is_err() {
                    self.fault =
                        Some("bot stopped; autostart disable could not be persisted".into());
                    return Err("bot stopped; NVS failed, autostart may resume after reboot".into());
                }
                Ok(json!({"stopping":self.running(),"autostart":false}))
            }
            "bot start echo" | "bot start deepseek" => {
                if self.diagnostic_rx.is_some() || self.notice_rx.is_some() {
                    return Err("wait for the current diagnostic or notification".into());
                }
                if !ready {
                    return Err("Wi-Fi and synchronized time required".into());
                }
                let mode = if text.trim() == "bot start echo" {
                    1
                } else {
                    2
                };
                let config = store.current().ok_or("configuration required")?;
                self.start(config, mode)?;
                if let Err(error) = store.set_bot_mode(mode) {
                    if let Some(bot) = &self.bot {
                        bot.stop();
                    }
                    return Err(error);
                }
                self.desired_mode = mode;
                Ok(self.status())
            }
            "deepseek probe" | "deepseek test" => self.diagnostic(
                if text.trim() == "deepseek probe" {
                    "probe"
                } else {
                    "test"
                },
                String::new(),
                store,
                ready,
            ),
            _ => Err("unknown service command".into()),
        }
    }

    fn diagnostic(
        &mut self,
        kind: &'static str,
        input: String,
        store: &Store,
        ready: bool,
    ) -> AppResult<Value> {
        if self.thread_failure || self.bot.as_ref().is_some_and(BotRuntime::cleanup_failed) {
            return Err("unsafe worker cleanup; reboot before another diagnostic".into());
        }
        if !ready {
            return Err("Wi-Fi and synchronized time required".into());
        }
        let model_worker_running = self
            .bot
            .as_ref()
            .is_some_and(|b| !b.is_stopped() && matches!(b.status().mode, BotMode::DeepSeek));
        if self.diagnostic_rx.is_some() || self.notice_rx.is_some() || model_worker_running {
            return Err("use echo mode or stop bot, and wait for existing diagnostics".into());
        }
        if input.len() > if kind == "vision" { 5120 } else { 2048 } {
            return Err("diagnostic input too long".into());
        }
        let config = store.current().ok_or("configuration required")?;
        let memory = self.memory.clone();
        let (tx, rx) = sync_channel(1);
        std::thread::Builder::new()
            .name("api-diagnostic".into())
            .stack_size(16384)
            .spawn(move || {
                let result = match kind {
                    "probe" => bot::probe_deepseek(&config).map(|models| json!({"models":models})),
                    "search" => bot::probe_search(config, &input),
                    "fetch" => bot::probe_fetch(config, &input),
                    "ask" => {
                        bot::ask_deepseek(config, &input).map(|answer| json!({"answer":answer}))
                    }
                    "vision" => {
                        bot::test_vision(config, &input).map(|answer| json!({"answer":answer}))
                    }
                    "memory_ask" => input
                        .split_once(' ')
                        .ok_or(bot::Error::Protocol)
                        .and_then(|(key, prompt)| bot::memory_ask(config, memory, key, prompt)),
                    _ => bot::test_deepseek(config).map(|answer| json!({"answer":answer})),
                };
                let _ = tx.try_send(result.map_err(|e| e.to_string()));
            })
            .map_err(|e| e.to_string())?;
        self.diagnostic_rx = Some(rx);
        self.diagnostic_private = matches!(kind, "ask" | "vision" | "memory_ask");
        self.diagnostic_result = Value::Null;
        Ok(json!({"started":true,"kind":kind}))
    }

    fn memory_command(&mut self, text: &str, store: &Store, ready: bool) -> AppResult<Value> {
        let client = self.memory.clone().ok_or("SD memory worker unavailable")?;
        if text.trim() == "memory status" {
            return Ok(client.status());
        }
        if self.thread_failure || self.bot.as_ref().is_some_and(BotRuntime::cleanup_failed) {
            return Err("reboot required before memory operations".into());
        }
        if self.diagnostic_rx.is_some() || self.notice_rx.is_some() {
            return Err("another operation is running".into());
        }
        let config = store.current().ok_or("configuration required")?;
        let (key, operation) = if let Some(rest) = text.strip_prefix("memory for ") {
            let (key, operation) = rest.split_once(' ').ok_or("use memory for KEY OPERATION")?;
            if !qq_deepseek_core::memory::valid_key(key) {
                return Err("invalid memory key".into());
            }
            (key.to_owned(), format!("memory {operation}"))
        } else {
            if config.allow_users.len() != 1 {
                return Err("web memory management requires exactly one configured owner".into());
            }
            (
                format!("u:{}", config.allow_users[0]),
                text.trim().to_owned(),
            )
        };
        if operation != "memory show" && self.running() {
            return Err(
                "stop bot before editing memory from the web or UART; QQ /remember works online"
                    .into(),
            );
        }
        if operation == "memory compact" && !ready {
            return Err("Wi-Fi and synchronized time required for compaction".into());
        }
        if operation.len() > 1024 {
            return Err("memory command too long".into());
        }
        let (tx, rx) = sync_channel(1);
        std::thread::Builder::new()
            .name("memory-admin".into())
            .stack_size(16384)
            .spawn(move || {
                let result = bot::memory_diagnostic(config, Some(client), &key, &operation)
                    .map_err(|e| e.to_string());
                let _ = tx.try_send(result);
            })
            .map_err(|_| "could not start memory management worker")?;
        self.diagnostic_rx = Some(rx);
        self.diagnostic_private = true;
        self.diagnostic_result = Value::Null;
        Ok(json!({"started":true,"kind":"memory","private_result":true}))
    }
}
