use crate::tool_calls::{ToolCall, MAX_ARGUMENT_BYTES, MAX_TOOL_CALLS};
use core::fmt;
use serde::{de::IgnoredAny, Deserialize};

pub const MAX_SSE_EVENT_BYTES: usize = 16 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024;
pub const MAX_REASONING_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy)]
pub struct SseLimits {
    pub max_event_bytes: usize,
    pub max_content_bytes: usize,
    pub max_reasoning_bytes: usize,
}

impl Default for SseLimits {
    fn default() -> Self {
        Self {
            max_event_bytes: MAX_SSE_EVENT_BYTES,
            max_content_bytes: MAX_RESPONSE_BYTES,
            max_reasoning_bytes: MAX_REASONING_BYTES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SseError {
    InvalidLimits,
    EventTooLarge,
    ContentTooLarge,
    ReasoningTooLarge,
    InvalidUtf8,
    InvalidJson,
    UpstreamError,
    MultipleChoices,
    AfterCompletion,
    MissingStop,
    TruncatedStream,
    LengthLimit,
    ContentFiltered,
    UnsupportedToolCall,
    InvalidToolCall,
    ToolCallTooLarge,
    UnsupportedFinishReason,
    AllocationFailed,
}

impl fmt::Display for SseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidLimits => "invalid_sse_limits",
            Self::EventTooLarge => "sse_event_too_large",
            Self::ContentTooLarge => "model_content_too_large",
            Self::ReasoningTooLarge => "model_reasoning_too_large",
            Self::InvalidUtf8 => "invalid_sse_utf8",
            Self::InvalidJson => "invalid_sse_json",
            Self::UpstreamError => "model_upstream_error",
            Self::MultipleChoices => "unsupported_multiple_choices",
            Self::AfterCompletion => "model_data_after_completion",
            Self::MissingStop => "model_missing_stop_reason",
            Self::TruncatedStream => "truncated_sse_stream",
            Self::LengthLimit => "model_length_limit",
            Self::ContentFiltered => "model_content_filtered",
            Self::UnsupportedToolCall => "unsupported_model_tool_call",
            Self::InvalidToolCall => "invalid_model_tool_call",
            Self::ToolCallTooLarge => "model_tool_call_limit",
            Self::UnsupportedFinishReason => "unsupported_model_finish_reason",
            Self::AllocationFailed => "protocol_allocation_failed",
        })
    }
}

impl std::error::Error for SseError {}

/// Reasoning is counted and discarded, never returned as an ordinary answer.
pub struct Completion {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
}

/// Processes complete SSE lines/events while accepting arbitrary TCP/UTF-8
/// fragmentation. Buffers have explicit byte limits; an error is sticky.
pub struct SseDecoder {
    line: Vec<u8>,
    data: Vec<u8>,
    content: String,
    limits: SseLimits,
    reasoning_bytes: usize,
    allow_tools: bool,
    tool_calls: Vec<ToolCall>,
    tool_finish: bool,
    stop_seen: bool,
    done_seen: bool,
    failed: Option<SseError>,
}

#[derive(Deserialize)]
struct Chunk {
    choices: Option<Vec<Choice>>,
    error: Option<IgnoredAny>,
}

#[derive(Deserialize)]
struct Choice {
    index: Option<u32>,
    #[serde(default)]
    delta: Delta,
    finish_reason: Option<String>,
}

#[derive(Default, Deserialize)]
struct Delta {
    content: Option<String>,
    reasoning_content: Option<String>,
    tool_calls: Option<Vec<ToolDelta>>,
}

#[derive(Deserialize)]
struct ToolDelta {
    index: usize,
    id: Option<String>,
    r#type: Option<String>,
    function: Option<FunctionDelta>,
}
#[derive(Deserialize)]
struct FunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}
fn append_tool(target: &mut String, part: Option<String>, max: usize) -> Result<(), SseError> {
    if let Some(part) = part {
        if target.len().saturating_add(part.len()) > max {
            return Err(SseError::ToolCallTooLarge);
        }
        target
            .try_reserve(part.len())
            .map_err(|_| SseError::AllocationFailed)?;
        target.push_str(&part);
    }
    Ok(())
}
impl SseDecoder {
    pub fn new(limits: SseLimits) -> Self {
        let invalid = limits.max_event_bytes == 0 || limits.max_content_bytes == 0;
        Self {
            line: Vec::new(),
            data: Vec::new(),
            content: String::new(),
            limits,
            reasoning_bytes: 0,
            allow_tools: false,
            tool_calls: Vec::new(),
            tool_finish: false,
            stop_seen: false,
            done_seen: false,
            failed: invalid.then_some(SseError::InvalidLimits),
        }
    }

    pub fn with_tools(limits: SseLimits) -> Self {
        let mut decoder = Self::new(limits);
        decoder.allow_tools = true;
        decoder
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    pub fn has_tool_calls(&self) -> bool {
        !self.tool_calls.is_empty()
    }

    pub fn content_so_far(&self) -> &str {
        &self.content
    }

    pub fn is_done(&self) -> bool {
        self.done_seen && self.failed.is_none()
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<(), SseError> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.push_inner(bytes);
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }

    fn push_inner(&mut self, bytes: &[u8]) -> Result<(), SseError> {
        for &byte in bytes {
            if byte == b'\n' {
                self.process_line()?;
                self.line.clear();
            } else {
                // This also bounds ignored SSE metadata and comment lines.
                if self.line.len() >= self.limits.max_event_bytes {
                    return Err(SseError::EventTooLarge);
                }
                self.line
                    .try_reserve(1)
                    .map_err(|_| SseError::AllocationFailed)?;
                self.line.push(byte);
            }
        }
        Ok(())
    }

    fn process_line(&mut self) -> Result<(), SseError> {
        if self.line.last() == Some(&b'\r') {
            self.line.pop();
        }
        // Checking each whole line keeps split multi-byte characters valid.
        core::str::from_utf8(&self.line).map_err(|_| SseError::InvalidUtf8)?;
        if self.line.is_empty() {
            return self.process_event();
        }
        if self.line[0] == b':' {
            return Ok(());
        }
        let Some(payload) = self.line.strip_prefix(b"data:") else {
            return Ok(());
        };
        let payload = payload.strip_prefix(b" ").unwrap_or(payload);
        let separator = usize::from(!self.data.is_empty());
        let next_len = self
            .data
            .len()
            .checked_add(payload.len())
            .and_then(|len| len.checked_add(separator))
            .ok_or(SseError::EventTooLarge)?;
        if next_len > self.limits.max_event_bytes {
            return Err(SseError::EventTooLarge);
        }
        self.data
            .try_reserve(payload.len() + separator)
            .map_err(|_| SseError::AllocationFailed)?;
        if separator != 0 {
            self.data.push(b'\n');
        }
        self.data.extend_from_slice(payload);
        Ok(())
    }

    fn process_event(&mut self) -> Result<(), SseError> {
        if self.data.is_empty() {
            return Ok(());
        }
        if self.done_seen {
            return Err(SseError::AfterCompletion);
        }
        let payload = core::str::from_utf8(&self.data).map_err(|_| SseError::InvalidUtf8)?;
        if payload.trim() == "[DONE]" {
            if !self.stop_seen {
                return Err(SseError::MissingStop);
            }
            self.done_seen = true;
            self.data.clear();
            return Ok(());
        }
        let chunk: Chunk = serde_json::from_str(payload).map_err(|_| SseError::InvalidJson)?;
        if chunk.error.is_some() {
            return Err(SseError::UpstreamError);
        }
        let mut choices = chunk.choices.ok_or(SseError::InvalidJson)?;
        if choices.len() > 1 {
            return Err(SseError::MultipleChoices);
        }
        if let Some(choice) = choices.pop() {
            if choice.index.is_some_and(|index| index != 0) {
                return Err(SseError::MultipleChoices);
            }
            if let Some(deltas) = choice.delta.tool_calls {
                if !self.allow_tools {
                    return Err(SseError::UnsupportedToolCall);
                }
                if self.stop_seen {
                    return Err(SseError::AfterCompletion);
                }
                if deltas.len() > MAX_TOOL_CALLS {
                    return Err(SseError::ToolCallTooLarge);
                }
                for delta in deltas {
                    if delta.index >= MAX_TOOL_CALLS {
                        return Err(SseError::ToolCallTooLarge);
                    }
                    if delta.index > self.tool_calls.len() {
                        return Err(SseError::InvalidToolCall);
                    }
                    if delta.index == self.tool_calls.len() {
                        self.tool_calls.push(ToolCall::default());
                    }
                    let call = &mut self.tool_calls[delta.index];
                    if delta.r#type.is_some_and(|t| t != "function") {
                        return Err(SseError::InvalidToolCall);
                    }
                    append_tool(&mut call.id, delta.id, 128)?;
                    if let Some(function) = delta.function {
                        append_tool(&mut call.function.name, function.name, 64)?;
                        append_tool(
                            &mut call.function.arguments,
                            function.arguments,
                            MAX_ARGUMENT_BYTES,
                        )?;
                    }
                }
            }
            if let Some(reasoning) = choice.delta.reasoning_content {
                self.reasoning_bytes = self
                    .reasoning_bytes
                    .checked_add(reasoning.len())
                    .ok_or(SseError::ReasoningTooLarge)?;
                if self.reasoning_bytes > self.limits.max_reasoning_bytes {
                    return Err(SseError::ReasoningTooLarge);
                }
            }
            if let Some(text) = choice.delta.content {
                if self.stop_seen && !text.is_empty() {
                    return Err(SseError::AfterCompletion);
                }
                let next_len = self
                    .content
                    .len()
                    .checked_add(text.len())
                    .ok_or(SseError::ContentTooLarge)?;
                if next_len > self.limits.max_content_bytes {
                    return Err(SseError::ContentTooLarge);
                }
                self.content
                    .try_reserve(text.len())
                    .map_err(|_| SseError::AllocationFailed)?;
                self.content.push_str(&text);
            }
            if let Some(reason) = choice.finish_reason {
                match reason.as_str() {
                    "stop" if self.tool_calls.is_empty() => self.stop_seen = true,
                    "stop" => return Err(SseError::InvalidToolCall),
                    "length" => return Err(SseError::LengthLimit),
                    "content_filter" => return Err(SseError::ContentFiltered),
                    "tool_calls" if self.allow_tools && !self.tool_calls.is_empty() => {
                        self.tool_finish = true;
                        self.stop_seen = true;
                    }
                    "tool_calls" => return Err(SseError::UnsupportedToolCall),
                    _ => return Err(SseError::UnsupportedFinishReason),
                }
            }
        }
        self.data.clear();
        Ok(())
    }

    /// EOF is never a substitute for the provider's explicit terminal event.
    pub fn finish(self) -> Result<Completion, SseError> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if !self.line.is_empty() || !self.data.is_empty() || !self.done_seen {
            return Err(SseError::TruncatedStream);
        }
        if !self.stop_seen {
            return Err(SseError::MissingStop);
        }
        if !self.tool_calls.is_empty() && !self.tool_finish {
            return Err(SseError::InvalidToolCall);
        }
        for (i, call) in self.tool_calls.iter().enumerate() {
            if call.id.is_empty()
                || !call
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
                || call.function.name.is_empty()
                || call.function.arguments.is_empty()
                || self.tool_calls[..i].iter().any(|other| other.id == call.id)
            {
                return Err(SseError::InvalidToolCall);
            }
        }
        Ok(Completion {
            content: self.content,
            tool_calls: self.tool_calls,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(text: &str, reason: &str) -> Vec<u8> {
        let content = serde_json::json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]});
        let terminal =
            serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":reason}]});
        format!(
            ": keep-alive\r\ndata: {content}\r\n\r\ndata: {terminal}\r\n\r\ndata: [DONE]\r\n\r\n"
        )
        .into_bytes()
    }

    #[test]
    fn accepts_every_fragment_size_including_split_utf8_and_crlf() {
        let wire = stream("你好，ESP 🌱", "stop");
        for size in 1..=wire.len() {
            let mut decoder = SseDecoder::new(SseLimits::default());
            for part in wire.chunks(size) {
                decoder.push(part).unwrap();
            }
            assert!(decoder.is_done());
            assert_eq!(decoder.finish().unwrap().content, "你好，ESP 🌱");
        }
    }

    #[test]
    fn rejects_eof_after_stop_without_done() {
        let wire = b"data: {\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":\"stop\"}]}\n\n";
        let mut decoder = SseDecoder::new(SseLimits::default());
        decoder.push(wire).unwrap();
        assert!(matches!(decoder.finish(), Err(SseError::TruncatedStream)));
    }

    #[test]
    fn done_requires_stop_and_failure_cannot_be_overwritten() {
        let mut decoder = SseDecoder::new(SseLimits::default());
        assert_eq!(
            decoder.push(b"data: [DONE]\n\n"),
            Err(SseError::MissingStop)
        );
        assert_eq!(
            decoder.push(&stream("late", "stop")),
            Err(SseError::MissingStop)
        );
    }

    #[test]
    fn finish_reasons_remain_distinct() {
        for (reason, error) in [
            ("length", SseError::LengthLimit),
            ("content_filter", SseError::ContentFiltered),
            ("tool_calls", SseError::UnsupportedToolCall),
            ("unexpected", SseError::UnsupportedFinishReason),
        ] {
            let mut decoder = SseDecoder::new(SseLimits::default());
            assert_eq!(decoder.push(&stream("partial", reason)), Err(error));
        }
    }

    #[test]
    fn bounds_a_line_an_event_and_the_total_answer_separately() {
        let limits = SseLimits {
            max_event_bytes: 10,
            ..SseLimits::default()
        };
        let mut decoder = SseDecoder::new(limits);
        assert_eq!(decoder.push(b"data: 12345"), Err(SseError::EventTooLarge));
        let mut decoder = SseDecoder::new(SseLimits {
            max_event_bytes: 20,
            ..SseLimits::default()
        });
        assert_eq!(
            decoder.push(b"data: 123456789012\ndata: 123456789012\n\n"),
            Err(SseError::EventTooLarge)
        );
        let limits = SseLimits {
            max_content_bytes: 3,
            ..SseLimits::default()
        };
        let mut decoder = SseDecoder::new(limits);
        assert_eq!(
            decoder.push(&stream("abcd", "stop")),
            Err(SseError::ContentTooLarge)
        );
    }

    #[test]
    fn discards_reasoning_and_checks_its_separate_budget() {
        let reasoning = b"data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"hidden\"},\"finish_reason\":null}]}\n\n";
        let mut decoder = SseDecoder::new(SseLimits::default());
        decoder.push(reasoning).unwrap();
        decoder.push(&stream("visible", "stop")).unwrap();
        assert_eq!(decoder.finish().unwrap().content, "visible");
        let limits = SseLimits {
            max_reasoning_bytes: 5,
            ..SseLimits::default()
        };
        let mut decoder = SseDecoder::new(limits);
        assert_eq!(decoder.push(reasoning), Err(SseError::ReasoningTooLarge));
    }

    #[test]
    fn rejects_invalid_encoding_json_and_upstream_errors() {
        for (wire, error) in [
            (&b"data: \xff\n\n"[..], SseError::InvalidUtf8),
            (&b"data: {not json}\n\n"[..], SseError::InvalidJson),
            (
                &b"data: {\"error\":{\"message\":\"private error\"}}\n\n"[..],
                SseError::UpstreamError,
            ),
        ] {
            let mut decoder = SseDecoder::new(SseLimits::default());
            assert_eq!(decoder.push(wire), Err(error));
        }
    }

    #[test]
    fn permits_multiline_json_and_rejects_data_after_done() {
        let mut decoder = SseDecoder::new(SseLimits::default());
        decoder.push(b"data: {\"choices\":\ndata: [{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\n").unwrap();
        decoder.push(b"data: [DONE]\n\n").unwrap();
        assert_eq!(decoder.content(), "ok");
        assert_eq!(
            decoder.push(&stream("late", "stop")),
            Err(SseError::AfterCompletion)
        );
    }
    #[test]
    fn tools_survive_every_fragment_size_and_require_opt_in() {
        let one = serde_json::json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"recall_history","arguments":"{\"query\":"}}]},"finish_reason":null}]});
        let two = serde_json::json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"彗星\"}"}}]},"finish_reason":"tool_calls"}]});
        let wire = format!("data: {one}\n\ndata: {two}\n\ndata: [DONE]\n\n");
        for size in 1..=wire.len() {
            let mut decoder = SseDecoder::with_tools(SseLimits::default());
            for part in wire.as_bytes().chunks(size) {
                decoder.push(part).unwrap();
            }
            let c = decoder.finish().unwrap();
            assert_eq!(c.tool_calls.len(), 1);
            assert!(matches!(
                crate::tool_calls::parse(&c.tool_calls[0]),
                Ok(crate::tool_calls::Task::Recall(_))
            ));
        }
        let mut decoder = SseDecoder::new(SseLimits::default());
        assert_eq!(
            decoder.push(wire.as_bytes()),
            Err(SseError::UnsupportedToolCall)
        );
    }
    #[test]
    fn tool_argument_and_index_limits_are_enforced_before_execution() {
        for (index, args, error) in [
            (
                0,
                "x".repeat(MAX_ARGUMENT_BYTES + 1),
                SseError::ToolCallTooLarge,
            ),
            (2, "{}".into(), SseError::ToolCallTooLarge),
            (1, "{}".into(), SseError::InvalidToolCall),
        ] {
            let payload = serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":index,"id":"a","function":{"name":"search_web","arguments":args}}]},"finish_reason":"tool_calls"}]});
            let mut decoder = SseDecoder::with_tools(SseLimits::default());
            assert_eq!(
                decoder.push(format!("data: {payload}\n\n").as_bytes()),
                Err(error)
            );
        }
    }
}
