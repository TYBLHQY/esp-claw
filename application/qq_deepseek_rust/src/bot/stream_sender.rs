//! REST side of one live private stream, owned by the existing QQ sender thread.
use super::{
    http,
    protocol::{
        reply_format,
        streaming::{Cursor, Mailbox, Terminal},
        Target,
    },
    sender, Error, Outbound, OutboundBody, ReplyContext, Shared,
};
use std::sync::{atomic::Ordering, Arc};
use std::time::{Duration, Instant};

pub(super) struct Session {
    context: Arc<ReplyContext>,
    mailbox: Arc<Mailbox>,
    cursor: Cursor,
    fallback: bool,
    closing: bool,
    cleanup_started: Option<Instant>,
    staged_final: Option<String>,
    verify_image: bool,
}
impl Session {
    pub fn new(context: Arc<ReplyContext>, mailbox: Arc<Mailbox>) -> Self {
        Self {
            context,
            mailbox,
            cursor: Cursor::default(),
            fallback: false,
            closing: false,
            cleanup_started: None,
            staged_final: None,
            verify_image: false,
        }
    }
    pub fn staged(context: Arc<ReplyContext>, mailbox: Arc<Mailbox>, text: String) -> Self {
        let mut session = Self::new(context, mailbox);
        session.staged_final = Some(text);
        session.verify_image = true;
        session
    }
    /// true means the actor may drop this slot. Unknown POSTs are never replayed.
    pub fn poll(&mut self, shared: &Shared) -> bool {
        self.poll_at(shared, Instant::now())
    }
    pub fn poll_at(&mut self, shared: &Shared, now: Instant) -> bool {
        if shared.stop.load(Ordering::Relaxed) {
            self.mailbox.abandon();
            return true;
        }
        let Some(snapshot) = self.mailbox.snapshot() else {
            self.mailbox.abandon();
            return true;
        };
        let cancelled = !shared.current(&self.context);
        if cancelled && self.cursor.id.is_none() {
            self.mailbox.abandon();
            return true;
        }
        if cancelled || self.closing {
            let since = *self.cleanup_started.get_or_insert(now);
            if now.saturating_duration_since(since) >= Duration::from_secs(15) {
                self.mailbox.abandon();
                shared
                    .delivery
                    .stream_failed
                    .fetch_add(1, Ordering::Relaxed);
                shared.error("qq_stream_close_deadline", Error::Timeout);
                return true;
            }
        }
        if self.fallback {
            if cancelled {
                return true;
            }
            if snapshot.terminal == Terminal::Pending {
                return false;
            }
            let result = if self.verify_image {
                sender::send_picture_markdown(shared, &self.context, &snapshot.text)
            } else {
                sender::send_reply(
                    shared,
                    Outbound {
                        context: self.context.clone(),
                        body: OutboundBody::Text(snapshot.text),
                    },
                )
            };
            if let Err(e) = result {
                shared.error("qq_stream_fallback", e);
            }
            return true;
        }
        if !self.cursor.ready(now)
            || (!cancelled && !self.closing && !self.cursor.due(&snapshot, now))
        {
            return false;
        }
        let (text, done) = if self.closing {
            (
                format!("{}\n\n（流式更新未完成）", self.cursor.confirmed),
                true,
            )
        } else {
            let Some(value) = self.cursor.text(&snapshot, cancelled) else {
                if snapshot.terminal != Terminal::Pending {
                    self.mailbox.abandon();
                    shared.error("qq_stream_prefix", Error::Protocol);
                    return true;
                }
                return false;
            };
            value
        };
        if !shared.network_headroom() {
            return false;
        }
        let result = self.post(shared, &text, done, now);
        match result {
            Ok(None) => false,
            Ok(Some(id)) => {
                let first = self.cursor.id.is_none();
                if !self.cursor.accepted(&id, text, snapshot.revision, now) {
                    self.mailbox.abandon();
                    shared
                        .delivery
                        .stream_unknown
                        .fetch_add(1, Ordering::Relaxed);
                    shared.error("qq_stream_id", Error::Protocol);
                    return true;
                }
                shared
                    .delivery
                    .stream_frames
                    .fetch_add(1, Ordering::Relaxed);
                if first {
                    shared
                        .delivery
                        .stream_started
                        .fetch_add(1, Ordering::Relaxed);
                }
                if !done && !cancelled {
                    if let Some(final_text) = self.staged_final.take() {
                        self.mailbox.finish(&final_text, true);
                    }
                }
                if done {
                    if cancelled {
                        shared
                            .delivery
                            .stream_cancelled
                            .fetch_add(1, Ordering::Relaxed);
                    } else if self.closing || snapshot.terminal == Terminal::Failure {
                        shared
                            .delivery
                            .stream_failed
                            .fetch_add(1, Ordering::Relaxed);
                    } else {
                        shared
                            .delivery
                            .stream_completed
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    shared.counters.sent.fetch_add(1, Ordering::Relaxed);
                    return true;
                }
                false
            }
            Err(error) if explicit_rejection(error) && self.cursor.id.is_none() => {
                shared
                    .delivery
                    .stream_fallbacks
                    .fetch_add(1, Ordering::Relaxed);
                if let Some(final_text) = self.staged_final.take() {
                    self.mailbox.finish(&final_text, true);
                }
                self.fallback = true;
                false
            }
            Err(error) if explicit_rejection(error) && !self.closing => {
                self.mailbox.abandon();
                shared.error("qq_stream_update", error);
                self.closing = true;
                false
            }
            Err(error) => {
                self.mailbox.abandon();
                shared
                    .delivery
                    .stream_unknown
                    .fetch_add(1, Ordering::Relaxed);
                shared.error("qq_stream_send", error);
                true
            }
        }
    }
    fn post(
        &mut self,
        shared: &Shared,
        text: &str,
        done: bool,
        now: Instant,
    ) -> Result<Option<String>, Error> {
        let Target::User(id) = &self.context.target else {
            return Err(Error::Protocol.at("qq_stream_private_only"));
        };
        let token = shared.tokens.get(&shared.config, &shared.stop)?;
        if !shared.network_headroom() {
            return Ok(None);
        }
        let sequence = match self.cursor.sequence {
            Some(n) => n,
            None => {
                let n = self
                    .context
                    .reply_budget
                    .reserve()
                    .ok_or(Error::Limit.at("qq_stream_quota"))?;
                self.cursor.sequence = Some(n);
                n
            }
        };
        let index = self.cursor.reserve_index();
        let body = super::protocol::streaming::frame(
            text,
            &self.context.message_id,
            sequence,
            index,
            self.cursor.id.as_deref(),
            done,
        )
        .map_err(|_| Error::Limit.at("qq_stream_body"))?;
        let uri = format!("{}/v2/users/{id}/stream_messages", http::QQ_API);
        let authorization = format!("QQBot {token}");
        self.cursor.last_sent = Some(now);
        let response = http::message_request(
            &uri,
            &authorization,
            &shared.config.qq_app_id,
            &body,
            &shared.stop,
        )?;
        response["id"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
            .map(|s| Some(s.to_owned()))
            .ok_or(Error::Protocol.at("qq_stream_id"))
    }
}
fn explicit_rejection(error: Error) -> bool {
    match error {
        Error::Rejected { status, .. } => {
            reply_format::may_fallback(status) || matches!(status, 404 | 405)
        }
        // Official SDK identifies 50002 as rate limiting. Do not replay that request.
        Error::RemoteCode(code) => code != 50002,
        _ => false,
    }
}
