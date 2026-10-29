//! Explicit, bounded DeepSeek Search/Fetch requests and untrusted result handling.
use serde::Serialize;
use serde_json::Value;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr};

pub const SEARCH_RESPONSE_BYTES: usize = 64 * 1024;
pub const FETCH_RESPONSE_BYTES: usize = 128 * 1024;
pub const MAX_WEB_CONTEXT_BYTES: usize = 16 * 1024;
pub const MAX_FETCH_URL_BYTES: usize = 2048;
const SEARCH_COUNT: usize = 3;
const MAX_QUERY_CHARS: usize = 128;
const MAX_TITLE_BYTES: usize = 256;
const MAX_SEARCH_URL_BYTES: usize = 1024;
const MAX_SUMMARY_BYTES: usize = 2048;
const MAX_FETCH_TEXT_BYTES: usize = 12 * 1024;
const MAX_REPLY_BYTES: usize = 7000;
const TRUNCATED_NOTICE: &str = "\n（内容已按设备长度限制截断）";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WebTask {
    Search(String),
    Fetch(String),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WebToolError {
    Query,
    Url,
    BodyLimit,
    Schema,
    ContextLimit,
}
impl fmt::Display for WebToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Query => "search_query_invalid",
            Self::Url => "fetch_url_invalid",
            Self::BodyLimit => "web_response_limit",
            Self::Schema => "web_response_schema",
            Self::ContextLimit => "web_context_limit",
        })
    }
}
impl std::error::Error for WebToolError {}

impl WebTask {
    /// Explicit commands only. A normal conversational question never silently incurs a search.
    pub fn parse(text: &str) -> Result<Option<Self>, WebToolError> {
        let text = text.trim();
        for (prefix, search) in [
            ("/search", true),
            ("/fetch", false),
            ("联网搜索", true),
            ("搜索", true),
            ("抓取", false),
        ] {
            let Some(rest) = text.strip_prefix(prefix) else {
                continue;
            };
            if prefix.starts_with('/') && !rest.is_empty() && !rest.starts_with(char::is_whitespace)
            {
                continue;
            }
            let input = rest.trim().trim_start_matches([':', '：']).trim();
            return if search {
                Self::search(input).map(Some)
            } else {
                Self::fetch(input).map(Some)
            };
        }
        Ok(None)
    }

    pub fn search(query: &str) -> Result<Self, WebToolError> {
        let query = query.trim();
        if query.is_empty()
            || query.chars().count() > MAX_QUERY_CHARS
            || query.chars().any(char::is_control)
        {
            return Err(WebToolError::Query);
        }
        Ok(Self::Search(query.to_owned()))
    }

    pub fn fetch(url: &str) -> Result<Self, WebToolError> {
        validate_url(url)?;
        Ok(Self::Fetch(url.to_owned()))
    }

    pub fn endpoint(&self) -> &'static str {
        match self {
            Self::Search(_) => "https://api.deepseek.com/v1/tools/search",
            Self::Fetch(_) => "https://api.deepseek.com/v1/tools/fetch",
        }
    }

    pub fn response_limit(&self) -> usize {
        match self {
            Self::Search(_) => SEARCH_RESPONSE_BYTES,
            Self::Fetch(_) => FETCH_RESPONSE_BYTES,
        }
    }

    pub fn request_body(&self) -> Result<Vec<u8>, WebToolError> {
        let value = match self {
            Self::Search(query) => serde_json::json!({
                "query": query, "count": SEARCH_COUNT, "need_content": false
            }),
            Self::Fetch(url) => serde_json::json!({"url":url}),
        };
        serde_json::to_vec(&value).map_err(|_| WebToolError::Schema)
    }

    pub fn question(&self) -> String {
        match self {
            Self::Search(query) => format!("请根据本次联网搜索资料，简洁回答：{query}"),
            Self::Fetch(_) => "请概括本次抓取网页的主要内容；区分网页陈述与已核实事实，不编造无法从资料确定的细节。".into(),
        }
    }

    pub fn parse_response(&self, bytes: &[u8]) -> Result<Retrieval, WebToolError> {
        if bytes.len() > self.response_limit() {
            return Err(WebToolError::BodyLimit);
        }
        let value: Value = serde_json::from_slice(bytes).map_err(|_| WebToolError::Schema)?;
        match self {
            Self::Search(_) => search_response(&value),
            Self::Fetch(url) => fetch_response(&value, url),
        }
    }
}

/// Only DeepSeek is contacted by the ESP. This also rejects clearly local targets before
/// handing a user-supplied address to the remote Fetch service's own security checks.
pub fn validate_url(url: &str) -> Result<(), WebToolError> {
    if url.is_empty()
        || url.len() > MAX_FETCH_URL_BYTES
        || url.chars().any(|c| c.is_control() || c.is_whitespace())
        || url.contains(['\\', '<', '>', '"'])
    {
        return Err(WebToolError::Url);
    }
    let rest = if url
        .get(..8)
        .is_some_and(|s| s.eq_ignore_ascii_case("https://"))
    {
        &url[8..]
    } else if url
        .get(..7)
        .is_some_and(|s| s.eq_ignore_ascii_case("http://"))
    {
        &url[7..]
    } else {
        return Err(WebToolError::Url);
    };
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .ok_or(WebToolError::Url)?;
    if authority.is_empty() || authority.contains('@') || !authority.is_ascii() {
        return Err(WebToolError::Url);
    }
    let host = if authority.starts_with('[') {
        let end = authority.find(']').ok_or(WebToolError::Url)?;
        let suffix = &authority[end + 1..];
        check_port(suffix)?;
        let host = &authority[1..end];
        let ip = host.parse::<IpAddr>().map_err(|_| WebToolError::Url)?;
        if !public_ip(ip) {
            return Err(WebToolError::Url);
        }
        return Ok(());
    } else {
        let (host, port) = authority.split_once(':').unwrap_or((authority, ""));
        if authority.contains(':') {
            if port.is_empty() {
                return Err(WebToolError::Url);
            }
            check_port(&authority[host.len()..])?;
        }
        host
    };
    let host = host.strip_suffix('.').unwrap_or(host);
    if let Ok(ip) = host.parse::<IpAddr>() {
        return if public_ip(ip) {
            Ok(())
        } else {
            Err(WebToolError::Url)
        };
    }
    if host.is_empty()
        || host.len() > 253
        || !host.contains('.')
        || host.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        || host.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
    {
        return Err(WebToolError::Url);
    }
    let lower = host.to_ascii_lowercase();
    if [
        "localhost",
        "local",
        "internal",
        "home.arpa",
        "invalid",
        "test",
    ]
    .iter()
    .any(|suffix| lower == *suffix || lower.ends_with(&format!(".{suffix}")))
    {
        return Err(WebToolError::Url);
    }
    Ok(())
}

fn check_port(suffix: &str) -> Result<(), WebToolError> {
    if suffix.is_empty() {
        return Ok(());
    }
    let digits = suffix.strip_prefix(':').ok_or(WebToolError::Url)?;
    if digits.is_empty()
        || !digits.bytes().all(|b| b.is_ascii_digit())
        || digits.parse::<u16>().ok().filter(|p| *p != 0).is_none()
    {
        return Err(WebToolError::Url);
    }
    Ok(())
}

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => public_ipv4(ip),
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return public_ipv4(v4);
            }
            // Current global unicast space only; exclude link-local, ULA, multicast,
            // transition mechanisms and documentation addresses conservatively.
            let s = ip.segments();
            (s[0] & 0xe000) == 0x2000
                && s[0] != 0x2002
                && !(s[0] == 0x2001 && (s[1] <= 0x001f || s[1] == 0x0db8))
        }
    }
}

fn public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !ip.is_private()
        && !ip.is_loopback()
        && !ip.is_link_local()
        && !ip.is_multicast()
        && !ip.is_unspecified()
        && a != 0
        && a < 224
        && !(a == 100 && (64..=127).contains(&b))
        && !(a == 192 && b == 0 && (c == 0 || c == 2))
        && !(a == 198 && (b == 18 || b == 19 || (b == 51 && c == 100)))
        && !(a == 203 && b == 0 && c == 113)
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Source {
    pub title: String,
    pub url: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Retrieval {
    pub kind: &'static str,
    pub context: String,
    pub sources: Vec<Source>,
    pub truncated: bool,
}

fn search_response(value: &Value) -> Result<Retrieval, WebToolError> {
    let results = value
        .get("results")
        .and_then(Value::as_array)
        .ok_or(WebToolError::Schema)?;
    if results.len() > SEARCH_COUNT {
        return Err(WebToolError::Schema);
    }
    let mut result = Retrieval {
        kind: "search",
        context: String::new(),
        sources: Vec::with_capacity(results.len()),
        truncated: false,
    };
    for item in results {
        let title = item
            .get("title")
            .and_then(Value::as_str)
            .ok_or(WebToolError::Schema)?;
        let url = item
            .get("url")
            .and_then(Value::as_str)
            .ok_or(WebToolError::Schema)?;
        if url.len() > MAX_SEARCH_URL_BYTES || validate_url(url).is_err() {
            // A source we cannot safely cite is never sent to the model as evidence.
            result.truncated = true;
            continue;
        }
        let summary = item
            .get("summary")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .or_else(|| item.get("snippet").and_then(Value::as_str))
            .ok_or(WebToolError::Schema)?;
        let source = Source {
            title: capped(title, MAX_TITLE_BYTES, &mut result.truncated),
            url: url.into(),
        };
        let text = capped(summary, MAX_SUMMARY_BYTES, &mut result.truncated);
        result.context.push_str(&format!(
            "[{}]\n标题：{}\n网址：{}\n检索摘要：{}\n\n",
            result.sources.len() + 1,
            source.title,
            source.url,
            text
        ));
        result.sources.push(source);
    }
    if result.sources.is_empty() {
        result.context = "本次搜索没有返回可用的公开网页来源。不得据此声称已核实用户问题。".into();
    }
    if result.context.len() > MAX_WEB_CONTEXT_BYTES {
        return Err(WebToolError::ContextLimit);
    }
    Ok(result)
}

fn fetch_response(value: &Value, url: &str) -> Result<Retrieval, WebToolError> {
    let title = value
        .get("title")
        .and_then(Value::as_str)
        .ok_or(WebToolError::Schema)?;
    let content = value
        .get("content")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or(WebToolError::Schema)?;
    let mut truncated = false;
    let title = capped(title, MAX_TITLE_BYTES, &mut truncated);
    let text = capped(content, MAX_FETCH_TEXT_BYTES, &mut truncated);
    let context = format!("[1]\n标题：{title}\n网址：{url}\n网页正文（Markdown）：\n{text}");
    if context.len() > MAX_WEB_CONTEXT_BYTES {
        return Err(WebToolError::ContextLimit);
    }
    Ok(Retrieval {
        kind: "fetch",
        context,
        sources: vec![Source {
            title,
            url: url.into(),
        }],
        truncated,
    })
}

fn capped(text: &str, maximum: usize, truncated: &mut bool) -> String {
    let mut end = text.len().min(maximum);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    *truncated |= end < text.len();
    text[..end].into()
}

/// Preserve real source URLs even when a model answer needs shortening for QQ.
pub fn append_sources(answer: &str, sources: &[Source]) -> String {
    if sources.is_empty() {
        return answer.to_owned();
    }
    let mut citations = "\n\n来源：\n".to_owned();
    for (i, source) in sources.iter().enumerate().take(SEARCH_COUNT) {
        if source.url.len() > MAX_FETCH_URL_BYTES || validate_url(&source.url).is_err() {
            continue;
        }
        let mut title_truncated = false;
        let title: String = capped(&source.title, 80, &mut title_truncated)
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        citations.push_str(&format!("[{}] {}\n{}\n", i + 1, title, source.url));
    }
    let budget = MAX_REPLY_BYTES.saturating_sub(citations.len() + TRUNCATED_NOTICE.len());
    let mut shortened = false;
    let mut reply = capped(answer, budget, &mut shortened);
    if shortened {
        reply.push_str(TRUNCATED_NOTICE);
    }
    reply.push_str(&citations);
    reply
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_commands_validate_unicode_query_and_keep_normal_chat_untouched() {
        assert_eq!(
            WebTask::parse("搜索：今天的天气").unwrap(),
            Some(WebTask::Search("今天的天气".into()))
        );
        assert!(matches!(
            WebTask::parse("/fetch https://example.com/a").unwrap(),
            Some(WebTask::Fetch(_))
        ));
        assert!(WebTask::parse("/searcher text").unwrap().is_none());
        assert!(WebTask::parse("普通问题").unwrap().is_none());
        assert!(WebTask::search(&"中".repeat(128)).is_ok());
        assert!(WebTask::search(&"中".repeat(129)).is_err());
        assert!(WebTask::parse("/search").is_err());
        assert!(WebTask::search("a\nb").is_err());
    }

    #[test]
    fn fetch_urls_reject_credentials_local_networks_and_ambiguous_authorities() {
        for bad in [
            "file:///tmp/a",
            "https://user:pw@example.com",
            "http://localhost/a",
            "http://a.local/x",
            "http://192.168.1.7",
            "http://127.0.0.1",
            "http://100.64.0.1",
            "http://169.254.1.1",
            "http://224.1.1.1",
            "http://[::1]",
            "http://[fc00::1]",
            "http://[::ffff:192.168.1.7]",
            "https://example.com:",
            "https://example.com:0",
            "https://example.com:65536",
            "https://0x7f000001",
            "https://2130706433",
            "https://a..com",
            "https://example.com\\@evil.com",
            "https://example.com/x y",
        ] {
            assert!(validate_url(bad).is_err(), "accepted {bad}");
        }
        for good in [
            "https://api-docs.deepseek.com/guides/search/",
            "HTTPS://example.com/a",
            "https://example.com:8443/a?q=b#c",
            "http://8.8.8.8",
            "https://[2606:4700:4700::1111]/",
            "https://example.com/中文",
        ] {
            assert!(validate_url(good).is_ok(), "rejected {good}");
        }
    }

    #[test]
    fn native_request_fields_and_remote_endpoints_are_exact() {
        let task = WebTask::search("DeepSeek").unwrap();
        assert_eq!(task.endpoint(), "https://api.deepseek.com/v1/tools/search");
        let value: Value = serde_json::from_slice(&task.request_body().unwrap()).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"query":"DeepSeek","count":3,"need_content":false})
        );
        let fetch = WebTask::fetch("https://example.com").unwrap();
        assert_eq!(fetch.endpoint(), "https://api.deepseek.com/v1/tools/fetch");
        assert_eq!(
            serde_json::from_slice::<Value>(&fetch.request_body().unwrap()).unwrap(),
            serde_json::json!({"url":"https://example.com"})
        );
    }

    #[test]
    fn parsing_bounds_search_summaries_and_keeps_true_source_urls() {
        let task = WebTask::search("test").unwrap();
        let body = serde_json::to_vec(&serde_json::json!({"results":[{"title":"标题","url":"https://example.com/a","summary":"中".repeat(2000)}]})).unwrap();
        let result = task.parse_response(&body).unwrap();
        assert!(result.truncated);
        assert!(result.context.len() < MAX_WEB_CONTEXT_BYTES);
        assert_eq!(result.sources[0].url, "https://example.com/a");
        assert!(task
            .parse_response(&vec![b' '; SEARCH_RESPONSE_BYTES + 1])
            .is_err());
        assert!(task
            .parse_response(br#"{"results":[{"title":"x"}]}"#)
            .is_err());
        let empty = task.parse_response(br#"{"results":[]}"#).unwrap();
        assert!(empty.sources.is_empty());
    }

    #[test]
    fn fetch_markdown_is_bounded_and_empty_pages_are_errors() {
        let task = WebTask::fetch("https://example.com/a").unwrap();
        let body =
            serde_json::to_vec(&serde_json::json!({"title":"hello","content":"中".repeat(12000)}))
                .unwrap();
        let result = task.parse_response(&body).unwrap();
        assert!(result.truncated);
        assert!(result.context.len() < MAX_WEB_CONTEXT_BYTES);
        assert_eq!(result.sources[0].url, "https://example.com/a");
        assert!(task
            .parse_response(br#"{"title":"none","content":" "}"#)
            .is_err());
    }

    #[test]
    fn final_reply_truncates_text_at_utf8_boundary_without_losing_sources() {
        let sources = vec![Source {
            title: "标题".repeat(100),
            url: "https://example.com/actual".into(),
        }];
        let result = append_sources(&"中".repeat(10000), &sources);
        assert!(result.len() <= MAX_REPLY_BYTES);
        assert!(result.contains(TRUNCATED_NOTICE));
        assert!(result.ends_with("https://example.com/actual\n"));
        let long_sources = (0..3)
            .map(|i| Source {
                title: "bad\0title\n".into(),
                url: format!("https://example.com/{i}/{}", "a".repeat(2000)),
            })
            .collect::<Vec<_>>();
        let result = append_sources(&"中".repeat(10000), &long_sources);
        assert!(result.len() <= MAX_REPLY_BYTES);
        assert!(!result.contains('\0'));
    }
}
