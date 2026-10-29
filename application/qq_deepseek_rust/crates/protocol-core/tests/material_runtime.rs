#![allow(dead_code)]
extern crate self as esp_idf_svc;
pub mod http {
    pub enum Method {
        Get,
        Post,
    }
}
#[path = "support/material_runtime_bot.rs"]
mod bot;
pub mod platform {
    pub mod sdcard {
        use qq_deepseek_core::{
            material::{CachedImage, ImageInfo, StorageOp as Op, StorageResult as Out},
            memory::MemoryError,
        };
        use std::sync::{Arc, Mutex};
        #[derive(Clone)]
        pub struct MemoryClient(pub Arc<Mutex<State>>);
        pub struct State {
            pub info: ImageInfo,
            pub data: Vec<u8>,
            pub cache: Option<Box<CachedImage>>,
            pub fail_save: bool,
        }
        impl MemoryClient {
            pub fn material(&self, op: Op) -> Result<Out, MemoryError> {
                let mut s = self.0.lock().unwrap();
                match op {
                    Op::Describe(_) => Ok(Out::Info(s.info.clone())),
                    Op::CacheLoad(_, _) => Ok(Out::Cache(s.cache.clone())),
                    Op::CacheRetire(_, _, cutoff) => {
                        if s.cache.as_ref().is_some_and(|r| r.expires_at > cutoff) {
                            return Err(MemoryError::Conflict);
                        }
                        s.cache = None;
                        Ok(Out::Done)
                    }
                    Op::CacheSave(r) => {
                        if s.fail_save {
                            return Err(MemoryError::Unavailable);
                        }
                        if s.cache.is_some() {
                            return Err(MemoryError::Conflict);
                        }
                        s.cache = Some(r);
                        Ok(Out::Done)
                    }
                    Op::Read(_, len, offset) => {
                        assert_eq!(len, s.info.bytes);
                        let offset = offset as usize;
                        Ok(Out::Chunk(
                            s.data[offset..s.data.len().min(offset + 1024)].into(),
                        ))
                    }
                    _ => Err(MemoryError::Invalid),
                }
            }
        }
    }
}
use qq_deepseek_core::{material::ImageInfo, Config, CONFIG_VERSION};
use serde_json::json;
use std::sync::{atomic::AtomicBool, Arc, Mutex};
static LOCK: Mutex<()> = Mutex::new(());
fn setup() -> (Config, platform::sdcard::MemoryClient) {
    use bot::http;
    http::REQUESTS.lock().unwrap().clear();
    http::RESPONSES.lock().unwrap().clear();
    http::PUT_BYTES.lock().unwrap().clear();
    *http::PUT.lock().unwrap() = Ok("a".repeat(32));
    let cfg = Config {
        version: CONFIG_VERSION,
        generation: 1,
        wifi_ssid: "fixture".into(),
        wifi_password: "fixture".into(),
        deepseek_key: "fixture".into(),
        deepseek_model: "fixture".into(),
        qq_app_id: "123".into(),
        qq_app_secret: "fixture".into(),
        allow_users: vec!["fixture".into()],
        allow_groups: vec![],
    };
    let sd = platform::sdcard::MemoryClient(Arc::new(Mutex::new(platform::sdcard::State {
        info: ImageInfo {
            name: "fixture.png".into(),
            bytes: 5000,
            content_type: "image/png".into(),
            md5: "a".repeat(32),
            sha1: "b".repeat(40),
            width: 128,
            height: 64,
        },
        data: vec![7; 5000],
        cache: None,
        fail_save: false,
    })));
    (cfg, sd)
}
fn prepare_response() -> serde_json::Value {
    json!({"upload_id":"task-fixture","presigned_url":"https://bucket.cos.ap-guangzhou.myqcloud.com/robot_upload/fixture?q-signature=fixture","expires_in":3600})
}
fn complete_response() -> serde_json::Value {
    json!({"image_id":"image_123","url":"qqbot-media://image/v1/actual_fixture","ttl":7776000,"preview_url":"https://do-not-store.example/temporary"})
}
fn prepare(
    c: &Config,
    sd: &platform::sdcard::MemoryClient,
) -> Result<bot::materials::Prepared, bot::Error> {
    bot::materials::prepare(
        c,
        &bot::http::TokenCache::new(),
        sd,
        "fixture.png",
        "Caption",
        &AtomicBool::new(false),
        || true,
    )
}
#[test]
fn upload_is_bounded_then_reuses_durable_reference_without_any_network() {
    let _g = LOCK.lock().unwrap();
    let (c, sd) = setup();
    *bot::http::RESPONSES.lock().unwrap() =
        vec![Ok(prepare_response()), Ok(complete_response())].into();
    let p = prepare(&c, &sd).unwrap();
    assert!(!p.cache_hit);
    assert!(p.text.contains("qqbot-media://image/v1/actual_fixture"));
    let r = bot::http::REQUESTS.lock().unwrap().clone();
    assert_eq!(r.len(), 2);
    assert!(r[0]["uri"].as_str().unwrap().ends_with("/prepare"));
    assert_eq!(r[0]["body"]["file_size"], 5000);
    assert_eq!(r[1]["body"]["md5"], "a".repeat(32));
    assert_eq!(
        *bot::http::PUT_BYTES.lock().unwrap(),
        vec![1024, 1024, 1024, 1024, 904]
    );
    let cache = serde_json::to_string(sd.0.lock().unwrap().cache.as_ref().unwrap()).unwrap();
    assert!(!cache.contains("preview_url") && !cache.contains("presigned"));
    let p = prepare(&c, &sd).unwrap();
    assert!(p.cache_hit);
    assert_eq!(bot::http::REQUESTS.lock().unwrap().len(), 2);
}
#[test]
fn expired_reference_is_retired_before_refresh_without_fatfs_overwrite() {
    let _g = LOCK.lock().unwrap();
    let (c, sd) = setup();
    *bot::http::RESPONSES.lock().unwrap() =
        vec![Ok(prepare_response()), Ok(complete_response())].into();
    assert!(!prepare(&c, &sd).unwrap().cache_hit);
    sd.0.lock().unwrap().cache.as_mut().unwrap().expires_at = 1;
    *bot::http::RESPONSES.lock().unwrap() =
        vec![Ok(prepare_response()), Ok(complete_response())].into();
    assert!(!prepare(&c, &sd).unwrap().cache_hit);
    assert_eq!(bot::http::REQUESTS.lock().unwrap().len(), 4);
    assert!(sd.0.lock().unwrap().cache.as_ref().unwrap().expires_at > 1);
    assert!(prepare(&c, &sd).unwrap().cache_hit);
    assert_eq!(bot::http::REQUESTS.lock().unwrap().len(), 4);
}
#[test]
fn untrusted_upload_url_is_rejected_before_any_binary_send() {
    let _g = LOCK.lock().unwrap();
    let (c, sd) = setup();
    let mut r = prepare_response();
    r["presigned_url"] = "https://evil.example/upload?signature=fixture".into();
    bot::http::RESPONSES.lock().unwrap().push_back(Ok(r));
    assert!(prepare(&c, &sd).is_err());
    assert!(bot::http::PUT_BYTES.lock().unwrap().is_empty());
    assert_eq!(bot::http::REQUESTS.lock().unwrap().len(), 1);
    assert!(sd.0.lock().unwrap().cache.is_none());
}
#[test]
fn etag_mismatch_and_ambiguous_complete_never_replay_or_cache() {
    let _g = LOCK.lock().unwrap();
    let (c, sd) = setup();
    bot::http::RESPONSES
        .lock()
        .unwrap()
        .push_back(Ok(prepare_response()));
    *bot::http::PUT.lock().unwrap() = Ok("c".repeat(32));
    assert!(prepare(&c, &sd).is_err());
    assert_eq!(bot::http::REQUESTS.lock().unwrap().len(), 1);
    assert!(sd.0.lock().unwrap().cache.is_none());
    let (c, sd) = setup();
    *bot::http::RESPONSES.lock().unwrap() =
        vec![Ok(prepare_response()), Err(bot::Error::Transport)].into();
    assert!(prepare(&c, &sd).is_err());
    assert_eq!(bot::http::REQUESTS.lock().unwrap().len(), 2);
    assert!(sd.0.lock().unwrap().cache.is_none());
}
#[test]
fn unknown_prepare_or_cache_write_failure_never_repeats_upload() {
    let _g = LOCK.lock().unwrap();
    let (c, sd) = setup();
    bot::http::RESPONSES
        .lock()
        .unwrap()
        .push_back(Err(bot::Error::Transport));
    assert!(prepare(&c, &sd).is_err());
    assert_eq!(bot::http::REQUESTS.lock().unwrap().len(), 1);
    assert!(bot::http::PUT_BYTES.lock().unwrap().is_empty());
    let (c, sd) = setup();
    sd.0.lock().unwrap().fail_save = true;
    *bot::http::RESPONSES.lock().unwrap() =
        vec![Ok(prepare_response()), Ok(complete_response())].into();
    assert!(prepare(&c, &sd).is_err());
    assert_eq!(bot::http::REQUESTS.lock().unwrap().len(), 2);
    assert!(sd.0.lock().unwrap().cache.is_none());
}
#[test]
fn cancelled_request_does_not_contact_platform() {
    let _g = LOCK.lock().unwrap();
    let (c, sd) = setup();
    assert!(matches!(
        bot::materials::prepare(
            &c,
            &bot::http::TokenCache::new(),
            &sd,
            "fixture.png",
            "",
            &AtomicBool::new(false),
            || false
        ),
        Err(bot::Error::Cancelled)
    ));
    assert!(bot::http::REQUESTS.lock().unwrap().is_empty());
}

#[test]
fn material_list_accepts_protobuf_uint64_strings() {
    let _g = LOCK.lock().unwrap();
    let (c, sd) = setup();
    bot::http::RESPONSES
        .lock()
        .unwrap()
        .push_back(Ok(json!({"total":"12","images":[],"next_cursor":""})));
    let value = bot::materials::diagnostic(&c, &sd, "list", &AtomicBool::new(false)).unwrap();
    assert_eq!(value["total"], 12);
}
