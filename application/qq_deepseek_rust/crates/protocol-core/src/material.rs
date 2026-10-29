//! Bounded QQ Markdown image-material protocol; no storage or network ownership.
use crate::{memory::MemoryError, record::crc32};
use serde::{Deserialize, Serialize};
pub const CHUNK_BYTES: usize = 1024;
pub const MAX_IMAGE_BYTES: u64 = 512 * 1024;
pub const MAX_CACHE_ENTRIES: usize = 32;
pub const MAX_CACHE_BYTES: usize = 2048;
pub const MAX_REFERENCE_BYTES: usize = 512;
pub const MAX_UPLOAD_URL_BYTES: usize = 4096;
pub const MAX_TTL: u64 = 90 * 86400;
pub const PROBE_NAME: &str = "copilotlee-probe.png";
pub const PROBE_IMAGE: &[u8] = include_bytes!("../assets/material-probe.png");
#[derive(Clone, Serialize, Deserialize)]
pub struct ImageInfo {
    pub name: String,
    pub bytes: u64,
    pub content_type: String,
    pub md5: String,
    pub sha1: String,
    pub width: u32,
    pub height: u32,
}
pub trait Digest {
    fn update(&mut self, data: &[u8]) -> Result<(), MemoryError>;
    fn finish(self) -> Result<(String, String), MemoryError>;
}
#[derive(Clone, Serialize, Deserialize)]
pub struct CachedImage {
    pub app_id: String,
    pub info: ImageInfo,
    pub image_id: String,
    pub reference: String,
    pub expires_at: u64,
}
impl CachedImage {
    pub fn validate(&self) -> bool {
        valid_app(&self.app_id)
            && self.info.validate()
            && !self.image_id.is_empty()
            && self.image_id.len() <= 128
            && self
                .image_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            && valid_reference(&self.reference)
            && self.expires_at > 0
    }
    pub fn usable(&self, app: &str, info: &ImageInfo, now: u64) -> bool {
        self.validate()
            && self.app_id == app
            && self.expires_at > now.saturating_add(60)
            && self.info.md5 == info.md5
            && self.info.sha1 == info.sha1
            && self.info.bytes == info.bytes
            && self.info.content_type == info.content_type
    }
}
impl ImageInfo {
    pub fn validate(&self) -> bool {
        mime(&self.name) == Some(self.content_type.as_str())
            && self.bytes > 0
            && self.bytes <= MAX_IMAGE_BYTES
            && hex(&self.md5, 32)
            && hex(&self.sha1, 40)
            && self.width <= 8192
            && self.height <= 8192
    }
    pub fn key(&self, app: &str) -> Result<String, MemoryError> {
        if !self.validate() || !valid_app(app) {
            return Err(MemoryError::Invalid);
        }
        Ok(format!(
            "{:08x}-{}-{}.qmi",
            crc32(app.as_bytes()),
            self.md5,
            self.sha1
        ))
    }
}
pub fn valid_app(app: &str) -> bool {
    !app.is_empty()
        && app.len() <= 128
        && app
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}
pub fn mime(name: &str) -> Option<&'static str> {
    if name.is_empty()
        || name.len() > 64
        || name.starts_with('.')
        || name.contains("..")
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
    {
        return None;
    }
    match name.rsplit_once('.')?.1.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}
pub fn hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit())
}
pub fn valid_reference(s: &str) -> bool {
    s.len() <= MAX_REFERENCE_BYTES
        && s.strip_prefix("qqbot-media://image/v1/").is_some_and(|t| {
            !t.is_empty()
                && t.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-.~=".contains(&b))
        })
}
pub fn presigned_url(s: &str) -> bool {
    if s.len() > MAX_UPLOAD_URL_BYTES
        || s.bytes()
            .any(|b| !b.is_ascii() || b.is_ascii_control() || b.is_ascii_whitespace())
        || s.contains('#')
    {
        return false;
    }
    let Some(rest) = s.strip_prefix("https://") else {
        return false;
    };
    let Some((host, path)) = rest.split_once('/') else {
        return false;
    };
    let host = host
        .strip_suffix(":443")
        .unwrap_or(host)
        .to_ascii_lowercase();
    !host.contains(['@', ':', '%'])
        && host.contains(".cos.")
        && host.ends_with(".myqcloud.com")
        && path
            .split_once('?')
            .is_some_and(|(p, q)| !p.is_empty() && !q.is_empty())
}
pub fn image_header(mime: &str, b: &[u8]) -> Option<(u32, u32)> {
    match mime {
        "image/png"
            if b.len() >= 24 && &b[..8] == b"\x89PNG\r\n\x1a\n" && &b[12..16] == b"IHDR" =>
        {
            let w = u32::from_be_bytes(b[16..20].try_into().ok()?);
            let h = u32::from_be_bytes(b[20..24].try_into().ok()?);
            (w > 0 && h > 0 && w <= 8192 && h <= 8192).then_some((w, h))
        }
        "image/jpeg" if b.starts_with(b"\xff\xd8\xff") => Some((0, 0)),
        "image/gif" if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") => Some((0, 0)),
        "image/webp" if b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" => Some((0, 0)),
        _ => None,
    }
}
pub fn encode(record: &CachedImage) -> Result<Vec<u8>, MemoryError> {
    if !record.validate() {
        return Err(MemoryError::Invalid);
    }
    let body = serde_json::to_vec(record).map_err(|_| MemoryError::Invalid)?;
    if body.len() > MAX_CACHE_BYTES {
        return Err(MemoryError::Capacity);
    }
    let mut out = b"QMI1".to_vec();
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&crc32(&body).to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}
pub fn decode(bytes: &[u8]) -> Result<Box<CachedImage>, MemoryError> {
    if bytes.len() < 12 || bytes.len() > MAX_CACHE_BYTES + 12 || &bytes[..4] != b"QMI1" {
        return Err(MemoryError::Corrupt);
    }
    let len =
        u32::from_le_bytes(bytes[4..8].try_into().map_err(|_| MemoryError::Corrupt)?) as usize;
    let crc = u32::from_le_bytes(bytes[8..12].try_into().map_err(|_| MemoryError::Corrupt)?);
    if len != bytes.len() - 12 || crc32(&bytes[12..]) != crc {
        return Err(MemoryError::Corrupt);
    }
    let r: Box<CachedImage> =
        serde_json::from_slice(&bytes[12..]).map_err(|_| MemoryError::Corrupt)?;
    if !r.validate() {
        return Err(MemoryError::Corrupt);
    }
    Ok(r)
}
#[derive(Clone)]
pub enum PictureCommand {
    List,
    Send {
        name: String,
        caption: String,
        stream: bool,
    },
}
impl PictureCommand {
    pub fn parse(text: &str) -> Result<Option<Self>, &'static str> {
        let mut parts = text.trim().splitn(2, char::is_whitespace);
        let first = parts.next().unwrap_or("");
        if !matches!(first, "/picture" | "/picture-stream") {
            return Ok(None);
        }
        let rest = parts.next().unwrap_or("").trim();
        if first == "/picture" && rest == "list" {
            return Ok(Some(Self::List));
        }
        let (name, caption) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        if mime(name).is_none()
            || caption.len() > 512
            || caption
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t')
        {
            return Err("用法：/picture 文件名.png 说明；/picture list；私聊可用 /picture-stream 文件名.png 说明。图片需位于 SD 的 qq-deepseek/media 目录。");
        }
        Ok(Some(Self::Send {
            name: name.into(),
            caption: caption.trim().into(),
            stream: first == "/picture-stream",
        }))
    }
}
pub fn caption(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if "\\`*_[]()!<>#|".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}
pub fn markdown(info: &ImageInfo, reference: &str, text: &str) -> Result<String, MemoryError> {
    if !info.validate() || !valid_reference(reference) || text.len() > 512 {
        return Err(MemoryError::Invalid);
    }
    let dim = if info.width > 0 && info.height > 0 {
        format!(" #{}px #{}px", info.width, info.height)
    } else {
        String::new()
    };
    Ok(format!(
        "{}![图片{dim}]({reference})",
        if text.is_empty() {
            String::new()
        } else {
            format!("{}\n\n", caption(text))
        }
    ))
}

pub enum StorageOp {
    Describe(String),
    Read(String, u64, u64),
    CacheLoad(Box<ImageInfo>, String),
    CacheSave(Box<CachedImage>),
    CacheRetire(Box<ImageInfo>, String, u64),
    List,
    Seed,
    Put(String, u64, Vec<u8>),
    Finish(String, u64),
}
pub enum StorageResult {
    Info(ImageInfo),
    Chunk(Vec<u8>),
    Cache(Option<Box<CachedImage>>),
    Done,
    Listing(serde_json::Value),
}
