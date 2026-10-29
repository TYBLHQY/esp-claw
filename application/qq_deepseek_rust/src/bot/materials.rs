//! QQ image materials. No SDK/SD handle leaves its owner; no unknown POST is replayed.
use super::{http, Error};
use crate::platform::sdcard::MemoryClient;
use qq_deepseek_core::{
    material::{self, CachedImage, ImageInfo, StorageOp, StorageResult},
    memory::MemoryError,
    Config,
};
use serde_json::{json, Value};
use std::{
    sync::atomic::AtomicBool,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
pub struct Prepared {
    pub text: String,
    pub cache_hit: bool,
    pub info: Box<ImageInfo>,
    pub upload_url_bytes: usize,
}
fn storage(e: MemoryError) -> Error {
    match e {
        MemoryError::Timeout => Error::Timeout.at("material_sd_deadline"),
        MemoryError::Capacity => Error::Limit.at("material_sd_limit"),
        MemoryError::Corrupt => Error::Protocol.at("material_cache_corrupt"),
        _ => Error::State.at("material_sd_unavailable"),
    }
}
fn now() -> Result<u64, Error> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|v| v.as_secs())
        .map_err(|_| Error::State)
}
fn check(deadline: Instant, current: &impl Fn() -> bool) -> Result<(), Error> {
    if !current() {
        return Err(Error::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(Error::Timeout.at("material_deadline"));
    }
    Ok(())
}
pub fn prepare(
    config: &Config,
    tokens: &http::TokenCache,
    sd: &MemoryClient,
    name: &str,
    caption: &str,
    stop: &AtomicBool,
    current: impl Fn() -> bool,
) -> Result<Prepared, Error> {
    let deadline = Instant::now() + Duration::from_secs(95);
    check(deadline, &current)?;
    let StorageResult::Info(info) = sd
        .material(StorageOp::Describe(name.into()))
        .map_err(storage)?
    else {
        return Err(Error::Protocol);
    };
    let StorageResult::Cache(cached) = sd
        .material(StorageOp::CacheLoad(
            Box::new(info.clone()),
            config.qq_app_id.clone(),
        ))
        .map_err(storage)?
    else {
        return Err(Error::Protocol);
    };
    let timestamp = now()?;
    if let Some(r) = cached {
        if r.usable(&config.qq_app_id, &info, timestamp) {
            return Ok(Prepared {
                text: material::markdown(&info, &r.reference, caption).map_err(storage)?,
                cache_hit: true,
                upload_url_bytes: 0,
                info: Box::new(info),
            });
        }
        sd.material(StorageOp::CacheRetire(
            Box::new(info.clone()),
            config.qq_app_id.clone(),
            timestamp.saturating_add(60),
        ))
        .map_err(storage)?;
    }
    check(deadline, &current)?;
    let token = tokens.get(config, stop)?;
    let auth = format!("QQBot {token}");
    let body=serde_json::to_vec(&json!({"file_name":info.name,"file_size":info.bytes,"content_type":info.content_type,"md5":info.md5,"sha1":info.sha1})).map_err(|_|Error::Protocol)?;
    let upload = http::json_request(
        esp_idf_svc::http::Method::Post,
        &format!("{}/v2/media-images/prepare", http::QQ_API),
        Some(&auth),
        Some(&config.qq_app_id),
        &body,
        stop,
    )?;
    let id = upload["upload_id"]
        .as_str()
        .filter(|v| !v.is_empty() && v.len() <= 256 && !v.chars().any(char::is_control))
        .ok_or(Error::Protocol.at("material_upload_id"))?;
    let uri = upload["presigned_url"]
        .as_str()
        .filter(|v| material::presigned_url(v))
        .ok_or(Error::Protocol.at("material_upload_url"))?;
    let expires = upload["expires_in"]
        .as_u64()
        .filter(|v| *v > 10)
        .ok_or(Error::Protocol.at("material_upload_expiry"))?;
    check(deadline, &current)?;
    let etag = http::put_image(
        uri,
        &info.content_type,
        info.bytes,
        deadline.min(Instant::now() + Duration::from_secs(expires)),
        &current,
        |offset| match sd
            .material(StorageOp::Read(info.name.clone(), info.bytes, offset))
            .map_err(storage)?
        {
            StorageResult::Chunk(b) => Ok(b),
            _ => Err(Error::Protocol),
        },
    )?;
    if etag != info.md5 {
        return Err(Error::Protocol.at("material_etag_mismatch"));
    }
    check(deadline, &current)?;
    let body =
        serde_json::to_vec(&json!({"upload_id":id,"md5":etag})).map_err(|_| Error::Protocol)?;
    let done = http::json_request(
        esp_idf_svc::http::Method::Post,
        &format!("{}/v2/media-images/complete", http::QQ_API),
        Some(&auth),
        Some(&config.qq_app_id),
        &body,
        stop,
    )?;
    let image_id = done["image_id"]
        .as_str()
        .ok_or(Error::Protocol.at("material_image_id"))?
        .to_owned();
    let reference = done["url"]
        .as_str()
        .ok_or(Error::Protocol.at("material_reference"))?
        .to_owned();
    let ttl = done["ttl"]
        .as_u64()
        .filter(|v| *v > 60)
        .ok_or(Error::Protocol.at("material_ttl"))?
        .min(material::MAX_TTL);
    let record = Box::new(CachedImage {
        app_id: config.qq_app_id.clone(),
        info: info.clone(),
        image_id,
        reference: reference.clone(),
        expires_at: timestamp.saturating_add(ttl),
    });
    if !record.validate() {
        return Err(Error::Protocol.at("material_complete_fields"));
    }
    // Never store the temporary preview_url or a pre-signed upload URL.
    sd.material(StorageOp::CacheSave(record)).map_err(storage)?;
    check(deadline, &current)?;
    Ok(Prepared {
        text: material::markdown(&info, &reference, caption).map_err(storage)?,
        cache_hit: false,
        upload_url_bytes: uri.len(),
        info: Box::new(info),
    })
}
pub fn list(sd: &MemoryClient) -> Result<String, Error> {
    let StorageResult::Listing(value) = sd.material(StorageOp::List).map_err(storage)? else {
        return Err(Error::Protocol);
    };
    let images = value["images"].as_array().ok_or(Error::Protocol)?;
    if images.is_empty() {
        return Ok("SD 的 qq-deepseek/media 目录中还没有可发送的图片。".into());
    }
    let mut text = "SD 图片（每张最多 512 KiB）：\n".to_owned();
    for image in images {
        if let (Some(name), Some(bytes)) = (image["name"].as_str(), image["bytes"].as_u64()) {
            text.push_str(&format!("- {name} ({bytes} B)\n"));
        }
    }
    if value["truncated"] == true {
        text.push_str("列表已截断。\n");
    }
    text.push_str("发送：/picture 文件名 说明");
    Ok(text)
}

pub fn diagnostic(
    config: &Config,
    sd: &MemoryClient,
    name: &str,
    stop: &AtomicBool,
) -> Result<Value, Error> {
    if name == "list" {
        let tokens = http::TokenCache::new();
        let token = tokens.get(config, stop)?;
        let auth = format!("QQBot {token}");
        let r = http::json_request(
            esp_idf_svc::http::Method::Get,
            &format!("{}/v2/media-images", http::QQ_API),
            Some(&auth),
            Some(&config.qq_app_id),
            &[],
            stop,
        )?;
        return Ok(
            json!({"total":r["total"].as_u64().or_else(||r["total"].as_str()?.parse::<u64>().ok()).ok_or(Error::Protocol.at("material_total"))?,"listed_images":r["images"].as_array().map(Vec::len),"more":r["next_cursor"].as_str().is_some_and(|v|!v.is_empty())}),
        );
    }
    let tokens = http::TokenCache::new();
    let result = prepare(config, &tokens, sd, name, "", stop, || {
        !stop.load(std::sync::atomic::Ordering::Relaxed)
    })?;
    Ok(
        json!({"uploaded_or_reused":true,"cache_hit":result.cache_hit,"bytes":result.info.bytes,"md5":result.info.md5,"sha1":result.info.sha1,"width":result.info.width,"height":result.info.height,"reference_valid":true,"upload_url_bytes":result.upload_url_bytes}),
    )
}
