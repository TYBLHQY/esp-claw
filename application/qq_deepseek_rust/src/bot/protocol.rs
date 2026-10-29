//! Pure, bounded QQ wire handling. No hardware resources or credentials live here.
#[path = "file_attachment.rs"]
pub mod file_attachment;
#[path = "media_reply.rs"]
pub mod media_reply;
#[path = "message_content.rs"]
pub mod message_content;
#[path = "reply_format.rs"]
pub mod reply_format;
#[path = "streaming.rs"]
pub mod streaming;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub const MAX_GATEWAY_MESSAGE: usize = 32 * 1024;
pub const MAX_INPUT: usize = 8 * 1024;
pub const MESSAGE_TTL_SECS: u64 = 280;
const FRAGMENT_TIMEOUT: Duration = Duration::from_secs(10);
const DEDUP_SIZE: usize = 64;
const MAX_ID: usize = 128;
#[cfg(test)]
const REPLY_PART_BYTES: usize = 1800;
#[cfg(test)]
const MAX_REPLY_PARTS: usize = 4;
pub const MAX_NOTIFICATION_BYTES: usize = 1800;
pub const MAX_IMAGE_URL_BYTES: usize = 4096;
const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_IMAGE_DIMENSION: u64 = 8192;

/// Reject excessive nesting before serde's recursive Value parser uses worker stack.
/// This is a resource preflight, not a replacement for JSON syntax validation.
pub fn json_depth_allowed(bytes: &[u8], maximum: usize) -> bool {
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for &b in bytes {
        if quoted {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                quoted = false;
            }
        } else {
            match b {
                b'"' => quoted = true,
                b'{' | b'[' => {
                    depth += 1;
                    if depth > maximum {
                        return false;
                    }
                }
                b'}' | b']' => {
                    if depth == 0 {
                        return false;
                    }
                    depth -= 1;
                }
                _ => {}
            }
        }
    }
    depth == 0 && !quoted
}

/// Build a proactive text request with no passive trigger fields, bounded after JSON escaping.
pub fn notification_payload(text: &str) -> Result<Vec<u8>, &'static str> {
    if text.trim().is_empty() {
        return Err("empty_notification");
    }
    if text.len() > MAX_NOTIFICATION_BYTES {
        return Err("notification_limit");
    }
    #[derive(Serialize)]
    struct Notification<'a> {
        content: &'a str,
        msg_type: u8,
    }
    let body = serde_json::to_vec(&Notification {
        content: text,
        msg_type: 0,
    })
    .map_err(|_| "notification_encode")?;
    if body.len() > MAX_NOTIFICATION_BYTES {
        return Err("notification_limit");
    }
    Ok(body)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    User(String),
    Group(String),
}

#[derive(Clone)]
pub struct Message {
    pub target: Target,
    pub author: String,
    pub message_id: String,
    pub content: String,
    pub images: Vec<ImageAttachment>,
    pub files: Vec<file_attachment::FileAttachment>,
    pub reference: String,
    pub image_error: Option<ImageError>,
    pub timestamp: u64,
}

/// A transient QQ attachment reference. Deliberately not Debug/Serialize: a signed
/// CDN URL is never a diagnostic field or a persistent conversation-history item.
#[derive(Clone)]
pub struct ImageAttachment {
    pub url: String,
    pub content_type: String,
    pub size: Option<u64>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ImageError {
    TooMany,
    UnsupportedFormat,
    Url,
    Metadata,
    TooLarge,
    Dimensions,
    PromptLimit,
}

impl ImageError {
    pub fn message(self) -> &'static str {
        match self {
            Self::TooMany => "目前一次最多支持两张图片，请减少图片后发送。",
            Self::UnsupportedFormat => "目前只支持 JPEG、PNG、GIF、WebP 图片。",
            Self::Url => "图片地址不可用或不符合 HTTPS 公网地址要求，请重新发送图片。",
            Self::Metadata => "图片附件信息不完整或格式不正确，请重新发送。",
            Self::TooLarge => "图片附件超过 8 MiB，请压缩后重新发送。",
            Self::Dimensions => "图片单边超过 8192 像素，请缩小后重新发送。",
            Self::PromptLimit => "图片说明或请求内容过长，请缩短后重试。",
        }
    }
}

impl ImageAttachment {
    pub fn validate(&self) -> Result<(), ImageError> {
        validate_image_url(&self.url)?;
        image_content_type(&self.content_type)?;
        if self.size.is_some_and(|size| size > MAX_IMAGE_BYTES) {
            return Err(ImageError::TooLarge);
        }
        Ok(())
    }
}

impl Message {
    pub fn session_key(&self) -> String {
        match &self.target {
            Target::User(id) => format!("u:{id}"),
            Target::Group(id) => format!("g:{id}:{}", self.author),
        }
    }

    pub fn is_authorized(&self, users: &[String], groups: &[String]) -> bool {
        // Empty lists deny access. A group never confers permission to an unlisted sender.
        users.contains(&self.author)
            && match &self.target {
                Target::User(_) => true,
                Target::Group(id) => groups.contains(id),
            }
    }

    pub fn is_fresh(&self, unix_now: u64) -> bool {
        self.timestamp <= unix_now.saturating_add(30)
            && unix_now < self.timestamp.saturating_add(MESSAGE_TTL_SECS)
    }
}

/// Full-group events need an explicit self mention before routing. Missing or
/// ambiguous metadata must never turn ordinary group conversation into a prompt.
pub fn group_message_addressed_to(data: &Value, app_id: &str) -> bool {
    if valid_id(app_id).is_none() {
        return false;
    }
    // QQ's server-marked self flag is authoritative even when the group-space
    // mention OpenID differs from READY.user.id and the configured AppID.
    if data
        .get("mentions")
        .and_then(Value::as_array)
        .is_some_and(|mentions| {
            mentions
                .iter()
                .any(|mention| mention.get("is_you").and_then(Value::as_bool) == Some(true))
        })
    {
        return true;
    }
    // The official SDK's fallback uses the AppID, never a learned mention ID.
    data.get("content")
        .and_then(Value::as_str)
        .is_some_and(|text| {
            text.match_indices("<@").any(|(start, _)| {
                let mention = &text[start + 2..];
                mention
                    .strip_prefix('!')
                    .unwrap_or(mention)
                    .strip_prefix(app_id)
                    .is_some_and(|suffix| suffix.starts_with('>'))
            })
        })
}

pub fn parse_message(kind: &str, data: &Value) -> Option<Message> {
    let (target, author) = match kind {
        "C2C_MESSAGE_CREATE" => {
            let id = data.pointer("/author/user_openid")?.as_str()?;
            (Target::User(valid_id(id)?.to_owned()), id)
        }
        "GROUP_AT_MESSAGE_CREATE" | "GROUP_MESSAGE_CREATE" => {
            let group = data.get("group_openid")?.as_str()?;
            let sender = data.pointer("/author/member_openid")?.as_str()?;
            (Target::Group(valid_id(group)?.to_owned()), sender)
        }
        _ => return None,
    };
    let author = valid_id(author)?.to_owned();
    let id = data.get("id")?.as_str()?;
    if id.is_empty() || id.len() > 256 || id.chars().any(char::is_control) {
        return None;
    }
    let mut text = match data.get("content") {
        None | Some(Value::Null) => "",
        Some(Value::String(text)) => text.trim(),
        _ => return None,
    };
    if matches!(target, Target::Group(_)) && text.starts_with("<@") {
        if let Some(end) = text.find('>').filter(|end| *end < MAX_ID + 5) {
            text = text[end + 1..].trim_start();
        }
    }
    let content = message_content::normalize(data, text)?;
    let timestamp = parse_timestamp(data.get("timestamp")?.as_str()?)?;
    Some(Message {
        target,
        author,
        message_id: id.to_owned(),
        content: content.text,
        images: content.images,
        files: content.files,
        reference: content.reference,
        image_error: content.image_error,
        timestamp,
    })
}

fn metadata_number(item: &Value, field: &str) -> Result<Option<u64>, ImageError> {
    match item.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_u64().map(Some).ok_or(ImageError::Metadata),
    }
}

fn image_content_type(content_type: &str) -> Result<&'static str, ImageError> {
    match content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "image/jpeg" | "image/jpg" => Ok("image/jpeg"),
        "image/png" => Ok("image/png"),
        "image/gif" => Ok("image/gif"),
        "image/webp" => Ok("image/webp"),
        _ => Err(ImageError::UnsupportedFormat),
    }
}

fn validate_image_url(url: &str) -> Result<(), ImageError> {
    if url.is_empty()
        || url.len() > MAX_IMAGE_URL_BYTES
        || url.chars().any(|c| c.is_control() || c.is_whitespace())
        || url.contains(['\\', '<', '>', '"'])
        || !url
            .get(..8)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
    {
        return Err(ImageError::Url);
    }
    let authority_len = url[8..].find(['/', '?', '#']).unwrap_or(url.len() - 8);
    // Reuse the shared public-host/IP checks without inheriting Fetch's shorter
    // full-URL budget. The complete path/query above remains bounded separately.
    qq_deepseek_core::web_tools::validate_url(&url[..8 + authority_len])
        .map_err(|_| ImageError::Url)
}

fn valid_id(id: &str) -> Option<&str> {
    (!id.is_empty()
        && id.len() <= MAX_ID
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
    .then_some(id)
}

/// Accept ISO-8601 timestamps with an explicit offset; reject malformed calendar dates.
pub fn parse_timestamp(text: &str) -> Option<u64> {
    let b = text.as_bytes();
    if b.len() < 20
        || b.get(4) != Some(&b'-')
        || b.get(7) != Some(&b'-')
        || !matches!(b.get(10), Some(b'T' | b't'))
        || b.get(13) != Some(&b':')
        || b.get(16) != Some(&b':')
    {
        return None;
    }
    let number = |start: usize, count: usize| -> Option<i64> {
        b.get(start..start + count)?.iter().try_fold(0i64, |n, c| {
            if c.is_ascii_digit() {
                Some(n * 10 + i64::from(*c - b'0'))
            } else {
                None
            }
        })
    };
    let year = number(0, 4)?;
    let month = number(5, 2)?;
    let day = number(8, 2)?;
    let hour = number(11, 2)?;
    let minute = number(14, 2)?;
    let second = number(17, 2)?;
    if !(1970..=2100).contains(&year)
        || !(1..=12).contains(&month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let month_days = match month {
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if day < 1 || day > month_days {
        return None;
    }
    let mut zone_at = 19;
    if b.get(zone_at) == Some(&b'.') {
        zone_at += 1;
        let first = zone_at;
        while b.get(zone_at).is_some_and(u8::is_ascii_digit) {
            zone_at += 1;
        }
        if zone_at == first {
            return None;
        }
    }
    let offset = match b.get(zone_at) {
        Some(b'Z' | b'z') if zone_at + 1 == b.len() => 0,
        Some(sign @ (b'+' | b'-'))
            if zone_at + 6 == b.len() && b.get(zone_at + 3) == Some(&b':') =>
        {
            let hours = number(zone_at + 1, 2)?;
            let minutes = number(zone_at + 4, 2)?;
            if hours > 23 || minutes > 59 {
                return None;
            }
            (hours * 3600 + minutes * 60) * if *sign == b'+' { 1 } else { -1 }
        }
        _ => return None,
    };
    let mut days = 0i64;
    for y in 1970..year {
        days += if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
            366
        } else {
            365
        };
    }
    for m in 1..month {
        days += match m {
            2 => {
                if leap {
                    29
                } else {
                    28
                }
            }
            4 | 6 | 9 | 11 => 30,
            _ => 31,
        };
    }
    let unix = (days + day - 1) * 86400 + hour * 3600 + minute * 60 + second - offset;
    u64::try_from(unix).ok()
}

pub fn gateway_uri_allowed(uri: &str) -> bool {
    if uri.len() > 1024 || uri.chars().any(char::is_control) {
        return false;
    }
    let Some(rest) = uri.strip_prefix("wss://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or("");
    if authority.contains('@') || authority.contains('?') || authority.contains('#') {
        return false;
    }
    let host = authority.strip_suffix(":443").unwrap_or(authority);
    !host.contains(':')
        && host.ends_with(".qq.com")
        && host
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'.')
}

pub struct Dedup(VecDeque<String>);
impl Dedup {
    pub fn new() -> Self {
        Self(VecDeque::with_capacity(DEDUP_SIZE))
    }
    pub fn admit(&mut self, message_id: &str) -> bool {
        if self.0.iter().any(|id| id == message_id) {
            return false;
        }
        if self.0.len() == DEDUP_SIZE {
            self.0.pop_front();
        }
        self.0.push_back(message_id.to_owned());
        true
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FrameError {
    Order,
    Limit,
    Expired,
    Allocation,
}

pub struct FrameChunk {
    pub opcode: u8,
    pub fin: bool,
    pub frame_len: usize,
    pub offset: usize,
    pub bytes: Vec<u8>,
}

/// Reassemble SDK buffer chunks AND RFC6455 continuation frames before UTF-8 parsing.
pub struct FrameAssembler {
    data: Vec<u8>,
    frame_len: usize,
    frame_offset: usize,
    opcode: u8,
    fin: bool,
    started: Option<Instant>,
}
impl FrameAssembler {
    pub fn new() -> Self {
        Self {
            data: Vec::new(),
            frame_len: 0,
            frame_offset: 0,
            opcode: 0,
            fin: false,
            started: None,
        }
    }
    pub fn is_expired(&self, now: Instant) -> bool {
        self.started
            .is_some_and(|start| now.saturating_duration_since(start) > FRAGMENT_TIMEOUT)
    }
    pub fn push(&mut self, chunk: FrameChunk, now: Instant) -> Result<Option<Vec<u8>>, FrameError> {
        if self.is_expired(now) {
            return Err(FrameError::Expired);
        }
        if chunk.frame_len > MAX_GATEWAY_MESSAGE
            || chunk.offset > chunk.frame_len
            || chunk.bytes.len() > chunk.frame_len - chunk.offset
            || self.data.len() + chunk.bytes.len() > MAX_GATEWAY_MESSAGE
        {
            return Err(FrameError::Limit);
        }
        if chunk.offset == 0 {
            if self.frame_offset != self.frame_len {
                return Err(FrameError::Order);
            }
            match chunk.opcode {
                1 if self.started.is_none() => {
                    self.started = Some(now);
                    self.data.clear();
                }
                0 if self.started.is_some() => {}
                _ => return Err(FrameError::Order),
            }
            self.frame_len = chunk.frame_len;
            self.frame_offset = 0;
            self.opcode = chunk.opcode;
            self.fin = chunk.fin;
        }
        if chunk.offset != self.frame_offset
            || chunk.frame_len != self.frame_len
            || chunk.opcode != self.opcode
            || chunk.fin != self.fin
        {
            return Err(FrameError::Order);
        }
        self.data
            .try_reserve(chunk.bytes.len())
            .map_err(|_| FrameError::Allocation)?;
        self.data.extend_from_slice(&chunk.bytes);
        self.frame_offset += chunk.bytes.len();
        if self.frame_offset == self.frame_len && self.fin {
            self.started = None;
            Ok(Some(std::mem::take(&mut self.data)))
        } else {
            Ok(None)
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: Value,
}

/// Limit passive replies to four messages, each well below QQ's content limit.
#[cfg(test)]
pub fn reply_parts(text: &str) -> Vec<String> {
    let mut parts = Vec::with_capacity(MAX_REPLY_PARTS);
    let mut remaining = text;
    while !remaining.is_empty() && parts.len() < MAX_REPLY_PARTS {
        let last = parts.len() + 1 == MAX_REPLY_PARTS;
        let suffix = "\n…（回答已截断）";
        let cap = if last && remaining.len() > REPLY_PART_BYTES {
            REPLY_PART_BYTES - suffix.len()
        } else {
            REPLY_PART_BYTES
        };
        let mut end = remaining.len().min(cap);
        while !remaining.is_char_boundary(end) {
            end -= 1;
        }
        let mut part = remaining[..end].to_owned();
        remaining = &remaining[end..];
        if last && !remaining.is_empty() {
            part.push_str(suffix);
        }
        parts.push(part);
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_depth_preflight_ignores_escaped_string_contents() {
        assert!(json_depth_allowed(br#"{"text":"[[[\"{{{","data":[1]}"#, 2));
        assert!(!json_depth_allowed(br#"{"data":[[1]]}"#, 2));
        assert!(!json_depth_allowed(br#"{"unclosed": ["#, 24));
        assert!(json_depth_allowed(br#"{"data":[[1]]}"#, 3));
    }

    #[test]
    fn timestamps_have_explicit_offsets_and_valid_calendar() {
        assert_eq!(parse_timestamp("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_timestamp("1970-01-01T08:00:01.123+08:00"), Some(1));
        assert_eq!(parse_timestamp("2024-02-29T00:00:00Z"), Some(1709164800));
        assert!(parse_timestamp("2023-02-29T00:00:00Z").is_none());
        assert!(parse_timestamp("2024-01-01T00:00:00").is_none());
        assert!(parse_timestamp("2024-0/-01T00:00:00Z").is_none());
    }

    #[test]
    fn fragment_reassembly_handles_split_utf8_and_continuation() {
        let text = "你abc".as_bytes();
        let now = Instant::now();
        let mut a = FrameAssembler::new();
        assert!(a
            .push(
                FrameChunk {
                    opcode: 1,
                    fin: false,
                    frame_len: 3,
                    offset: 0,
                    bytes: text[..1].to_vec()
                },
                now
            )
            .unwrap()
            .is_none());
        assert!(a
            .push(
                FrameChunk {
                    opcode: 1,
                    fin: false,
                    frame_len: 3,
                    offset: 1,
                    bytes: text[1..3].to_vec()
                },
                now
            )
            .unwrap()
            .is_none());
        assert_eq!(
            a.push(
                FrameChunk {
                    opcode: 0,
                    fin: true,
                    frame_len: 3,
                    offset: 0,
                    bytes: text[3..].to_vec()
                },
                now
            )
            .unwrap(),
            Some(text.to_vec())
        );
    }

    #[test]
    fn fragment_loss_and_oversize_are_errors() {
        let now = Instant::now();
        let mut a = FrameAssembler::new();
        a.push(
            FrameChunk {
                opcode: 1,
                fin: true,
                frame_len: 3,
                offset: 0,
                bytes: vec![1],
            },
            now,
        )
        .unwrap();
        assert_eq!(
            a.push(
                FrameChunk {
                    opcode: 1,
                    fin: true,
                    frame_len: 3,
                    offset: 2,
                    bytes: vec![1]
                },
                now
            ),
            Err(FrameError::Order)
        );
        assert!(a.is_expired(now + Duration::from_secs(11)));
        let mut a = FrameAssembler::new();
        assert_eq!(
            a.push(
                FrameChunk {
                    opcode: 1,
                    fin: true,
                    frame_len: MAX_GATEWAY_MESSAGE + 1,
                    offset: 0,
                    bytes: vec![]
                },
                now
            ),
            Err(FrameError::Limit)
        );
    }

    #[test]
    fn permissions_default_closed_and_groups_require_both_lists() {
        let d = json!({"id":"msg1", "author":{"member_openid":"alice"}, "group_openid":"group1", "content":"<@!bot> 你好", "timestamp":"2026-01-01T00:00:00Z"});
        let m = parse_message("GROUP_AT_MESSAGE_CREATE", &d).unwrap();
        assert_eq!(m.content, "你好");
        assert!(!m.is_authorized(&[], &[]));
        assert!(!m.is_authorized(&[], &["group1".into()]));
        assert!(m.is_authorized(&["alice".into()], &["group1".into()]));
        assert_eq!(m.session_key(), "g:group1:alice");
        assert!(!m.is_fresh(m.timestamp + MESSAGE_TTL_SECS));
    }

    fn group_event() -> Value {
        json!({
            "id":"group-msg-1", "author":{"member_openid":"alice"},
            "group_openid":"group1", "content":" hello ",
            "timestamp":"2026-01-01T08:00:00+08:00"
        })
    }

    #[test]
    fn both_group_event_kinds_share_the_same_schema_and_authorization() {
        for kind in ["GROUP_AT_MESSAGE_CREATE", "GROUP_MESSAGE_CREATE"] {
            let event = group_event();
            let message = parse_message(kind, &event).unwrap();
            assert_eq!(message.target, Target::Group("group1".into()));
            assert_eq!(message.author, "alice");
            assert_eq!(message.content, "hello");
            assert_eq!(message.session_key(), "g:group1:alice");
            assert!(!message.is_authorized(&["alice".into()], &[]));
            assert!(!message.is_authorized(&[], &["group1".into()]));
            assert!(message.is_authorized(&["alice".into()], &["group1".into()]));

            let mut prefixed = event;
            prefixed["content"] = json!(" <@!bot>  hello ");
            assert_eq!(parse_message(kind, &prefixed).unwrap().content, "hello");
        }
    }

    #[test]
    fn both_group_event_kinds_reject_missing_or_wrong_schema_fields() {
        for kind in ["GROUP_AT_MESSAGE_CREATE", "GROUP_MESSAGE_CREATE"] {
            for field in ["id", "author", "group_openid", "timestamp"] {
                let mut event = group_event();
                event.as_object_mut().unwrap().remove(field);
                assert!(parse_message(kind, &event).is_none(), "{kind}: {field}");
            }
            for (field, value) in [
                ("author", json!({"user_openid":"alice", "id":"alice"})),
                ("group_openid", json!(1234)),
                ("content", json!([])),
                ("timestamp", json!(1767225600)),
                ("timestamp", json!("2026-01-01T00:00:00")),
            ] {
                let mut event = group_event();
                event[field] = value;
                assert!(parse_message(kind, &event).is_none(), "{kind}: {field}");
            }
        }
    }

    #[test]
    fn full_group_events_accept_only_complete_app_id_markers_anywhere() {
        for text in [
            "<@bot> hello",
            "<@!bot>hello",
            "  <@!bot> hello",
            "hello <@bot>",
            "hello <@!bot> world",
            "<@other> hello <@bot>",
        ] {
            assert!(group_message_addressed_to(&json!({"content":text}), "bot"));
        }
        for text in [
            "hello",
            "<@other> hello",
            "<@!other> hello",
            "<@bot-extra> hello",
            "<@!bot_extra> hello",
            "<@bot > hello",
            "<@ bot> hello",
            "<@!!bot> hello",
            "<@bot",
            "<@bot/>",
            "hello <@!bot-extra> world",
        ] {
            assert!(
                !group_message_addressed_to(&json!({"content":text}), "bot"),
                "{text}"
            );
        }
    }

    #[test]
    fn full_group_events_do_not_infer_self_from_mention_ids_or_names() {
        for mentions in [
            json!(null),
            json!({"id":"bot"}),
            json!(["bot"]),
            json!([{"id":"other"}]),
            json!([{"id":"bot-extra"}]),
            json!([{"id":"bot"}]),
            json!([{"id":"bot", "bot":true}]),
            json!([{"id":"other", "bot":true, "nickname":"bot", "username":"bot"}]),
            json!([{"id":123}]),
            json!([{"member_openid":"bot"}]),
            json!([{"user_openid":"bot"}]),
        ] {
            assert!(!group_message_addressed_to(
                &json!({"mentions":mentions}),
                "bot"
            ));
        }
        assert!(!group_message_addressed_to(&json!({}), "bot"));
    }

    #[test]
    fn full_group_events_trust_only_boolean_server_self_flags() {
        assert!(group_message_addressed_to(
            &json!({"content":"hello", "mentions":[{"id":"other", "is_you":false}, {"id":"group-bot-openid", "is_you":true}]}),
            "app-id"
        ));
        for flag in [
            json!(false),
            json!("true"),
            json!(1),
            json!(null),
            json!([]),
        ] {
            assert!(!group_message_addressed_to(
                &json!({"content":"<@other> hello", "mentions":[{"id":"app-id", "is_you":flag}]}),
                "app-id"
            ));
        }
        assert!(!group_message_addressed_to(
            &json!({"content":"hello", "mentions":{"is_you":true}}),
            "app-id"
        ));
    }

    #[test]
    fn full_group_events_require_a_valid_configured_app_id() {
        for app_id in ["", "app id", "app>hello", "!app", "机器人", "app\n"] {
            let event =
                json!({"content":format!("<@{app_id}> hello"), "mentions":[{"is_you":true}]});
            assert!(!group_message_addressed_to(&event, app_id));
        }
        let app_id = "a".repeat(MAX_ID + 1);
        let event = json!({"content":format!("<@{app_id}> hello")});
        assert!(!group_message_addressed_to(&event, &app_id));
    }

    #[test]
    fn duplicate_message_ids_are_shared_across_group_event_kinds() {
        let event = group_event();
        for kinds in [
            ["GROUP_AT_MESSAGE_CREATE", "GROUP_MESSAGE_CREATE"],
            ["GROUP_MESSAGE_CREATE", "GROUP_AT_MESSAGE_CREATE"],
        ] {
            let mut dedup = Dedup::new();
            assert!(dedup.admit(&parse_message(kinds[0], &event).unwrap().message_id));
            assert!(!dedup.admit(&parse_message(kinds[1], &event).unwrap().message_id));
        }
    }

    #[test]
    fn gateway_authority_and_reply_splits_are_bounded() {
        assert!(gateway_uri_allowed("wss://api.bot.qq.com/websocket/"));
        assert!(!gateway_uri_allowed("wss://api.bot.qq.com.evil.org/"));
        assert!(!gateway_uri_allowed("wss://evil@api.bot.qq.com/"));
        assert!(!gateway_uri_allowed("ws://api.bot.qq.com/"));
        let text = "你好".repeat(2000);
        let parts = reply_parts(&text);
        assert_eq!(parts.len(), MAX_REPLY_PARTS);
        assert!(parts.iter().all(|p| p.len() <= REPLY_PART_BYTES));
        assert!(parts.last().unwrap().ends_with("（回答已截断）"));
    }

    #[test]
    fn dedup_rejects_only_recent_ids() {
        let mut d = Dedup::new();
        assert!(d.admit("one"));
        assert!(!d.admit("one"));
        for i in 0..DEDUP_SIZE {
            assert!(d.admit(&format!("id{i}")));
        }
        assert!(d.admit("one"));
    }

    #[test]
    fn proactive_payload_has_no_passive_id_and_caps_actual_json_bytes() {
        let body = notification_payload("设备验收完成 RUST_ESP_OK").unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 2);
        assert_eq!(value["msg_type"], 0);
        assert!(value.get("msg_id").is_none());
        assert!(value.get("msg_seq").is_none());
        assert!(notification_payload(" ").is_err());
        assert!(notification_payload(&"a".repeat(MAX_NOTIFICATION_BYTES)).is_err());
        assert!(notification_payload(&"\n".repeat(1000)).is_err());
        assert!(notification_payload(&"a".repeat(1700)).unwrap().len() <= MAX_NOTIFICATION_BYTES);
    }

    fn image_event() -> Value {
        json!({
            "id":"image-msg-1", "author":{"user_openid":"alice"},
            "timestamp":"2026-01-01T00:00:00Z", "content":"",
            "attachments":[{
                "url":"https://gchat.qpic.cn/fixture.png?rkey=fixture-signature",
                "content_type":"image/png","size":1234,"width":800,"height":600
            }]
        })
    }

    #[test]
    fn pure_qq_image_retains_original_context_and_transient_signed_url() {
        let event = image_event();
        let message = parse_message("C2C_MESSAGE_CREATE", &event).unwrap();
        assert_eq!(message.message_id, "image-msg-1");
        assert_eq!(message.content, "");
        assert!(message.image_error.is_none());
        let image = message.images.first().unwrap();
        assert_eq!(image.size, Some(1234));
        assert!(image.url.ends_with("rkey=fixture-signature"));
        assert!(!message.is_authorized(&[], &[]));
        assert!(message.is_authorized(&["alice".into()], &[]));

        let mut no_content = event;
        no_content.as_object_mut().unwrap().remove("content");
        assert!(!parse_message("C2C_MESSAGE_CREATE", &no_content)
            .unwrap()
            .images
            .is_empty());
    }

    #[test]
    fn image_errors_remain_replyable_instead_of_dropping_context_or_faking_vision() {
        let mut event = image_event();
        let second = event["attachments"][0].clone();
        event["attachments"]
            .as_array_mut()
            .unwrap()
            .push(second.clone());
        event["attachments"].as_array_mut().unwrap().push(second);
        let message = parse_message("C2C_MESSAGE_CREATE", &event).unwrap();
        assert_eq!(message.image_error, Some(ImageError::TooMany));
        assert!(message.images.is_empty());
        assert_eq!(message.message_id, "image-msg-1");

        for (field, value, expected) in [
            (
                "content_type",
                json!("image/svg+xml"),
                ImageError::UnsupportedFormat,
            ),
            ("size", json!(MAX_IMAGE_BYTES + 1), ImageError::TooLarge),
            (
                "width",
                json!(MAX_IMAGE_DIMENSION + 1),
                ImageError::Dimensions,
            ),
            ("height", json!(-1), ImageError::Metadata),
        ] {
            let mut event = image_event();
            event["attachments"][0][field] = value;
            let message = parse_message("C2C_MESSAGE_CREATE", &event).unwrap();
            assert_eq!(message.image_error, Some(expected));
            assert!(message.images.is_empty());
        }
    }

    #[test]
    fn image_url_normalizes_only_https_and_rejects_local_or_credential_targets() {
        for bad in [
            "http://gchat.qpic.cn/image.png",
            "https://user:password@gchat.qpic.cn/image.png",
            "https://localhost/a.png",
            "https://192.168.1.1/a.png",
            "https://127.0.0.1/a.png",
            "https://[::1]/a.png",
            "https://[::ffff:127.0.0.1]/a.png",
            "https://thing.local/a.png",
            "https://gchat.qpic.cn/a\nb.png",
            "https://gchat.qpic.cn\\@evil.example/a.png",
        ] {
            let mut event = image_event();
            event["attachments"][0]["url"] = json!(bad);
            assert_eq!(
                parse_message("C2C_MESSAGE_CREATE", &event)
                    .unwrap()
                    .image_error,
                Some(ImageError::Url)
            );
        }
        let mut event = image_event();
        event["attachments"][0]["url"] = json!("//gchat.qpic.cn/a.png?rkey=fixture");
        event["attachments"][0]["content_type"] = json!("IMAGE/JPEG");
        let image = parse_message("C2C_MESSAGE_CREATE", &event)
            .unwrap()
            .images
            .remove(0);
        assert_eq!(image.url, "https://gchat.qpic.cn/a.png?rkey=fixture");
        assert_eq!(image.content_type, "image/jpeg");

        event["attachments"][0]["url"] = json!(format!(
            "https://gchat.qpic.cn/a.png?rkey={}",
            "a".repeat(3000)
        ));
        assert!(!parse_message("C2C_MESSAGE_CREATE", &event)
            .unwrap()
            .images
            .is_empty());
        event["attachments"][0]["url"] = json!(format!(
            "https://gchat.qpic.cn/a.png?rkey={}",
            "a".repeat(MAX_IMAGE_URL_BYTES)
        ));
        assert_eq!(
            parse_message("C2C_MESSAGE_CREATE", &event)
                .unwrap()
                .image_error,
            Some(ImageError::Url)
        );
    }

    #[test]
    fn ordinary_text_serialization_and_no_attachment_cases_stay_compatible() {
        let message = ChatMessage {
            role: "assistant".into(),
            content: json!("text response"),
        };
        assert_eq!(
            serde_json::to_value(message).unwrap(),
            json!({"role":"assistant","content":"text response"})
        );
        let mut event = image_event();
        event["attachments"] = json!([]);
        assert!(parse_message("C2C_MESSAGE_CREATE", &event).is_none());
        event["content"] = json!("ordinary text");
        let message = parse_message("C2C_MESSAGE_CREATE", &event).unwrap();
        assert!(message.images.is_empty() && message.image_error.is_none());
        assert_eq!(message.content, "ordinary text");
    }
}
