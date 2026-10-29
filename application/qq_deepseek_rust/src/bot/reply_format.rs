//! Bounded, incremental reply formatting shared by the sender and host tests.
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU32, Ordering};

pub const REPLY_BYTES: usize = 1800;
pub const REPLY_LIMIT: u32 = 4;
const TRUNCATED: &str = "\n…（回答已截断）";

#[derive(Default)]
pub struct Budget(AtomicU32);
impl Budget {
    /// Reserve before POST: an unknown delivery must never reuse a sequence.
    pub fn reserve(&self) -> Option<u32> {
        self.0
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n < REPLY_LIMIT).then_some(n + 1)
            })
            .ok()
            .map(|n| n + 1)
    }
    pub fn remaining(&self) -> u32 {
        REPLY_LIMIT.saturating_sub(self.0.load(Ordering::Relaxed))
    }
}

pub fn payload(content: &str, msg_id: &str, seq: u32, markdown: bool) -> Value {
    if markdown {
        json!({"msg_type":2,"markdown":{"content":content},"msg_id":msg_id,"msg_seq":seq})
    } else {
        json!({"msg_type":0,"content":content,"msg_id":msg_id,"msg_seq":seq})
    }
}

/// Retain paragraph/line boundaries and close/reopen fenced code across messages.
/// Oversized individual inline constructs may still be cut at a UTF-8 boundary.
pub struct Parts<'a> {
    remaining: &'a str,
    fence: Option<String>,
}
impl<'a> Parts<'a> {
    pub fn new(text: &'a str) -> Self {
        Self {
            remaining: text,
            fence: None,
        }
    }
    /// A fallback can consume the final slot earlier than the original split plan.
    pub fn finish_here(&mut self, mut part: String) -> String {
        if self.remaining.is_empty() {
            return part;
        }
        let mut end = part.len().min(REPLY_BYTES - TRUNCATED.len());
        while !part.is_char_boundary(end) {
            end -= 1;
        }
        part.truncate(end);
        part.push_str(TRUNCATED);
        self.remaining = "";
        part
    }
    pub fn next(&mut self, slots: u32) -> Option<String> {
        if self.remaining.is_empty() || slots == 0 {
            return None;
        }
        let prefix = self
            .fence
            .as_ref()
            .map(|f| format!("{f}\n"))
            .unwrap_or_default();
        // Room for a closing fence, truncation marker and reopened language label.
        let cap = REPLY_BYTES - prefix.len() - 5 - if slots == 1 { TRUNCATED.len() } else { 0 };
        let mut end = self.remaining.len().min(cap);
        while !self.remaining.is_char_boundary(end) {
            end -= 1;
        }
        if end < self.remaining.len() {
            if let Some(pos) = self.remaining[..end].rfind('\n').filter(|p| *p > cap / 2) {
                end = pos + 1;
            }
        }
        let raw = &self.remaining[..end];
        self.remaining = &self.remaining[end..];
        for line in raw.lines() {
            let line = line.trim_start();
            if line.starts_with("```") || line.starts_with("~~~") {
                if let Some(open) = self.fence.as_ref() {
                    if line.starts_with(&open[..3]) {
                        self.fence = None;
                    }
                } else if line.len() <= 48 {
                    self.fence = Some(line.to_owned());
                }
            }
        }
        let mut part = prefix;
        part.push_str(raw);
        if let Some(fence) = &self.fence {
            part.push('\n');
            part.push_str(&fence[..3]);
        }
        if slots == 1 && !self.remaining.is_empty() {
            part.push_str(TRUNCATED);
            self.remaining = "";
        }
        Some(part)
    }
}

/// Only a completely received explicit HTTP rejection can trigger a text fallback.
/// Rate limits, credential failures, server errors and transport ambiguity must not replay.
pub fn may_fallback(status: u16) -> bool {
    matches!(status, 400 | 403 | 415 | 422)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn body_excludes_incompatible_fields() {
        let md = payload("# Hi", "trigger", 1, true);
        assert_eq!(md["msg_type"], 2);
        assert!(md.get("content").is_none());
        assert_eq!(md["markdown"]["content"], "# Hi");
        let text = payload("Hi", "trigger", 2, false);
        assert!(text.get("markdown").is_none());
    }
    #[test]
    fn shared_budget_never_reuses_unknown_or_rejected_attempts() {
        let b = std::sync::Arc::new(Budget::default());
        assert_eq!(b.reserve(), Some(1));
        let c = b.clone();
        assert_eq!(c.reserve(), Some(2));
        assert_eq!(b.reserve(), Some(3));
        assert_eq!(b.reserve(), Some(4));
        assert_eq!(c.reserve(), None);
        assert_eq!(b.remaining(), 0);
        assert!(may_fallback(400));
        assert!(!may_fallback(429));
        assert!(!may_fallback(500));
        assert!(!may_fallback(200));
        assert!(!may_fallback(401));
    }
    #[test]
    fn unicode_fences_and_dynamic_quota_remain_bounded() {
        let text = format!(
            "# Header\n\n```rust\n{}\n```\nTail",
            "let x = 1; // 你好\n".repeat(450)
        );
        let mut parts = Parts::new(&text);
        let mut got = Vec::new();
        for slots in (1..=3).rev() {
            got.push(parts.next(slots).unwrap());
        }
        assert!(got.iter().all(|p| p.len() <= REPLY_BYTES));
        assert!(got[0].ends_with("\n```"));
        assert!(got[1].starts_with("```rust\n"));
        assert!(got[2].ends_with("（回答已截断）"));
        assert!(parts.next(1).is_none());
    }
    #[test]
    fn short_answer_is_unchanged_and_paragraph_split_is_preserved() {
        assert_eq!(Parts::new("**Hello** 🙂").next(4).unwrap(), "**Hello** 🙂");
        let text = format!("{}\n\n{}", "a".repeat(1200), "b".repeat(1200));
        let mut parts = Parts::new(&text);
        assert!(parts.next(4).unwrap().ends_with("\n\n"));
        assert_eq!(parts.next(3).unwrap(), "b".repeat(1200));
    }
}
