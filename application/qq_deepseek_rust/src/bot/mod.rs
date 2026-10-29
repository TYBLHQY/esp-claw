//! Minimal QQ ↔ DeepSeek runtime. Enable only after board, NVS and Wi-Fi acceptance.
mod attachments;
mod gateway;
mod gateway_socket;
mod http;
mod inference;
mod materials;
mod memory;
mod protocol;
mod sender;
mod stream_sender;
mod vision;
mod web_tools;

pub use inference::{ask_deepseek, memory_ask, memory_diagnostic, test_deepseek, test_vision};
pub use materials::diagnostic as material_diagnostic;
pub use web_tools::{probe_fetch, probe_search};

use protocol::{Message, Target};
use qq_deepseek_core::{Config, LedCommand};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub enum DeviceCommand {
    Status,
    Led(LedCommand),
}
pub struct ControlRequest {
    pub command: DeviceCommand,
    pub respond_to: SyncSender<ControlReply>,
}
pub struct ControlReply {
    pub text: String,
    pub success: bool,
}

#[derive(Copy, Clone, Eq, PartialEq, Serialize)]
pub enum BotMode {
    EchoOnly,
    DeepSeek,
}

/// Authenticate and discover model IDs without starting inference or a QQ connection.
/// Run this blocking network operation on a worker, never in the board control loop.
pub fn probe_deepseek(config: &Config) -> Result<Vec<String>, Error> {
    config
        .validate()
        .map_err(|_| Error::Config.at("models_configuration"))?;
    if config.deepseek_key.is_empty() {
        return Err(Error::Config.at("models_configuration"));
    }
    let stop = AtomicBool::new(false);
    let authorization = format!("Bearer {}", config.deepseek_key);
    let value = http::json_request(
        esp_idf_svc::http::Method::Get,
        "https://api.deepseek.com/models",
        Some(&authorization),
        None,
        &[],
        &stop,
    )
    .map_err(|error| error.at("models_request"))?;
    let data = value
        .get("data")
        .and_then(serde_json::Value::as_array)
        .ok_or(Error::Protocol.at("models_schema"))?;
    if data.is_empty() || data.len() > 32 {
        return Err(Error::Limit.at("models_count"));
    }
    let mut ids = Vec::new();
    ids.try_reserve(data.len())
        .map_err(|_| Error::Allocation.at("models_allocation"))?;
    for model in data {
        let id = model
            .get("id")
            .and_then(serde_json::Value::as_str)
            .ok_or(Error::Protocol.at("models_identifier"))?;
        if id.is_empty()
            || id.len() > 128
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
        {
            return Err(Error::Protocol.at("models_identifier"));
        }
        ids.push(id.to_owned());
    }
    Ok(ids)
}

#[derive(Copy, Clone, Debug)]
pub enum Error {
    Transport,
    Protocol,
    Auth,
    State,
    Limit,
    Allocation,
    Cancelled,
    Timeout,
    Config,
    Thread,
    RemoteCode(i64),
    Rejected {
        status: u16,
        code: i64,
    },
    /// Only literal stages and numerical SDK/HTTP/local codes, never server bodies or headers.
    Diagnostic {
        stage: &'static str,
        code: i32,
    },
    Sse {
        stage: &'static str,
        error: qq_deepseek_core::SseError,
    },
}
impl Error {
    pub fn label(self) -> &'static str {
        match self {
            Self::Transport => "transport",
            Self::Protocol => "protocol",
            Self::Auth => "authentication",
            Self::State => "state",
            Self::Limit => "capacity",
            Self::Allocation => "allocation",
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::Config => "configuration",
            Self::Thread => "thread_start",
            Self::RemoteCode(_) => "qq_business_response",
            Self::Rejected { .. } => "qq_message_rejected",
            Self::Diagnostic { stage, .. } | Self::Sse { stage, .. } => stage,
        }
    }
    fn number(self) -> u32 {
        match self {
            Self::Transport => 1,
            Self::Protocol => 2,
            Self::Auth => 4,
            Self::State => 5,
            Self::Limit => 6,
            Self::Allocation => 7,
            Self::Cancelled => 8,
            Self::Timeout => 9,
            Self::Config => 10,
            Self::Thread => 11,
            Self::RemoteCode(code) | Self::Rejected { code, .. } => {
                code.unsigned_abs().min(u64::from(u32::MAX)) as u32
            }
            Self::Diagnostic { code, .. } => code.unsigned_abs(),
            Self::Sse { error, .. } => 2000 + error as u32,
        }
    }
    fn at(self, stage: &'static str) -> Self {
        match self {
            Self::Diagnostic { .. }
            | Self::Sse { .. }
            | Self::Thread
            | Self::RemoteCode(_)
            | Self::Rejected { .. } => self,
            _ => Self::Diagnostic {
                stage,
                code: self.number() as i32,
            },
        }
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Diagnostic { stage, code } => write!(f, "stage={stage} code={code}"),
            Self::Sse { stage, error } => write!(f, "stage={stage} error={error}"),
            Self::Rejected { status, code } => {
                write!(f, "stage=qq_message_rejected status={status} code={code}")
            }
            Self::RemoteCode(code) => write!(f, "stage=qq_business_response code={code}"),
            _ => write!(f, "{} ({})", self.label(), self.number()),
        }
    }
}
impl std::error::Error for Error {}

#[derive(Default)]
struct Counters {
    gateway_ready: AtomicBool,
    received: AtomicU32,
    denied: AtomicU32,
    group_events: AtomicU32,
    group_self_mentions: AtomicU32,
    group_ignored_without_mention: AtomicU32,
    group_parse_failed: AtomicU32,
    group_denied: AtomicU32,
    sent: AtomicU32,
    errors: AtomicU32,
    last_error: AtomicU32,
    active_workers: AtomicUsize,
    notifying_owner: AtomicBool,
    reconnects: AtomicU32,
    server_reconnects: AtomicU32,
    inference_started: AtomicU32,
    inference_completed: AtomicU32,
    inference_failed: AtomicU32,
    inference_cancelled: AtomicU32,
    history: Mutex<VecDeque<ErrorRecord>>,
}
#[derive(Clone, Serialize)]
pub struct ErrorRecord {
    pub origin: &'static str,
    pub detail: String,
    pub code: u32,
    pub unix_time: Option<u64>,
}
#[derive(Serialize)]
pub struct BotStatus {
    pub mode: BotMode,
    pub gateway_ready: bool,
    pub received: u32,
    pub denied: u32,
    pub group_events: u32,
    pub group_self_mentions: u32,
    pub group_ignored_without_mention: u32,
    pub group_parse_failed: u32,
    pub group_denied: u32,
    pub sent: u32,
    pub errors: u32,
    pub last_error: u32,
    pub active_workers: usize,
    pub cleanup_failed: bool,
    pub cleanup_error_code: i32,
    pub cleanup_worker_active: bool,
    pub reconnects: u32,
    pub server_reconnects: u32,
    pub inference_started: u32,
    pub inference_completed: u32,
    pub inference_failed: u32,
    pub inference_cancelled: u32,
    pub delivery: serde_json::Value,
    pub recent_errors: Vec<ErrorRecord>,
    pub memory: serde_json::Value,
}

pub struct BotRuntime {
    shared: Arc<Shared>,
    _threads: Vec<JoinHandle<()>>,
}
impl BotRuntime {
    /// One locally requested proactive private message. There is no automatic retry or trigger.
    pub fn notify_owner(&mut self, text: String) -> Result<Receiver<Result<String, Error>>, Error> {
        if self.shared.stop.load(Ordering::Relaxed) {
            return Err(Error::State.at("qq_notify_runtime_stopping"));
        }
        if self.shared.config.allow_users.len() != 1 {
            return Err(Error::Config.at("qq_notify_unique_owner_required"));
        }
        let owner = &self.shared.config.allow_users[0];
        if owner.is_empty()
            || owner.len() > 128
            || !owner
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
        {
            return Err(Error::Config.at("qq_notify_owner_identifier"));
        }
        if text.trim().is_empty() || text.len() > sender::MAX_NOTIFICATION_BYTES {
            return Err(Error::Limit.at("qq_notify_text_limit"));
        }
        if self
            .shared
            .counters
            .notifying_owner
            .swap(true, Ordering::AcqRel)
        {
            return Err(Error::Limit.at("qq_notify_busy"));
        }
        // Detach completed thread handles; no join/network wait occurs on the board loop.
        self._threads.retain(|worker| !worker.is_finished());
        let (result_tx, result_rx) = sync_channel(1);
        let shared = self.shared.clone();
        let result = spawn(
            &self.shared,
            &mut self._threads,
            "qq-notify",
            12288,
            move || {
                let result = sender::notify_owner(&shared, &text);
                if let Err(error) = &result {
                    shared.error("qq_notify_owner", *error);
                }
                shared
                    .counters
                    .notifying_owner
                    .store(false, Ordering::Release);
                let _ = result_tx.try_send(result);
            },
        );
        if let Err(error) = result {
            self.shared
                .counters
                .notifying_owner
                .store(false, Ordering::Release);
            return Err(error);
        }
        Ok(result_rx)
    }

    pub fn start(
        config: Arc<Config>,
        control_tx: SyncSender<ControlRequest>,
        mode: BotMode,
        memory_client: Option<crate::platform::sdcard::MemoryClient>,
    ) -> Result<Self, Error> {
        config.validate().map_err(|_| Error::Config)?;
        if config.qq_app_id.is_empty()
            || config.qq_app_secret.is_empty()
            || config.allow_users.is_empty()
            || (mode == BotMode::DeepSeek
                && (config.deepseek_key.is_empty() || config.deepseek_model.is_empty()))
        {
            return Err(Error::Config);
        }
        let shared = Arc::new(Shared {
            config,
            mode,
            stop: Arc::new(AtomicBool::new(false)),
            cleanup_error: Arc::new(AtomicI32::new(0)),
            cleanup_active: Arc::new(AtomicBool::new(false)),
            counters: Counters::default(),
            sessions: Mutex::new(SessionBook::new()),
            tokens: http::TokenCache::new(),
            memory_client,
            memory_stats: memory::Stats::default(),
            vision_support: AtomicU32::new(0),
            delivery: protocol::streaming::DeliveryStats::default(),
        });
        let (out_tx, out_rx) = sync_channel(4);
        let (infer_tx, infer_rx) = sync_channel(2);
        let (done_tx, done_rx) = sync_channel(4);
        let mut threads = Vec::with_capacity(3);
        let state = shared.clone();
        spawn(&shared, &mut threads, "qq-send", 12288, move || {
            sender::run(state, out_rx)
        })?;
        if mode == BotMode::DeepSeek {
            let state = shared.clone();
            let responses = out_tx.clone();
            spawn(&shared, &mut threads, "deepseek", 16384, move || {
                inference::run(state, infer_rx, responses, done_tx)
            })?;
        } else {
            drop(infer_rx);
            drop(done_tx);
        }
        let state = shared.clone();
        spawn(&shared, &mut threads, "qq-gateway", 16384, move || {
            gateway::run(state, control_tx, infer_tx, out_tx, done_rx)
        })?;
        Ok(Self {
            shared,
            _threads: threads,
        })
    }

    /// Cooperative: poll is_stopped before starting another runtime or changing its config.
    pub fn stop(&self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared
            .counters
            .gateway_ready
            .store(false, Ordering::Relaxed);
    }
    pub fn cleanup_failed(&self) -> bool {
        self.shared.cleanup_error.load(Ordering::Acquire) != 0
    }
    pub fn is_stopped(&self) -> bool {
        self.shared.counters.active_workers.load(Ordering::Relaxed) == 0
    }
    pub fn status(&self) -> BotStatus {
        let c = &self.shared.counters;
        BotStatus {
            mode: self.shared.mode,
            gateway_ready: !self.shared.stop.load(Ordering::Relaxed)
                && c.gateway_ready.load(Ordering::Relaxed),
            received: c.received.load(Ordering::Relaxed),
            denied: c.denied.load(Ordering::Relaxed),
            group_events: c.group_events.load(Ordering::Relaxed),
            group_self_mentions: c.group_self_mentions.load(Ordering::Relaxed),
            group_ignored_without_mention: c.group_ignored_without_mention.load(Ordering::Relaxed),
            group_parse_failed: c.group_parse_failed.load(Ordering::Relaxed),
            group_denied: c.group_denied.load(Ordering::Relaxed),
            sent: c.sent.load(Ordering::Relaxed),
            errors: c.errors.load(Ordering::Relaxed),
            last_error: c.last_error.load(Ordering::Relaxed),
            active_workers: c.active_workers.load(Ordering::Relaxed),
            cleanup_failed: self.cleanup_failed(),
            cleanup_error_code: self.shared.cleanup_error.load(Ordering::Acquire),
            // This SDK owner is separate from the ordinary bot worker count.
            cleanup_worker_active: self.shared.cleanup_active.load(Ordering::Acquire),
            reconnects: c.reconnects.load(Ordering::Relaxed),
            server_reconnects: c.server_reconnects.load(Ordering::Relaxed),
            inference_started: c.inference_started.load(Ordering::Relaxed),
            inference_completed: c.inference_completed.load(Ordering::Relaxed),
            inference_failed: c.inference_failed.load(Ordering::Relaxed),
            inference_cancelled: c.inference_cancelled.load(Ordering::Relaxed),
            delivery: self.shared.delivery.snapshot(),
            recent_errors: c
                .history
                .lock()
                .map(|h| h.iter().cloned().collect())
                .unwrap_or_default(),
            memory: self.shared.memory_stats.status(),
        }
    }
}
impl Drop for BotRuntime {
    fn drop(&mut self) {
        self.stop();
    }
}

fn spawn(
    shared: &Arc<Shared>,
    threads: &mut Vec<JoinHandle<()>>,
    name: &str,
    stack: usize,
    worker: impl FnOnce() + Send + 'static,
) -> Result<(), Error> {
    let state = shared.clone();
    state
        .counters
        .active_workers
        .fetch_add(1, Ordering::Relaxed);
    match thread::Builder::new()
        .name(name.into())
        .stack_size(stack)
        .spawn(move || {
            worker();
            state
                .counters
                .active_workers
                .fetch_sub(1, Ordering::Relaxed);
        }) {
        Ok(handle) => {
            threads.push(handle);
            Ok(())
        }
        Err(error) => {
            shared
                .counters
                .active_workers
                .fetch_sub(1, Ordering::Relaxed);
            shared.stop.store(true, Ordering::Relaxed);
            // Drop JoinHandles to detach, never wait for network workers in the board loop.
            // They own Shared through Arc and will cooperatively retire. Services must latch
            // Error::Thread until reboot, so no replacement overlaps this failed startup.
            threads.clear();
            log::error!("bot stage=worker_start errno={:?}", error.raw_os_error());
            Err(Error::Thread)
        }
    }
}

struct Shared {
    config: Arc<Config>,
    mode: BotMode,
    stop: Arc<AtomicBool>,
    cleanup_error: Arc<AtomicI32>,
    cleanup_active: Arc<AtomicBool>,
    counters: Counters,
    sessions: Mutex<SessionBook>,
    tokens: http::TokenCache,
    memory_client: Option<crate::platform::sdcard::MemoryClient>,
    memory_stats: memory::Stats,
    vision_support: AtomicU32,
    delivery: protocol::streaming::DeliveryStats,
}
impl Shared {
    fn error(&self, stage: &'static str, error: Error) {
        self.counters.errors.fetch_add(1, Ordering::Relaxed);
        self.counters
            .last_error
            .store(error.number(), Ordering::Relaxed);
        if let Ok(mut history) = self.counters.history.lock() {
            if history.len() == 16 {
                history.pop_front();
            }
            if history.try_reserve(1).is_ok() {
                history.push_back(ErrorRecord {
                    origin: stage,
                    detail: error.to_string(),
                    code: error.number(),
                    unix_time: unix_now(),
                });
            }
        }
        log::warn!("bot stage={stage} error={error}");
    }
    fn network_headroom(&self) -> bool {
        const MIN_INTERNAL_FREE: usize = 80 * 1024;
        unsafe {
            esp_idf_svc::sys::heap_caps_get_free_size(
                esp_idf_svc::sys::MALLOC_CAP_INTERNAL | esp_idf_svc::sys::MALLOC_CAP_8BIT,
            ) >= MIN_INTERNAL_FREE
        }
    }
    fn current(&self, context: &ReplyContext) -> bool {
        !self.stop.load(Ordering::Relaxed)
            && unix_now().is_some_and(|now| now < context.deadline)
            && self
                .sessions
                .lock()
                .ok()
                .is_some_and(|book| book.matches(context))
    }
}

#[derive(Clone)]
struct ReplyContext {
    target: Target,
    message_id: String,
    key: String,
    generation: u64,
    deadline: u64,
    reply_budget: Arc<protocol::reply_format::Budget>,
}
struct InferenceJob {
    context: Arc<ReplyContext>,
    input: String,
    images: Vec<protocol::ImageAttachment>,
    files: Vec<protocol::file_attachment::FileAttachment>,
    reference: String,
}
enum OutboundBody {
    Text(String),
    Media(protocol::media_reply::MediaReply),
    Picture(qq_deepseek_core::material::PictureCommand),
    Typing,
    Stream(Arc<protocol::streaming::Mailbox>),
}
struct Outbound {
    context: Arc<ReplyContext>,
    body: OutboundBody,
}
struct Finished {
    key: String,
    generation: u64,
}

struct SessionEntry {
    key: String,
    generation: u64,
    history_version: u64,
}
struct SessionBook {
    entries: VecDeque<SessionEntry>,
    next: u64,
}
impl SessionBook {
    fn new() -> Self {
        Self {
            entries: VecDeque::with_capacity(8),
            next: 1,
        }
    }
    fn context(
        &mut self,
        message: &Message,
        inflight: &HashMap<String, u64>,
    ) -> Result<(Arc<ReplyContext>, u64), Error> {
        let key = message.session_key();
        let existing = self
            .entries
            .iter()
            .position(|entry| entry.key == key)
            .and_then(|n| self.entries.remove(n));
        let entry = if let Some(entry) = existing {
            entry
        } else {
            if self.entries.len() == 8 {
                let victim = self
                    .entries
                    .iter()
                    .position(|entry| !inflight.contains_key(&entry.key))
                    .ok_or(Error::Limit)?;
                self.entries.remove(victim);
            }
            let version = self.next;
            self.next = self.next.checked_add(1).ok_or(Error::State)?;
            SessionEntry {
                key: key.clone(),
                generation: version,
                history_version: version,
            }
        };
        let context = Arc::new(ReplyContext {
            target: message.target.clone(),
            message_id: message.message_id.clone(),
            key,
            generation: entry.generation,
            deadline: message.timestamp.saturating_add(protocol::MESSAGE_TTL_SECS),
            reply_budget: Arc::new(protocol::reply_format::Budget::default()),
        });
        let history_version = entry.history_version;
        self.entries.push_back(entry);
        Ok((context, history_version))
    }
    fn invalidate(&mut self, key: &str, reset_history: bool) -> Result<u64, Error> {
        let generation = self.next;
        self.next = self.next.checked_add(1).ok_or(Error::State)?;
        let entry = self
            .entries
            .iter_mut()
            .find(|entry| entry.key == key)
            .ok_or(Error::State)?;
        entry.generation = generation;
        if reset_history {
            entry.history_version = generation;
        }
        Ok(generation)
    }
    fn matches(&self, context: &ReplyContext) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.key == context.key && entry.generation == context.generation)
    }
}

fn unix_now() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}
fn sleep_until_retry(shared: &Shared, seconds: u64) {
    for _ in 0..seconds * 5 {
        if shared.stop.load(Ordering::Relaxed) {
            break;
        }
        thread::sleep(Duration::from_millis(200));
    }
}
