//! HTTP work runs only on workers. Callbacks and board control never call this module.
use super::Error;
use esp_idf_svc::handle::RawHandle;
use esp_idf_svc::http::client::{Configuration, EspHttpConnection, FollowRedirectsPolicy};
use esp_idf_svc::http::Method;
use qq_deepseek_core::Config;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

pub const QQ_API: &str = "https://api.bot.qq.com";
pub const DEEPSEEK_API: &str = "https://api.deepseek.com/chat/completions";
pub const BODY_LIMIT: usize = 16 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(12);

pub fn open(
    method: Method,
    uri: &str,
    authorization: Option<&str>,
    app_id: Option<&str>,
    body: &[u8],
    deadline: Instant,
) -> Result<EspHttpConnection, Error> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .ok_or(Error::Timeout.at("http_open_deadline"))?;
    let mut connection = EspHttpConnection::new(&Configuration {
        timeout: Some(CONNECT_TIMEOUT.min(remaining)),
        buffer_size: Some(2048),
        buffer_size_tx: Some(1024),
        crt_bundle_attach: Some(esp_idf_svc::sys::esp_crt_bundle_attach),
        follow_redirects_policy: FollowRedirectsPolicy::FollowNone,
        ..Default::default()
    })
    .map_err(|error| Error::Diagnostic {
        stage: "http_init",
        code: error.code(),
    })?;
    let length = body.len().to_string();
    let mut headers = vec![
        ("Content-Type", "application/json"),
        ("Content-Length", length.as_str()),
        ("Accept", "application/json, text/event-stream"),
    ];
    if let Some(auth) = authorization {
        headers.push(("Authorization", auth));
    }
    if let Some(id) = app_id {
        headers.push(("X-Union-Appid", id));
    }
    connection
        .initiate_request(method, uri, &headers)
        .map_err(|error| Error::Diagnostic {
            stage: "http_open",
            code: error.code(),
        })?;
    // Do not use a helper which can spin forever if the underlying transport writes zero bytes.
    let mut written = 0;
    while written < body.len() {
        set_phase_timeout(&connection, deadline, CONNECT_TIMEOUT)?;
        let n = connection
            .write(&body[written..])
            .map_err(|error| Error::Diagnostic {
                stage: "http_write",
                code: error.code(),
            })?;
        if n == 0 {
            return Err(Error::Transport.at("http_zero_write"));
        }
        written += n;
    }
    // Model first-token latency and server-side Fetch can exceed the TCP/TLS
    // connection budget. Do not replay a POST: wait on its existing connection.
    set_phase_timeout(
        &connection,
        deadline,
        if uri.starts_with("https://api.deepseek.com/") {
            Duration::from_secs(45)
        } else {
            CONNECT_TIMEOUT
        },
    )?;
    connection
        .initiate_response()
        .map_err(|error| Error::Diagnostic {
            stage: "http_headers",
            code: error.code(),
        })?;
    // After the bounded handshake/header phase, use short reads for cooperative cancellation.
    let result =
        unsafe { esp_idf_svc::sys::esp_http_client_set_timeout_ms(connection.handle(), 1000) };
    if result != esp_idf_svc::sys::ESP_OK {
        return Err(Error::Diagnostic {
            stage: "http_read_timeout_setting",
            code: result,
        });
    }
    Ok(connection)
}

fn set_phase_timeout(
    connection: &EspHttpConnection,
    deadline: Instant,
    maximum: Duration,
) -> Result<(), Error> {
    let left = deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
        .ok_or(Error::Timeout.at("http_open_deadline"))?;
    let milliseconds = left.min(maximum).as_millis().clamp(1, i32::MAX as u128) as i32;
    let code = unsafe {
        esp_idf_svc::sys::esp_http_client_set_timeout_ms(connection.handle(), milliseconds)
    };
    if code != esp_idf_svc::sys::ESP_OK {
        return Err(Error::Diagnostic {
            stage: "http_phase_timeout_setting",
            code,
        });
    }
    Ok(())
}

pub fn read_chunk(
    connection: &mut EspHttpConnection,
    buffer: &mut [u8],
) -> Result<Option<usize>, Error> {
    match connection.read(buffer) {
        Ok(0)
            if !unsafe {
                esp_idf_svc::sys::esp_http_client_is_complete_data_received(connection.handle())
            } =>
        {
            Ok(None)
        }
        Ok(n) => Ok(Some(n)),
        Err(error)
            if error.code() == esp_idf_svc::sys::ESP_ERR_HTTP_EAGAIN
                || error.code() == -esp_idf_svc::sys::ESP_ERR_HTTP_EAGAIN =>
        {
            Ok(None)
        }
        Err(error) => Err(Error::Diagnostic {
            stage: "http_body_read",
            code: error.code(),
        }),
    }
}

pub fn json_request(
    method: Method,
    uri: &str,
    authorization: Option<&str>,
    app_id: Option<&str>,
    body: &[u8],
    stop: &AtomicBool,
) -> Result<Value, Error> {
    json_inner(method, uri, authorization, app_id, body, stop, false)
}
/// Return explicit rejection metadata only after receiving the entire response.
pub fn message_request(
    uri: &str,
    authorization: &str,
    app_id: &str,
    body: &[u8],
    stop: &AtomicBool,
) -> Result<Value, Error> {
    json_inner(
        Method::Post,
        uri,
        Some(authorization),
        Some(app_id),
        body,
        stop,
        true,
    )
}
fn json_inner(
    method: Method,
    uri: &str,
    authorization: Option<&str>,
    app_id: Option<&str>,
    body: &[u8],
    stop: &AtomicBool,
    explicit_rejection: bool,
) -> Result<Value, Error> {
    let deadline = Instant::now() + Duration::from_secs(25);
    let mut conn = open(method, uri, authorization, app_id, body, deadline)?;
    let status = conn.status();
    let successful = (200..300).contains(&status);
    let status_error = || Error::Diagnostic {
        stage: "http_response_status",
        code: i32::from(status),
    };
    let mut response = Vec::new();
    let mut buffer = vec![0u8; 1024];
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(if successful {
                Error::Timeout.at("http_json_deadline")
            } else {
                status_error()
            });
        }
        let chunk = match read_chunk(&mut conn, &mut buffer) {
            Ok(chunk) => chunk,
            Err(error) => return Err(if successful { error } else { status_error() }),
        };
        match chunk {
            Some(0) => break,
            Some(n) => {
                if response.len() + n > BODY_LIMIT {
                    return Err(if successful {
                        Error::Limit.at("http_json_body_limit")
                    } else {
                        status_error()
                    });
                }
                response.try_reserve(n).map_err(|_| {
                    if successful {
                        Error::Allocation.at("http_json_allocation")
                    } else {
                        status_error()
                    }
                })?;
                response.extend_from_slice(&buffer[..n]);
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    let value: Value = serde_json::from_slice(&response).map_err(|_| {
        if successful {
            Error::Protocol.at("http_json_decode")
        } else {
            status_error()
        }
    })?;
    if let Some(code) = value
        .get("code")
        .and_then(|value| value.as_i64().or_else(|| value.as_str()?.parse().ok()))
        .filter(|code| *code != 0)
    {
        return Err(if explicit_rejection && !successful {
            Error::Rejected { status, code }
        } else {
            Error::RemoteCode(code)
        });
    }
    if !successful {
        return Err(if explicit_rejection {
            Error::Rejected {
                status,
                code: i64::from(status),
            }
        } else {
            status_error()
        });
    }
    Ok(value)
}

struct CachedToken {
    value: String,
    expires: Instant,
}
struct TokenState {
    token: Option<CachedToken>,
    refreshing: bool,
    retry_at: Instant,
}
/// A single refresh owner; the network operation happens outside the cache mutex.
pub struct TokenCache {
    state: Mutex<TokenState>,
    changed: Condvar,
}
impl TokenCache {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(TokenState {
                token: None,
                refreshing: false,
                retry_at: Instant::now(),
            }),
            changed: Condvar::new(),
        }
    }

    pub fn get(&self, config: &Config, stop: &AtomicBool) -> Result<String, Error> {
        let wait_until = Instant::now() + Duration::from_secs(30);
        loop {
            if stop.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let mut state = self.state.lock().map_err(|_| Error::State)?;
            let now = Instant::now();
            if let Some(token) = state.token.as_ref().filter(|token| token.expires > now) {
                return Ok(token.value.clone());
            }
            if state.refreshing {
                if now >= wait_until {
                    return Err(Error::Timeout);
                }
                let (guard, _) = self
                    .changed
                    .wait_timeout(state, Duration::from_millis(200))
                    .map_err(|_| Error::State)?;
                drop(guard);
                continue;
            }
            if now < state.retry_at {
                return Err(Error::Auth);
            }
            state.refreshing = true;
            drop(state);

            let fetched = fetch_token(config, stop);
            let mut state = self.state.lock().map_err(|_| Error::State)?;
            state.refreshing = false;
            let result = match fetched {
                Ok((value, duration)) => {
                    state.token = Some(CachedToken {
                        value: value.clone(),
                        expires: Instant::now() + duration,
                    });
                    Ok(value)
                }
                Err(error) => {
                    state.token = None;
                    state.retry_at = Instant::now() + Duration::from_secs(30);
                    Err(error)
                }
            };
            self.changed.notify_all();
            return result;
        }
    }
}

fn fetch_token(config: &Config, stop: &AtomicBool) -> Result<(String, Duration), Error> {
    let body = serde_json::to_vec(&json!({
        "appId": config.qq_app_id,
        "clientSecret": config.qq_app_secret,
    }))
    .map_err(|_| Error::Protocol)?;
    let value = json_request(
        Method::Post,
        &format!("{QQ_API}/app/getAppAccessToken"),
        None,
        None,
        &body,
        stop,
    )?;
    let token = value
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or(Error::Auth)?;
    if token.is_empty() || token.len() > 2048 || token.bytes().any(|b| b.is_ascii_control()) {
        return Err(Error::Auth);
    }
    let expires = value
        .get("expires_in")
        .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
        .filter(|n| (1..=86400).contains(n))
        .ok_or(Error::Auth)?;
    Ok((
        token.to_owned(),
        Duration::from_secs(expires.saturating_sub(30).max(1)),
    ))
}

/// Direct COS upload: fixed chunks, exact MIME/length, no QQ credentials or redirects.
pub fn put_image(
    uri: &str,
    content_type: &str,
    size: u64,
    deadline: Instant,
    current: impl Fn() -> bool,
    mut source: impl FnMut(u64) -> Result<Vec<u8>, Error>,
) -> Result<String, Error> {
    if !qq_deepseek_core::material::presigned_url(uri)
        || size == 0
        || size > qq_deepseek_core::material::MAX_IMAGE_BYTES
    {
        return Err(Error::Protocol.at("material_put_url"));
    }
    if !current() {
        return Err(Error::Cancelled);
    }
    let left = deadline
        .checked_duration_since(Instant::now())
        .ok_or(Error::Timeout)?;
    let mut conn = EspHttpConnection::new(&Configuration {
        timeout: Some(CONNECT_TIMEOUT.min(left)),
        buffer_size: Some(1024),
        // ESP-IDF prepares the whole request line, including the COS signature.
        buffer_size_tx: Some(qq_deepseek_core::material::MAX_UPLOAD_URL_BYTES + 1024),
        crt_bundle_attach: Some(esp_idf_svc::sys::esp_crt_bundle_attach),
        follow_redirects_policy: FollowRedirectsPolicy::FollowNone,
        ..Default::default()
    })
    .map_err(|e| Error::Diagnostic {
        stage: "material_put_init",
        code: e.code(),
    })?;
    let length = size.to_string();
    conn.initiate_request(
        Method::Put,
        uri,
        &[("Content-Type", content_type), ("Content-Length", &length)],
    )
    .map_err(|e| Error::Diagnostic {
        stage: "material_put_open",
        code: e.code(),
    })?;
    let mut offset = 0;
    while offset < size {
        if !current() {
            return Err(Error::Cancelled);
        }
        set_phase_timeout(&conn, deadline, CONNECT_TIMEOUT)?;
        let chunk = source(offset)?;
        if chunk.is_empty()
            || chunk.len() > qq_deepseek_core::material::CHUNK_BYTES
            || offset + chunk.len() as u64 > size
        {
            return Err(Error::Protocol.at("material_put_chunk"));
        }
        let mut written = 0;
        while written < chunk.len() {
            if !current() {
                return Err(Error::Cancelled);
            }
            set_phase_timeout(&conn, deadline, CONNECT_TIMEOUT)?;
            let n = conn
                .write(&chunk[written..])
                .map_err(|e| Error::Diagnostic {
                    stage: "material_put_write",
                    code: e.code(),
                })?;
            if n == 0 {
                return Err(Error::Transport.at("material_put_zero_write"));
            }
            written += n;
        }
        offset += chunk.len() as u64;
    }
    if !current() {
        return Err(Error::Cancelled);
    }
    set_phase_timeout(&conn, deadline, CONNECT_TIMEOUT)?;
    conn.initiate_response().map_err(|e| Error::Diagnostic {
        stage: "material_put_response",
        code: e.code(),
    })?;
    if !(200..300).contains(&conn.status()) {
        return Err(Error::Diagnostic {
            stage: "material_put_status",
            code: i32::from(conn.status()),
        });
    }
    let etag = conn
        .header("ETag")
        .ok_or(Error::Protocol.at("material_put_etag"))?
        .trim_matches('"')
        .to_ascii_lowercase();
    if !qq_deepseek_core::material::hex(&etag, 32) {
        return Err(Error::Protocol.at("material_put_etag"));
    }
    Ok(etag)
}
