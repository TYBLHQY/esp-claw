//! DeepSeek-native web tools, run exclusively on an inference/diagnostic worker.
//! The ESP always connects to DeepSeek, never directly to an arbitrary Fetch URL.
use super::{http, Error};
use esp_idf_svc::http::Method;
use qq_deepseek_core::{
    web_tools::{Retrieval, WebTask, WebToolError},
    Config,
};
use serde_json::{json, Value};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

const TOOL_TIMEOUT: Duration = Duration::from_secs(45);
const DIAGNOSTIC_PREVIEW_BYTES: usize = 1200;
const READ_BUFFER_BYTES: usize = 2048;

/// The same bounded retrieval path is used by QQ and local diagnostics.
pub fn retrieve(
    config: &Config,
    task: &WebTask,
    deadline: Instant,
    current: impl Fn() -> bool,
) -> Result<Retrieval, Error> {
    if config.deepseek_key.is_empty() {
        return Err(Error::Config.at("web_tool_configuration"));
    }
    let deadline = deadline.min(Instant::now() + TOOL_TIMEOUT);
    if !current() {
        return Err(Error::Cancelled);
    }
    let body = task.request_body().map_err(input_error)?;
    let authorization = format!("Bearer {}", config.deepseek_key);
    let mut conn = http::open(
        Method::Post,
        task.endpoint(),
        Some(&authorization),
        None,
        &body,
        deadline,
    )?;
    if !(200..300).contains(&conn.status()) {
        // Fixed local stage plus HTTP status only; do not echo server bodies or URLs.
        return Err(Error::Diagnostic {
            stage: "web_tool_http_status",
            code: i32::from(conn.status()),
        });
    }
    if !conn.header("Content-Type").is_some_and(|value| {
        value
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .eq_ignore_ascii_case("application/json")
    }) {
        return Err(Error::Protocol.at("web_tool_content_type"));
    }
    let mut response = Vec::new();
    let mut buffer = vec![0; READ_BUFFER_BYTES];
    loop {
        if !current() {
            return Err(Error::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(Error::Timeout.at("web_tool_deadline"));
        }
        match http::read_chunk(&mut conn, &mut buffer)? {
            Some(0) => break,
            Some(n) => {
                if response.len().saturating_add(n) > task.response_limit() {
                    return Err(Error::Limit.at("web_tool_response_limit"));
                }
                response
                    .try_reserve(n)
                    .map_err(|_| Error::Allocation.at("web_tool_response_allocation"))?;
                response.extend_from_slice(&buffer[..n]);
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    task.parse_response(&response).map_err(response_error)
}

pub fn probe_search(config: Arc<Config>, query: &str) -> Result<Value, Error> {
    let task = WebTask::search(query).map_err(input_error)?;
    probe(config, task)
}

pub fn probe_fetch(config: Arc<Config>, url: &str) -> Result<Value, Error> {
    let task = WebTask::fetch(url).map_err(input_error)?;
    probe(config, task)
}

fn probe(config: Arc<Config>, task: WebTask) -> Result<Value, Error> {
    let retrieval = retrieve(&config, &task, Instant::now() + TOOL_TIMEOUT, || true)?;
    // Keep status snapshots/UART output bounded even when Fetch returns a long page.
    let mut end = retrieval.context.len().min(DIAGNOSTIC_PREVIEW_BYTES);
    while !retrieval.context.is_char_boundary(end) {
        end -= 1;
    }
    Ok(json!({
        "kind":retrieval.kind,
        "sources":retrieval.sources,
        "preview":&retrieval.context[..end],
        "context_bytes":retrieval.context.len(),
        "truncated":retrieval.truncated || end < retrieval.context.len(),
    }))
}

pub fn input_error(error: WebToolError) -> Error {
    match error {
        WebToolError::Query => Error::Protocol.at("search_query_invalid"),
        WebToolError::Url => Error::Protocol.at("fetch_url_invalid"),
        _ => Error::Protocol.at("web_tool_input"),
    }
}

fn response_error(error: WebToolError) -> Error {
    match error {
        WebToolError::BodyLimit => Error::Limit.at("web_tool_response_limit"),
        WebToolError::ContextLimit => Error::Limit.at("web_tool_context_limit"),
        _ => Error::Protocol.at("web_tool_response_schema"),
    }
}
