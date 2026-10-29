//! Small UTF-8 QQ files are read on the existing inference worker, one at a time.
use super::protocol::file_attachment::{FileAttachment, FILE_CONTEXT_BYTES, MAX_FILE_BYTES};
use super::{http, Error};
use esp_idf_svc::http::Method;
use std::time::{Duration, Instant};

pub fn read_text(
    file: &FileAttachment,
    deadline: Instant,
    context_budget: usize,
    current: impl Fn() -> bool,
) -> Result<String, Error> {
    file.validate()
        .map_err(|_| Error::Protocol.at("qq_file_metadata"))?;
    if !current() {
        return Err(Error::Cancelled);
    }
    let deadline = deadline.min(Instant::now() + Duration::from_secs(20));
    // No QQ/DeepSeek credentials, redirects, arbitrary hosts, or SD handles.
    let mut connection = http::open(Method::Get, &file.url, None, None, &[], deadline)?;
    if connection.status() != 200 {
        return Err(Error::Protocol.at("qq_file_http_status"));
    }
    if connection
        .header("Content-Encoding")
        .is_some_and(|s| !s.eq_ignore_ascii_case("identity"))
    {
        return Err(Error::Protocol.at("qq_file_encoding"));
    }
    if connection
        .header("Content-Length")
        .is_some_and(|s| s.parse::<usize>().map_or(true, |n| n > MAX_FILE_BYTES))
    {
        return Err(Error::Limit.at("qq_file_size"));
    }
    let mut bytes = Vec::new();
    let mut buffer = vec![0u8; 1024];
    loop {
        if !current() {
            return Err(Error::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(Error::Timeout.at("qq_file_deadline"));
        }
        match http::read_chunk(&mut connection, &mut buffer)? {
            Some(0) => break,
            Some(n) => {
                if bytes.len() + n > MAX_FILE_BYTES {
                    return Err(Error::Limit.at("qq_file_size"));
                }
                bytes.try_reserve(n).map_err(|_| Error::Allocation)?;
                bytes.extend_from_slice(&buffer[..n]);
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    drop(connection);
    if file.size.is_some_and(|s| s != bytes.len() as u64) {
        return Err(Error::Protocol.at("qq_file_length_mismatch"));
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| Error::Protocol.at("qq_file_not_utf8"))?
        .trim_start_matches('\u{feff}');
    if text
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        return Err(Error::Protocol.at("qq_file_binary_content"));
    }
    let mut end = text.len().min(FILE_CONTEXT_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    loop {
        let encoded=serde_json::json!({"kind":"untrusted_text_file","reference_only":true,"name":file.name,"content":&text[..end],"source_bytes":bytes.len(),"truncated":end<text.len()}).to_string();
        if encoded.len() <= context_budget {
            return Ok(encoded);
        }
        if end == 0 {
            return Err(Error::Limit.at("qq_file_context_limit"));
        }
        end = end * 3 / 4;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
    }
}

/// Assemble bounded material before opening the model connection; no overlapping file TLS.
pub fn prepare(
    files: &[FileAttachment],
    mut input: String,
    mut reference: String,
    deadline: Instant,
    current: impl Fn() -> bool,
) -> Result<(String, String), Error> {
    if files.len() > super::protocol::file_attachment::MAX_FILES
        || input.len() + reference.len() > super::protocol::MAX_INPUT
    {
        return Err(Error::Limit.at("qq_file_context_limit"));
    }
    for file in files {
        if !current() {
            return Err(Error::Cancelled);
        }
        let budget = 4096usize
            .saturating_sub(reference.len())
            .min(super::protocol::MAX_INPUT.saturating_sub(input.len() + reference.len()))
            .saturating_sub(1);
        if budget < 256 {
            return Err(Error::Limit.at("qq_file_context_limit"));
        }
        let material=match read_text(file,deadline,budget,&current) {
            Ok(text)=>text,
            Err(Error::Cancelled)=>return Err(Error::Cancelled),
            Err(error)=>serde_json::json!({"kind":"unread_file","name":file.name,"error":error.label(),"content_unavailable":true}).to_string(),
        };
        if material.len() > budget {
            return Err(Error::Limit.at("qq_file_context_limit"));
        }
        reference.push_str(&material);
        reference.push('\n');
    }
    for file in files {
        input = input.replace(&file.url, "[file URL omitted]");
        reference = reference.replace(&file.url, "[file URL omitted]");
    }
    Ok((input, reference))
}
