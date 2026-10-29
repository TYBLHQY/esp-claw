use super::gateway_socket::{Event, Socket};
use super::protocol::{
    gateway_uri_allowed, group_message_addressed_to, parse_message, Dedup, FrameAssembler,
};
use super::{
    http, sleep_until_retry, unix_now, BotMode, ControlReply, ControlRequest, DeviceCommand, Error,
    Finished, InferenceJob, Outbound, ReplyContext, Shared,
};
use esp_idf_svc::http::Method;
use qq_deepseek_core::{parse_command, Command};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

// GROUP_AND_C2C_EVENT (bit 25) is the official intent for both text channels.
const QQ_INTENTS: u64 = 1 << 25;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

struct Session {
    id: Option<String>,
    sequence: Option<i64>,
}
enum Reconnect {
    Stopped,
    ServerRequested,
    PeerClosed,
}
struct PendingControl {
    context: Arc<ReplyContext>,
    receiver: Receiver<ControlReply>,
    expires: Instant,
}
struct Router {
    controls: SyncSender<ControlRequest>,
    inference: SyncSender<InferenceJob>,
    outbound: SyncSender<Outbound>,
    completed: Receiver<Finished>,
    pending: VecDeque<PendingControl>,
    inflight: HashMap<String, u64>,
    dedup: Dedup,
}

pub(super) fn run(
    shared: Arc<Shared>,
    controls: SyncSender<ControlRequest>,
    inference: SyncSender<InferenceJob>,
    outbound: SyncSender<Outbound>,
    completed: Receiver<Finished>,
) {
    let mut router = Router {
        controls,
        inference,
        outbound,
        completed,
        pending: VecDeque::with_capacity(4),
        inflight: HashMap::with_capacity(3),
        dedup: Dedup::new(),
    };
    let mut session = Session {
        id: None,
        sequence: None,
    };
    let mut retry = 1;
    while !shared.stop.load(Ordering::Relaxed) {
        let connection_started = Instant::now();
        let result = connection(&shared, &mut session, &mut router);
        shared
            .counters
            .gateway_ready
            .store(false, Ordering::Relaxed);
        let cleanup_error = shared.cleanup_error.load(Ordering::Acquire);
        if cleanup_error != 0 {
            shared.error(
                "qq_gateway",
                Error::Diagnostic {
                    stage: "ws_cleanup",
                    code: cleanup_error,
                },
            );
            shared.stop.store(true, Ordering::Relaxed);
            break;
        }
        if shared.stop.load(Ordering::Relaxed) {
            break;
        }
        match result {
            Ok(Reconnect::Stopped) => break,
            Ok(Reconnect::ServerRequested) => {
                shared
                    .counters
                    .server_reconnects
                    .fetch_add(1, Ordering::Relaxed);
                log::info!("bot QQ server requested reconnect");
            }
            Ok(Reconnect::PeerClosed) => log::info!("bot QQ peer closed normally"),
            Err(error) => shared.error("qq_gateway", error),
        }
        shared.counters.reconnects.fetch_add(1, Ordering::Relaxed);
        router.poll(&shared);
        if connection_started.elapsed() >= Duration::from_secs(60) {
            retry = 1;
        }
        sleep_until_retry(&shared, retry);
        retry = (retry * 2).min(30);
    }
}

fn connection(
    shared: &Shared,
    session: &mut Session,
    router: &mut Router,
) -> Result<Reconnect, Error> {
    let token = shared.tokens.get(&shared.config, &shared.stop)?;
    let authorization = format!("QQBot {token}");
    let gateway = http::json_request(
        Method::Get,
        &format!("{}/gateway", http::QQ_API),
        Some(&authorization),
        Some(&shared.config.qq_app_id),
        &[],
        &shared.stop,
    )?;
    let uri = gateway
        .get("url")
        .and_then(Value::as_str)
        .filter(|url| gateway_uri_allowed(url))
        .ok_or(Error::Protocol)?;
    let (events_tx, events_rx) = sync_channel(24);
    let lost = Arc::new(AtomicBool::new(false));
    let mut socket = Socket::connect(
        uri,
        events_tx,
        lost.clone(),
        shared.stop.clone(),
        shared.cleanup_error.clone(),
        shared.cleanup_active.clone(),
    )?;
    let mut assembler = FrameAssembler::new();
    let opened = Instant::now();
    let mut interval = None;
    let mut next_heartbeat = Instant::now();
    let mut awaiting_ack = false;
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            return Ok(Reconnect::Stopped);
        }
        router.poll(shared);
        let now = Instant::now();
        if lost.load(Ordering::Relaxed) {
            return Err(Error::Limit);
        }
        if assembler.is_expired(now) {
            return Err(Error::Timeout);
        }
        if !shared.counters.gateway_ready.load(Ordering::Relaxed)
            && now.saturating_duration_since(opened) > Duration::from_secs(25)
        {
            return Err(Error::Timeout);
        }
        if let Some(period) = interval {
            if now >= next_heartbeat {
                if awaiting_ack {
                    return Err(Error::Timeout);
                }
                heartbeat(&mut socket, session.sequence)?;
                awaiting_ack = true;
                next_heartbeat = now + period;
            }
        }
        let event = match events_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return Err(Error::Transport),
        };
        match event {
            Event::Connected => {}
            Event::Closed(code) if matches!(code, 1000 | 1001) => return Ok(Reconnect::PeerClosed),
            Event::Closed(code) => {
                return Err(Error::Diagnostic {
                    stage: "ws_peer_close",
                    code: i32::from(code),
                })
            }
            Event::Disconnected => return Err(Error::Transport.at("ws_disconnected")),
            Event::Failed(error) => return Err(error),
            Event::Data(chunk) => {
                let Some(data) = assembler
                    .push(chunk, now)
                    .map_err(|_| Error::Protocol.at("gateway_fragment"))?
                else {
                    continue;
                };
                if !super::protocol::json_depth_allowed(&data, 24) {
                    return Err(Error::Limit.at("qq_json_depth"));
                }
                let payload: Value = serde_json::from_slice(&data)
                    .map_err(|_| Error::Protocol.at("gateway_json"))?;
                let op = payload
                    .get("op")
                    .and_then(Value::as_u64)
                    .ok_or(Error::Protocol)?;
                match op {
                    10 => {
                        let millis = payload
                            .pointer("/d/heartbeat_interval")
                            .and_then(Value::as_u64)
                            .filter(|n| (5000..=120000).contains(n))
                            .ok_or(Error::Protocol)?;
                        if interval.is_some() {
                            return Err(Error::Protocol);
                        }
                        let period = Duration::from_millis(millis);
                        interval = Some(period);
                        next_heartbeat = Instant::now() + period;
                        let identify = match (&session.id, session.sequence) {
                            (Some(id), Some(sequence)) => json!({
                                "op": 6, "d": {"token": authorization, "session_id": id, "seq": sequence},
                            }),
                            _ => json!({
                                "op": 2, "d": {"token": authorization, "intents": QQ_INTENTS, "shard": [0, 1]},
                            }),
                        };
                        socket.send_text(
                            &serde_json::to_vec(&identify).map_err(|_| Error::Protocol)?,
                        )?;
                    }
                    11 => {
                        awaiting_ack = false;
                    }
                    1 => {
                        heartbeat(&mut socket, session.sequence)?;
                        awaiting_ack = true;
                        // A server-requested heartbeat also starts a fresh ACK deadline.
                        // Otherwise one arriving just before the periodic tick can be
                        // mistaken for a missed ACK before the server has time to reply.
                        if let Some(period) = interval {
                            next_heartbeat = Instant::now() + period;
                        }
                    }
                    7 => return Ok(Reconnect::ServerRequested),
                    9 => {
                        session.id = None;
                        session.sequence = None;
                        return Err(Error::Auth);
                    }
                    0 => {
                        if let Some(sequence) =
                            payload.get("s").and_then(Value::as_i64).filter(|n| *n >= 0)
                        {
                            session.sequence = Some(sequence);
                        }
                        let kind = payload
                            .get("t")
                            .and_then(Value::as_str)
                            .ok_or(Error::Protocol)?;
                        let data = payload.get("d").ok_or(Error::Protocol)?;
                        if kind == "READY" {
                            let id = data
                                .get("session_id")
                                .and_then(Value::as_str)
                                .filter(|id| {
                                    !id.is_empty()
                                        && id.len() <= 128
                                        && !id.chars().any(char::is_control)
                                })
                                .ok_or(Error::Protocol)?;
                            session.id = Some(id.to_owned());
                            shared.counters.gateway_ready.store(true, Ordering::Relaxed);
                            log::info!("bot QQ gateway ready");
                        } else if kind == "RESUMED" {
                            shared.counters.gateway_ready.store(true, Ordering::Relaxed);
                            log::info!("bot QQ gateway resumed");
                        } else if shared.counters.gateway_ready.load(Ordering::Relaxed) {
                            router.dispatch(shared, kind, data)?;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

fn heartbeat(socket: &mut Socket, sequence: Option<i64>) -> Result<(), Error> {
    socket.send_text(
        &serde_json::to_vec(&json!({"op": 1, "d": sequence})).map_err(|_| Error::Protocol)?,
    )
}

impl Router {
    fn reply(&self, shared: &Shared, context: Arc<ReplyContext>, text: impl Into<String>) {
        if self
            .outbound
            .try_send(Outbound {
                context,
                body: super::OutboundBody::Text(text.into()),
            })
            .is_err()
        {
            shared.error("reply_queue", Error::Limit);
        }
    }

    fn poll(&mut self, shared: &Shared) {
        while let Ok(done) = self.completed.try_recv() {
            if self.inflight.get(&done.key) == Some(&done.generation) {
                self.inflight.remove(&done.key);
            }
        }
        let count = self.pending.len();
        for _ in 0..count {
            let Some(pending) = self.pending.pop_front() else {
                break;
            };
            if !shared.current(&pending.context) {
                continue;
            }
            match pending.receiver.try_recv() {
                Ok(reply) => {
                    let text = if reply.success {
                        reply.text
                    } else {
                        format!("控制失败：{}", reply.text)
                    };
                    // The board bridge is trusted but its reply must still fit a bounded QQ result.
                    if text.len() <= 8192 {
                        self.reply(shared, pending.context, text);
                    } else {
                        self.reply(shared, pending.context, "控制响应超限。");
                    }
                }
                Err(TryRecvError::Empty) if Instant::now() < pending.expires => {
                    self.pending.push_back(pending)
                }
                _ => self.reply(
                    shared,
                    pending.context,
                    "控制结果尚未确认，请用 /status 核对。",
                ),
            }
        }
    }

    fn dispatch(&mut self, shared: &Shared, kind: &str, data: &Value) -> Result<(), Error> {
        let group = matches!(kind, "GROUP_AT_MESSAGE_CREATE" | "GROUP_MESSAGE_CREATE");
        if group {
            shared.counters.group_events.fetch_add(1, Ordering::Relaxed);
            if data
                .get("mentions")
                .and_then(Value::as_array)
                .is_some_and(|mentions| {
                    mentions
                        .iter()
                        .any(|item| item.get("is_you").and_then(Value::as_bool) == Some(true))
                })
            {
                shared
                    .counters
                    .group_self_mentions
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
        if kind == "GROUP_MESSAGE_CREATE"
            && !group_message_addressed_to(data, &shared.config.qq_app_id)
        {
            shared
                .counters
                .group_ignored_without_mention
                .fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        let Some(message) = parse_message(kind, data) else {
            if group {
                shared
                    .counters
                    .group_parse_failed
                    .fetch_add(1, Ordering::Relaxed);
            }
            return Ok(());
        };
        shared.counters.received.fetch_add(1, Ordering::Relaxed);
        if !message.is_authorized(&shared.config.allow_users, &shared.config.allow_groups) {
            shared.counters.denied.fetch_add(1, Ordering::Relaxed);
            if group {
                shared.counters.group_denied.fetch_add(1, Ordering::Relaxed);
            }
            return Ok(());
        }
        if !unix_now().is_some_and(|now| message.is_fresh(now))
            || !self.dedup.admit(&message.message_id)
        {
            return Ok(());
        }
        let (context, _history_version) = {
            let mut book = shared.sessions.lock().map_err(|_| Error::State)?;
            book.context(&message, &self.inflight)?
        };
        let stream_requested = message.reference.is_empty()
            && message.images.is_empty()
            && message.files.is_empty()
            && message.content.trim().starts_with("/stream ");
        let text = if stream_requested {
            message
                .content
                .trim()
                .strip_prefix("/stream ")
                .unwrap()
                .trim()
        } else {
            message.content.trim()
        };
        if let Some(error) = message.image_error {
            self.reply(shared, context, error.message());
            return Ok(());
        }
        let web_task = match qq_deepseek_core::web_tools::WebTask::parse(text) {
            Ok(task) => task,
            Err(_) => {
                self.reply(
                    shared,
                    context,
                    "搜索词或网页地址不正确，请使用 /search 查询 或 /fetch https://网页地址。",
                );
                return Ok(());
            }
        };
        if !message.images.is_empty() && web_task.is_some() {
            self.reply(
                shared,
                context,
                "Please send image and web-search requests separately.",
            );
            return Ok(());
        }
        if message.reference.is_empty() && message.images.is_empty() && message.files.is_empty() {
            match qq_deepseek_core::material::PictureCommand::parse(text) {
                Ok(Some(command)) => {
                    if matches!(
                        &command,
                        qq_deepseek_core::material::PictureCommand::Send { stream: true, .. }
                    ) && !matches!(context.target, super::protocol::Target::User(_))
                    {
                        self.reply(
                            shared,
                            context,
                            "流式图文测试仅限私聊；群聊请用 /picture 文件名 说明。",
                        );
                        return Ok(());
                    }
                    if self
                        .outbound
                        .try_send(Outbound {
                            context: context.clone(),
                            body: super::OutboundBody::Picture(command),
                        })
                        .is_err()
                    {
                        self.reply(shared, context, "发送队列繁忙，请稍后重试。");
                    }
                    return Ok(());
                }
                Err(error) => {
                    self.reply(shared, context, error);
                    return Ok(());
                }
                Ok(None) => (),
            }
            match super::protocol::media_reply::MediaReply::parse(text) {
                Ok(Some(media)) => {
                    if self
                        .outbound
                        .try_send(Outbound {
                            context: context.clone(),
                            body: super::OutboundBody::Media(media),
                        })
                        .is_err()
                    {
                        self.reply(shared, context, "回复队列繁忙，请稍后重试。");
                    }
                    return Ok(());
                }
                Err(error) => {
                    self.reply(shared, context, error);
                    return Ok(());
                }
                Ok(None) => {}
            }
        }
        if let Some(command) = text.strip_prefix('/').filter(|_| {
            !stream_requested
                && web_task.is_none()
                && message.images.is_empty()
                && message.reference.is_empty()
                && !super::memory::is_command(text)
        }) {
            if command == "ping" {
                self.reply(shared, context, "pong");
                return Ok(());
            }
            if let Some(echo) = command.strip_prefix("echo ") {
                self.reply(shared, context, echo);
                return Ok(());
            }
            if command == "stop" {
                let generation = shared
                    .sessions
                    .lock()
                    .map_err(|_| Error::State)?
                    .invalidate(&context.key, false)?;
                let mut context = (*context).clone();
                context.generation = generation;
                self.reply(shared, Arc::new(context), "已取消本会话待发送的模型结果。");
                return Ok(());
            }
            let command = match parse_command(text) {
                Ok(Command::Status) => DeviceCommand::Status,
                Ok(Command::Led(command)) => DeviceCommand::Led(command),
                Ok(Command::Help) => {
                    self.reply(
                        shared,
                        context,
                        "CopilotLee: /ping /echo TEXT /status /led on|off|rgb R G B|status /reset /stop /stream PROMPT (private streaming is automatic) /picture list | NAME CAPTION /picture-stream NAME CAPTION /image HTTPS_URL /file HTTPS_URL /search QUERY /fetch URL /memory /memory search QUERY /remember TEXT /forget INDEX|all /memory compact. Send up to two images with a question; quotes, forwards, cards and QQ voice transcripts are supported. UTF-8 text/code files up to 16 KiB can be read automatically. Audio without a transcript and PDF/Office/video contents are not understood.",
                    );
                    return Ok(());
                }
                _ => {
                    self.reply(shared, context, "无效或未开放的远程指令，请用 /help 查看。");
                    return Ok(());
                }
            };
            if self.pending.len() == 4 {
                self.reply(shared, context, "控制队列繁忙，请稍后重试。");
                return Ok(());
            }
            let (respond_to, receiver) = sync_channel(1);
            if self
                .controls
                .try_send(ControlRequest {
                    command,
                    respond_to,
                })
                .is_err()
            {
                self.reply(shared, context, "控制通路暂不可用。");
            } else {
                self.pending.push_back(PendingControl {
                    context,
                    receiver,
                    expires: Instant::now() + CONTROL_TIMEOUT,
                });
            }
            return Ok(());
        }
        if shared.mode == BotMode::EchoOnly {
            self.reply(
                shared,
                context,
                "当前为 QQ 回环验收模式；可发送 /ping、/echo 文字或 /led 指令。",
            );
            return Ok(());
        }
        let reset = text == "/reset" && message.reference.is_empty() && message.images.is_empty();
        let context = if reset {
            // Cancel the old model result; the inference owner clears both its RAM
            // state and the durable record before acknowledging the reset.
            let generation = shared
                .sessions
                .lock()
                .map_err(|_| Error::State)?
                .invalidate(&context.key, false)?;
            let mut next = (*context).clone();
            next.generation = generation;
            Arc::new(next)
        } else {
            context
        };
        if self.inflight.contains_key(&context.key) && !reset {
            self.reply(shared, context, "本会话已有请求，请等待或先 /stop。");
            return Ok(());
        }
        let key = context.key.clone();
        let generation = context.generation;
        let job = InferenceJob {
            context: context.clone(),
            input: if stream_requested {
                text.to_owned()
            } else {
                message.content
            },
            images: message.images,
            files: message.files,
            reference: message.reference,
        };
        if self.inference.try_send(job).is_err() {
            self.reply(shared, context, "请求队列繁忙，请稍后再试。");
        } else {
            self.inflight.insert(key, generation);
        }
        Ok(())
    }
}
