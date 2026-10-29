//! Small authenticated LAN manager. Device resources stay on the main control loop.
use esp_idf_svc::{
    http::{
        server::{Configuration, EspHttpConnection, EspHttpServer, Request},
        Method,
    },
    io::{EspIOError, Write},
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    sync::{
        mpsc::{sync_channel, SyncSender, TrySendError},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

const MAX_COMMAND_BYTES: usize = 2048;
const MAX_CONFIG_BYTES: usize = 6144;
const CONTROL_WAIT: Duration = Duration::from_secs(4);
const BODY_READ_DEADLINE: Duration = Duration::from_secs(6);
const INDEX: &[u8] = include_bytes!("web_assets/index.html");
const SCRIPT: &[u8] = include_bytes!("web_assets/app.js");
const STYLE: &[u8] = include_bytes!("web_assets/style.css");
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; frame-ancestors 'none'; form-action 'self'";

/// Receivers must execute on the device owner thread, never in an HTTP callback.
pub struct WebCommand {
    pub action: WebAction,
    pub respond_to: SyncSender<Result<Value, String>>,
}

pub enum WebAction {
    Command(String),
    /// Empty secret strings mean retain; root validates/merges before NVS commit.
    ConfigPatch {
        expected_generation: u32,
        patch: Value,
    },
}

/// Keeping this object alive keeps the SDK HTTP server and its callbacks alive.
pub struct WebServer {
    _server: EspHttpServer<'static>,
}

type HttpRequest<'a, 'b> = Request<&'a mut EspHttpConnection<'b>>;
type HttpResult = Result<(), EspIOError>;

impl WebServer {
    /// Snapshot shape: {"status": <redacted device status>, "config": <editable public fields>}.
    /// No route reads a device handle or executes network work in this task.
    pub fn start(
        command_tx: SyncSender<WebCommand>,
        snapshot: Arc<Mutex<Value>>,
        admin_token: String,
    ) -> Result<Self, String> {
        if !(32..=128).contains(&admin_token.len())
            || !admin_token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            return Err("invalid web management token".into());
        }
        let token = Arc::new(admin_token);
        let mut server = EspHttpServer::new(&Configuration {
            stack_size: 8192,
            max_open_sockets: 3,
            max_uri_handlers: 8,
            max_resp_headers: 10,
            session_timeout: Duration::from_secs(30),
            lru_purge_enable: true,
            ..Default::default()
        })
        .map_err(|error| format!("web start: {error}"))?;

        for (path, content_type, body) in [
            ("/", "text/html; charset=utf-8", INDEX),
            ("/app.js", "text/javascript; charset=utf-8", SCRIPT),
            ("/style.css", "text/css; charset=utf-8", STYLE),
        ] {
            server
                .fn_handler(path, Method::Get, move |req| {
                    respond(req, 200, content_type, body)
                })
                .map_err(|error| error.to_string())?;
        }

        let shared = snapshot.clone();
        let authorization = token.clone();
        server
            .fn_handler("/api/status", Method::Get, move |req| {
                if let Some((status, error)) = authorize(&req, &authorization, false) {
                    return failure(req, status, error);
                }
                let value = match shared.lock() {
                    Ok(value) => value.get("status").cloned().unwrap_or(Value::Null),
                    Err(_) => return failure(req, 503, "设备状态暂不可用"),
                };
                success(req, value)
            })
            .map_err(|error| error.to_string())?;

        let shared = snapshot;
        let authorization = token.clone();
        server
            .fn_handler("/api/config", Method::Get, move |req| {
                if let Some((status, error)) = authorize(&req, &authorization, false) {
                    return failure(req, status, error);
                }
                // Defense in depth: even a mistakenly oversized snapshot cannot export secrets.
                let value = match shared.lock() {
                    Ok(value) => public_configuration(value.get("config").unwrap_or(&Value::Null)),
                    Err(_) => return failure(req, 503, "配置暂不可用"),
                };
                success(req, value)
            })
            .map_err(|error| error.to_string())?;

        let tx = command_tx.clone();
        let authorization = token.clone();
        server
            .fn_handler("/api/command", Method::Post, move |mut req| {
                if let Some((status, error)) = authorize(&req, &authorization, true) {
                    return failure(req, status, error);
                }
                let body = match read_json_body(&mut req, MAX_COMMAND_BYTES) {
                    Ok(value) => value,
                    Err((status, error)) => return failure(req, status, error),
                };
                let command: CommandBody = match serde_json::from_slice(&body) {
                    Ok(value) => value,
                    Err(_) => return failure(req, 400, "指令格式不正确"),
                };
                if !command_allowed(&command.command) {
                    return failure(req, 400, "此操作未开放或参数不正确");
                }
                dispatch(
                    req,
                    &tx,
                    WebAction::Command(command.command.trim().to_owned()),
                )
            })
            .map_err(|error| error.to_string())?;

        let authorization = token;
        server
            .fn_handler("/api/config", Method::Put, move |mut req| {
                if let Some((status, error)) = authorize(&req, &authorization, true) {
                    return failure(req, status, error);
                }
                let body = match read_json_body(&mut req, MAX_CONFIG_BYTES) {
                    Ok(value) => value,
                    Err((status, error)) => return failure(req, status, error),
                };
                let config: PatchBody = match serde_json::from_slice(&body) {
                    Ok(value) => value,
                    Err(_) => return failure(req, 400, "配置格式不正确"),
                };
                if !config.patch.is_object() || !patch_keys_allowed(&config.patch) {
                    return failure(req, 400, "配置中含有未开放字段");
                }
                dispatch(
                    req,
                    &command_tx,
                    WebAction::ConfigPatch {
                        expected_generation: config.expected_generation,
                        patch: config.patch,
                    },
                )
            })
            .map_err(|error| error.to_string())?;
        Ok(Self { _server: server })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandBody {
    command: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchBody {
    expected_generation: u32,
    patch: Value,
}

fn authorize(
    req: &HttpRequest<'_, '_>,
    token: &str,
    mutation: bool,
) -> Option<(u16, &'static str)> {
    let supplied = req
        .header("Authorization")
        .and_then(|value| value.strip_prefix("Bearer "));
    if !supplied.is_some_and(|value| constant_time_equal(value.as_bytes(), token.as_bytes())) {
        return Some((401, "管理码不正确或尚未解锁"));
    }
    if mutation {
        if matches!(
            req.header("Sec-Fetch-Site"),
            Some("cross-site" | "cross-origin")
        ) {
            return Some((403, "请从设备自己的管理页面操作"));
        }
        if let Some(origin) = req.header("Origin") {
            let host = req.header("Host").unwrap_or("");
            if host.is_empty() || origin.strip_prefix("http://") != Some(host) {
                return Some((403, "操作来源与设备地址不一致"));
            }
        }
        if !req.header("Content-Type").is_some_and(|value| {
            value
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("application/json")
        }) {
            return Some((415, "此操作需要 JSON 请求"));
        }
    }
    None
}

fn constant_time_equal(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0_u8, |difference, (left, right)| {
        difference | (left ^ right)
    }) == 0
}

fn read_json_body(
    req: &mut HttpRequest<'_, '_>,
    maximum: usize,
) -> Result<Vec<u8>, (u16, &'static str)> {
    if req.header("Transfer-Encoding").is_some() {
        return Err((400, "不接受分块请求正文"));
    }
    let length = req
        .header("Content-Length")
        .and_then(|value| value.parse::<usize>().ok())
        .ok_or((411, "缺少请求长度"))?;
    if length == 0 || length > maximum {
        return Err((413, "请求内容过长或为空"));
    }
    let mut body = Vec::new();
    body.try_reserve_exact(length)
        .map_err(|_| (503, "设备内存暂不足"))?;
    body.resize(length, 0);
    let mut offset = 0;
    let deadline = Instant::now() + BODY_READ_DEADLINE;
    while offset < length {
        if Instant::now() >= deadline {
            return Err((408, "接收请求超时，请先查看设备状态"));
        }
        let read = req
            .read(&mut body[offset..])
            .map_err(|_| (408, "接收请求超时，请先查看设备状态"))?;
        if read == 0 {
            return Err((400, "请求正文不完整"));
        }
        offset += read;
    }
    Ok(body)
}

fn dispatch(
    req: HttpRequest<'_, '_>,
    tx: &SyncSender<WebCommand>,
    action: WebAction,
) -> HttpResult {
    let (respond_to, rx) = sync_channel(1);
    match tx.try_send(WebCommand { action, respond_to }) {
        Ok(()) => (),
        Err(TrySendError::Full(_)) => return failure(req, 503, "设备忙，请稍后操作"),
        Err(TrySendError::Disconnected(_)) => return failure(req, 503, "设备控制服务暂不可用"),
    }
    match rx.recv_timeout(CONTROL_WAIT) {
        Ok(Ok(value)) => success(req, value),
        Ok(Err(error)) => json_response(req, 409, json!({"ok":false,"error":error})),
        Err(_) => failure(req, 504, "操作结果尚未确认；请先刷新状态，不要重复提交"),
    }
}

fn public_configuration(config: &Value) -> Value {
    let mut result = serde_json::Map::new();
    for key in [
        "generation",
        "wifi_ssid",
        "deepseek_model",
        "qq_app_id",
        "allow_users",
        "allow_groups",
        "wifi_password_present",
        "deepseek_configured",
        "qq_configured",
    ] {
        if let Some(value) = config.get(key) {
            result.insert(key.into(), value.clone());
        }
    }
    Value::Object(result)
}

fn patch_keys_allowed(patch: &Value) -> bool {
    patch.as_object().is_some_and(|values| {
        values.keys().all(|key| {
            matches!(
                key.as_str(),
                "wifi_ssid"
                    | "wifi_password"
                    | "deepseek_model"
                    | "deepseek_key"
                    | "qq_app_id"
                    | "qq_app_secret"
                    | "allow_users"
                    | "allow_groups"
            )
        })
    })
}

fn command_allowed(text: &str) -> bool {
    const MAX_MEMORY_FACT_BYTES: usize = 512;
    let text = text.trim();
    if text.is_empty() || text.len() > MAX_COMMAND_BYTES || text.chars().any(char::is_control) {
        return false;
    }
    if matches!(
        text,
        "status"
            | "selftest"
            | "led on"
            | "led off"
            | "led status"
            | "bot status"
            | "bot stop"
            | "bot start echo"
            | "bot start deepseek"
            | "deepseek status"
            | "deepseek probe"
            | "deepseek test"
            | "sd status"
            | "sd mount"
            | "sd unmount"
            | "sd test"
            | "sd list"
            | "memory status"
            | "memory show"
            | "memory clear"
            | "memory compact"
            | "memory forget all"
    ) {
        return true;
    }
    if text.starts_with("led rgb ") {
        return matches!(
            qq_deepseek_core::parse_command(text),
            Ok(qq_deepseek_core::Command::Led(_))
        );
    }
    if let Some(query) = text.strip_prefix("deepseek search ") {
        return !query.trim().is_empty() && query.chars().count() <= 128;
    }
    if let Some(url) = text.strip_prefix("deepseek fetch ") {
        return (url.starts_with("https://") || url.starts_with("http://"))
            && !url.chars().any(char::is_whitespace)
            && url.len() <= 1536;
    }
    if let Some(fact) = text.strip_prefix("memory remember ") {
        return !fact.trim().is_empty() && fact.len() <= MAX_MEMORY_FACT_BYTES;
    }
    if let Some(index) = text.strip_prefix("memory forget ") {
        return !index.is_empty()
            && index.bytes().all(|byte| byte.is_ascii_digit())
            && index.parse::<u16>().is_ok_and(|value| value > 0);
    }
    false
}

fn success(req: HttpRequest<'_, '_>, value: Value) -> HttpResult {
    json_response(req, 200, json!({"ok":true,"result":value}))
}
fn failure(req: HttpRequest<'_, '_>, status: u16, error: &str) -> HttpResult {
    json_response(req, status, json!({"ok":false,"error":error}))
}
fn json_response(req: HttpRequest<'_, '_>, status: u16, body: Value) -> HttpResult {
    let bytes = serde_json::to_vec(&body)
        .unwrap_or_else(|_| b"{\"ok\":false,\"error\":\"response unavailable\"}".to_vec());
    respond(req, status, "application/json; charset=utf-8", &bytes)
}
fn respond(req: HttpRequest<'_, '_>, status: u16, content_type: &str, body: &[u8]) -> HttpResult {
    req.into_response(
        status,
        None,
        &[
            ("Content-Type", content_type),
            ("Cache-Control", "no-store"),
            ("Content-Security-Policy", CSP),
            ("X-Content-Type-Options", "nosniff"),
            ("Referrer-Policy", "no-referrer"),
            ("X-Frame-Options", "DENY"),
            ("Connection", "close"),
        ],
    )?
    .write_all(body)
}
