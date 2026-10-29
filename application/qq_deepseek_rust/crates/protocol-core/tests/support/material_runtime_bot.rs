#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Error {
    Protocol,
    State,
    Limit,
    Cancelled,
    Timeout,
    Transport,
}
impl Error {
    fn at(self, _: &'static str) -> Self {
        self
    }
}
#[path = "../../../../src/bot/materials.rs"]
pub mod materials;
pub mod http {
    use super::Error;
    use qq_deepseek_core::Config;
    use serde_json::{json, Value};
    use std::{
        collections::VecDeque,
        sync::{
            atomic::{AtomicBool, Ordering},
            Mutex,
        },
        time::Instant,
    };
    pub const QQ_API: &str = "https://api.bot.qq.com";
    pub static REQUESTS: Mutex<Vec<Value>> = Mutex::new(Vec::new());
    pub static RESPONSES: Mutex<VecDeque<Result<Value, Error>>> = Mutex::new(VecDeque::new());
    pub static PUT: Mutex<Result<String, Error>> = Mutex::new(Ok(String::new()));
    pub static PUT_BYTES: Mutex<Vec<usize>> = Mutex::new(Vec::new());
    pub struct TokenCache;
    impl TokenCache {
        pub fn new() -> Self {
            Self
        }
        pub fn get(&self, _: &Config, stop: &AtomicBool) -> Result<String, Error> {
            if stop.load(Ordering::Relaxed) {
                Err(Error::Cancelled)
            } else {
                Ok("fixture-token".into())
            }
        }
    }
    pub fn json_request(
        _m: crate::http::Method,
        uri: &str,
        auth: Option<&str>,
        app: Option<&str>,
        body: &[u8],
        _: &AtomicBool,
    ) -> Result<Value, Error> {
        REQUESTS.lock().unwrap().push(json!({"uri":uri,"has_auth":auth.is_some(),"has_app":app.is_some(),"body":if body.is_empty(){Value::Null}else{serde_json::from_slice(body).unwrap()}}));
        RESPONSES
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected POST replay")
    }
    pub fn put_image(
        uri: &str,
        mime: &str,
        size: u64,
        _: Instant,
        current: impl Fn() -> bool,
        mut source: impl FnMut(u64) -> Result<Vec<u8>, Error>,
    ) -> Result<String, Error> {
        assert!(qq_deepseek_core::material::presigned_url(uri));
        assert_eq!(mime, "image/png");
        let mut offset = 0;
        while offset < size {
            if !current() {
                return Err(Error::Cancelled);
            }
            let b = source(offset)?;
            assert!(!b.is_empty() && b.len() <= 1024);
            offset += b.len() as u64;
            PUT_BYTES.lock().unwrap().push(b.len());
        }
        PUT.lock().unwrap().clone()
    }
}
