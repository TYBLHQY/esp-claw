//! One bounded progress mailbox; no worker, socket, persistent token or queue of deltas.
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

pub const UPDATE_INTERVAL: Duration = Duration::from_millis(1000);
pub const MAX_PROGRESS_BYTES: usize = 5400;
pub const MAX_STREAM_BYTES: usize = 6000;
pub const MIN_PROGRESS_BYTES: usize = 96;
const TRUNCATED: &str = "\n…（回答已截断）";
#[derive(Default)]
pub struct DeliveryStats {
    pub typing_sent: AtomicU32,
    pub typing_skipped: AtomicU32,
    pub stream_started: AtomicU32,
    pub stream_frames: AtomicU32,
    pub stream_completed: AtomicU32,
    pub stream_fallbacks: AtomicU32,
    pub stream_unknown: AtomicU32,
    pub stream_cancelled: AtomicU32,
    pub stream_failed: AtomicU32,
    slot: Arc<AtomicBool>,
}
impl DeliveryStats {
    pub fn snapshot(&self) -> Value {
        json!({"typing_sent":self.typing_sent.load(Ordering::Relaxed),"typing_skipped":self.typing_skipped.load(Ordering::Relaxed),"stream_started":self.stream_started.load(Ordering::Relaxed),"stream_frames":self.stream_frames.load(Ordering::Relaxed),"stream_completed":self.stream_completed.load(Ordering::Relaxed),"stream_fallbacks":self.stream_fallbacks.load(Ordering::Relaxed),"stream_unknown":self.stream_unknown.load(Ordering::Relaxed),"stream_cancelled":self.stream_cancelled.load(Ordering::Relaxed),"stream_failed":self.stream_failed.load(Ordering::Relaxed),"active_stream_slot":self.slot.load(Ordering::Relaxed),"progress_budget_bytes":MAX_STREAM_BYTES,"update_interval_ms":UPDATE_INTERVAL.as_millis()})
    }
    pub fn acquire(&self) -> Option<Arc<Mailbox>> {
        self.slot
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .ok()?;
        Some(Arc::new(Mailbox {
            slot: self.slot.clone(),
            state: Mutex::new(Snapshot {
                text: String::new(),
                terminal: Terminal::Pending,
                revision: 0,
                after_tools: false,
            }),
            last_offer: Mutex::new(Instant::now() - UPDATE_INTERVAL),
            aborted: AtomicBool::new(false),
        }))
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Terminal {
    Pending,
    Success,
    Failure,
}
#[derive(Clone)]
pub struct Snapshot {
    pub text: String,
    pub terminal: Terminal,
    pub revision: u32,
    pub after_tools: bool,
}
pub struct Mailbox {
    slot: Arc<AtomicBool>,
    state: Mutex<Snapshot>,
    last_offer: Mutex<Instant>,
    aborted: AtomicBool,
}
impl Drop for Mailbox {
    fn drop(&mut self) {
        self.slot.store(false, Ordering::Release);
    }
}
impl Mailbox {
    pub fn abandon(&self) {
        self.aborted.store(true, Ordering::Release);
    }
    pub fn aborted(&self) -> bool {
        self.aborted.load(Ordering::Acquire)
    }
    pub fn offer(&self, raw: &str, secrets: &[&str], now: Instant) {
        if self.aborted() {
            return;
        }
        let Ok(mut last) = self.last_offer.try_lock() else {
            return;
        };
        if now.saturating_duration_since(*last) < UPDATE_INTERVAL {
            return;
        }
        let Some(text) = stable_prefix(raw, secrets) else {
            return;
        };
        if text.len() < MIN_PROGRESS_BYTES {
            return;
        }
        let Ok(mut state) = self.state.try_lock() else {
            return;
        };
        if state.terminal != Terminal::Pending
            || state.after_tools
            || text == state.text
            || !text.starts_with(&state.text)
        {
            return;
        }
        state.text = text;
        state.revision = state.revision.saturating_add(1);
        *last = now;
    }
    /// Freeze any visible lead-in once the model starts calling tools.
    pub fn seal_for_tools(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.after_tools = true;
        }
    }
    /// Explicit precomputed material preview; model deltas still use offer's throttle/guard.
    pub fn preview(&self, text: &str) {
        if let Ok(mut state) = self.state.lock() {
            if state.terminal == Terminal::Pending && state.text.is_empty() {
                state.text = clip_answer(text);
                state.revision = state.revision.saturating_add(1);
            }
        }
    }
    pub fn finish(&self, text: &str, success: bool) {
        if let Ok(mut state) = self.state.lock() {
            if state.terminal != Terminal::Pending {
                return;
            }
            state.text = clip_answer(text);
            state.terminal = if success {
                Terminal::Success
            } else {
                Terminal::Failure
            };
            state.revision = state.revision.saturating_add(1);
        }
    }
    pub fn snapshot(&self) -> Option<Snapshot> {
        self.state.lock().ok().map(|v| v.clone())
    }
}
fn boundary(text: &str, max: usize) -> usize {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}
fn visible(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() && !matches!(c, '\n' | '\r' | '\t') {
                '�'
            } else {
                c
            }
        })
        .collect()
}
fn bounded_visible(text: &str, max: usize) -> String {
    let mut out = String::new();
    for c in text.chars() {
        let c = if c.is_control() && !matches!(c, '\n' | '\r' | '\t') {
            '�'
        } else {
            c
        };
        if out.len() + c.len_utf8() > max {
            break;
        }
        out.push(c);
    }
    out
}
pub fn clip_answer(text: &str) -> String {
    let text = visible(text);
    if text.len() <= MAX_STREAM_BYTES {
        return text;
    }
    format!(
        "{}{}",
        &text[..boundary(&text, MAX_STREAM_BYTES - TRUNCATED.len())],
        TRUNCATED
    )
}
/// Hold enough undecided raw bytes that a secret split across SSE chunks cannot leak
/// and redaction cannot later rewrite an already-delivered QQ prefix.
pub fn stable_prefix(raw: &str, secrets: &[&str]) -> Option<String> {
    let hold = secrets.iter().map(|s| s.len()).max().unwrap_or(0).max(1) - 1;
    if hold > 1024 || raw.len() <= hold {
        return None;
    }
    let mut end = boundary(raw, raw.len() - hold);
    loop {
        let old = end;
        for secret in secrets.iter().filter(|s| !s.is_empty()) {
            for (start, _) in raw.match_indices(*secret) {
                if start < end && start + secret.len() > end {
                    end = end.min(start);
                }
            }
        }
        if old == end {
            break;
        }
    }
    let mut text = raw[..end].to_owned();
    for secret in secrets.iter().filter(|s| !s.is_empty()) {
        text = text.replace(*secret, "[redacted]");
    }
    Some(bounded_visible(&text, MAX_PROGRESS_BYTES))
}
#[derive(Default)]
pub struct Cursor {
    pub id: Option<String>,
    pub sequence: Option<u32>,
    pub index: u32,
    pub confirmed: String,
    pub revision: u32,
    pub last_sent: Option<Instant>,
}
impl Cursor {
    pub fn ready(&self, now: Instant) -> bool {
        self.last_sent
            .is_none_or(|last| now.saturating_duration_since(last) >= UPDATE_INTERVAL)
    }
    pub fn due(&self, snapshot: &Snapshot, now: Instant) -> bool {
        self.ready(now)
            && (snapshot.terminal != Terminal::Pending || snapshot.revision > self.revision)
    }
    pub fn text(&self, snapshot: &Snapshot, cancelled: bool) -> Option<(String, bool)> {
        if cancelled {
            return Some((format!("{}\n\n（已停止）", self.confirmed), true));
        }
        if snapshot.terminal == Terminal::Failure {
            return Some((
                clip_answer(&format!("{}\n\n{}", self.confirmed, snapshot.text)),
                true,
            ));
        }
        if snapshot.terminal == Terminal::Success
            && snapshot.after_tools
            && !self.confirmed.is_empty()
            && !snapshot.text.starts_with(&self.confirmed)
        {
            return Some((
                clip_answer(&format!("{}\n\n{}", self.confirmed, snapshot.text)),
                true,
            ));
        }
        if snapshot.text.is_empty() || !snapshot.text.starts_with(&self.confirmed) {
            return None;
        }
        Some((
            snapshot.text.clone(),
            snapshot.terminal == Terminal::Success,
        ))
    }
    /// Advance before POST, including attempts that receive an explicit rejection.
    pub fn reserve_index(&mut self) -> u32 {
        let index = self.index;
        self.index = self.index.saturating_add(1);
        index
    }
    pub fn accepted(&mut self, id: &str, text: String, revision: u32, now: Instant) -> bool {
        if id.is_empty() || id.len() > 256 || id.chars().any(char::is_control) {
            return false;
        }
        if self.id.is_none() {
            self.id = Some(id.into());
        }
        self.confirmed = text;
        self.revision = revision;
        self.last_sent = Some(now);
        true
    }
}
/// Borrow strings while encoding; no Value tree or second full-content clone.
pub fn frame(
    text: &str,
    msg_id: &str,
    sequence: u32,
    index: u32,
    id: Option<&str>,
    done: bool,
) -> Result<Vec<u8>, ()> {
    #[derive(Serialize)]
    struct Frame<'a> {
        input_mode: &'static str,
        input_state: u8,
        content_type: &'static str,
        content_raw: &'a str,
        event_id: &'a str,
        msg_id: &'a str,
        msg_seq: u32,
        index: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        stream_msg_id: Option<&'a str>,
    }
    if text.len() > MAX_STREAM_BYTES
        || text
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
        || msg_id.len() > 256
        || msg_id.chars().any(char::is_control)
        || id.is_some_and(|id| id.len() > 256 || id.chars().any(char::is_control))
    {
        return Err(());
    }
    let body = serde_json::to_vec(&Frame {
        input_mode: "replace",
        input_state: if done { 10 } else { 1 },
        content_type: "markdown",
        content_raw: text,
        event_id: msg_id,
        msg_id,
        msg_seq: sequence,
        index,
        stream_msg_id: id,
    })
    .map_err(|_| ())?;
    if body.len() > 16 * 1024 {
        return Err(());
    }
    Ok(body)
}
pub fn typing(msg_id: &str, seq: u32) -> Value {
    json!({"msg_type":6,"input_notify":{"input_type":1,"input_second":60},"msg_id":msg_id,"msg_seq":seq})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn split_secret_never_appears_and_committed_prefixes_never_change() {
        let raw="你好 This is an example with secret-fixture-and-a-long-key followed by a useful longer answer.";
        let secrets = ["secret-fixture-and-a-long-key"];
        let mut previous = String::new();
        for end in 0..=raw.len() {
            if raw.is_char_boundary(end) {
                if let Some(current) = stable_prefix(&raw[..end], &secrets) {
                    assert!(current.starts_with(&previous));
                    assert!(!current.contains("secret-fixture"));
                    previous = current;
                }
            }
        }
        assert!(previous.contains("[redacted]"));
    }
    #[test]
    fn mailbox_coalesces_progress_and_has_one_slot_with_bounded_final_text() {
        let stats = DeliveryStats::default();
        let m = stats.acquire().unwrap();
        assert!(stats.acquire().is_none());
        m.offer(&"A".repeat(200), &[], Instant::now());
        let a = m.snapshot().unwrap();
        assert!(!a.text.is_empty());
        m.offer(&"A".repeat(300), &[], Instant::now());
        assert_eq!(m.snapshot().unwrap().revision, a.revision);
        m.finish(&"好".repeat(4000), true);
        let s = m.snapshot().unwrap();
        assert!(s.text.len() <= MAX_STREAM_BYTES && s.text.ends_with("（回答已截断）"));
        drop(m);
        assert!(stats.acquire().is_some());
    }
    #[test]
    fn stream_frames_keep_one_seq_and_increasing_index_and_done_state() {
        let mut c = Cursor {
            sequence: Some(2),
            ..Default::default()
        };
        let body = frame("Hello", "trigger", 2, c.reserve_index(), None, false).unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["index"], 0);
        assert_eq!(v["input_state"], 1);
        assert!(v.get("stream_msg_id").is_none());
        assert!(c.accepted("stream-id", "Hello".into(), 1, Instant::now()));
        let body = frame(
            "Hello world",
            "trigger",
            c.sequence.unwrap(),
            c.reserve_index(),
            c.id.as_deref(),
            true,
        )
        .unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["msg_seq"], 2);
        assert_eq!(v["index"], 1);
        assert_eq!(v["input_state"], 10);
        assert_eq!(v["stream_msg_id"], "stream-id");
    }
    #[test]
    fn cancellation_never_reveals_pending_unconfirmed_tokens() {
        let c = Cursor {
            confirmed: "visible".into(),
            ..Default::default()
        };
        let snapshot = Snapshot {
            text: "unconfirmed private data".into(),
            terminal: Terminal::Pending,
            revision: 2,
            after_tools: false,
        };
        let (text, done) = c.text(&snapshot, true).unwrap();
        assert!(done);
        assert!(text.contains("visible") && !text.contains("private data"));
    }
    #[test]
    fn escaped_frames_are_bounded_and_invalid_controls_are_rejected() {
        let body = frame(&"\n".repeat(MAX_STREAM_BYTES), "trigger", 1, 0, None, true).unwrap();
        assert!(body.len() <= 16 * 1024);
        assert!(frame("bad\0", "trigger", 1, 0, None, false).is_err());
        assert!(frame("ok", "bad\ntrigger", 1, 0, None, false).is_err());
        assert!(frame("ok", "trigger", 1, 0, Some(&"x".repeat(257)), false).is_err());
    }
    #[test]
    fn tool_transition_freezes_progress_then_appends_answer_to_confirmed_lead_in() {
        let stats = DeliveryStats::default();
        let m = stats.acquire().unwrap();
        let now = Instant::now();
        m.offer(&"Checking the source. ".repeat(8), &[], now);
        let lead = m.snapshot().unwrap().text;
        m.seal_for_tools();
        m.offer(
            "tool arguments must stay internal",
            &[],
            now + UPDATE_INTERVAL,
        );
        assert_eq!(m.snapshot().unwrap().text, lead);
        m.finish("Answer with real source https://example.com/", true);
        let c = Cursor {
            confirmed: lead.clone(),
            ..Default::default()
        };
        let (text, done) = c.text(&m.snapshot().unwrap(), false).unwrap();
        assert!(done);
        assert!(text.starts_with(&lead));
        assert!(text.contains("https://example.com/"));
        assert!(!text.contains("arguments"));
    }
}
