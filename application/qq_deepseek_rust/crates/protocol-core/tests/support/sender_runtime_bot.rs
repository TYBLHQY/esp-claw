use qq_deepseek_core::{Config, CONFIG_VERSION};
use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc, Mutex,
};
#[path = "../../../../src/bot/protocol.rs"]
pub mod protocol;
#[path = "../../../../src/bot/sender.rs"]
mod sender;
#[path = "../../../../src/bot/stream_sender.rs"]
mod stream_sender;
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Error {
    Transport,
    Protocol,
    Config,
    Limit,
    Cancelled,
    Timeout,
    State,
    RemoteCode(i64),
    Rejected { status: u16, code: i64 },
}
impl Error {
    fn label(self) -> &'static str {
        "fixture_error"
    }
    fn at(self, _: &'static str) -> Self {
        self
    }
}
#[derive(Default)]
struct Counters {
    sent: AtomicU32,
}
struct ReplyContext {
    target: protocol::Target,
    message_id: String,
    reply_budget: Arc<protocol::reply_format::Budget>,
}
enum OutboundBody {
    Text(String),
    Media(protocol::media_reply::MediaReply),
    Picture(qq_deepseek_core::material::PictureCommand),
    Typing,
    Stream(Arc<protocol::streaming::Mailbox>),
}
struct Outbound {
    context: Arc<ReplyContext>,
    body: OutboundBody,
}
struct Shared {
    config: Config,
    memory_client: Option<()>,
    stop: AtomicBool,
    tokens: http::TokenCache,
    counters: Counters,
    errors: Mutex<Vec<Error>>,
    delivery: protocol::streaming::DeliveryStats,
    headroom: AtomicBool,
    current: AtomicBool,
}
impl Shared {
    fn current(&self, _: &ReplyContext) -> bool {
        !self.stop.load(Ordering::Relaxed) && self.current.load(Ordering::Relaxed)
    }
    fn network_headroom(&self) -> bool {
        self.headroom.load(Ordering::Relaxed)
    }
    fn error(&self, _: &'static str, e: Error) {
        self.errors.lock().unwrap().push(e);
    }
}
pub mod http {
    use super::*;
    use serde_json::Value;
    use std::collections::VecDeque;
    pub const BODY_LIMIT: usize = 16384;
    pub const QQ_API: &str = "https://api.bot.qq.com";
    pub static RESPONSES: Mutex<VecDeque<Result<Value, Error>>> = Mutex::new(VecDeque::new());
    pub static REQUESTS: Mutex<Vec<Value>> = Mutex::new(Vec::new());
    pub struct TokenCache;
    impl TokenCache {
        pub fn get(&self, _: &Config, _: &AtomicBool) -> Result<String, Error> {
            Ok("fixture-token".into())
        }
    }
    pub fn message_request(
        _: &str,
        _: &str,
        _: &str,
        body: &[u8],
        _: &AtomicBool,
    ) -> Result<Value, Error> {
        REQUESTS
            .lock()
            .unwrap()
            .push(serde_json::from_slice(body).unwrap());
        RESPONSES
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected extra POST")
    }
    pub fn json_request(
        _: crate::http::Method,
        uri: &str,
        auth: Option<&str>,
        app: Option<&str>,
        body: &[u8],
        stop: &AtomicBool,
    ) -> Result<Value, Error> {
        message_request(uri, auth.unwrap(), app.unwrap(), body, stop)
    }
}
static LOCK: Mutex<()> = Mutex::new(());
pub fn fixture(
    text: &str,
    outcomes: Vec<Result<serde_json::Value, Error>>,
) -> (Vec<serde_json::Value>, u32, Vec<Error>) {
    fixture_kind(text, None, outcomes)
}
pub fn fixture_kind(
    text: &str,
    media: Option<protocol::media_reply::MediaReply>,
    outcomes: Vec<Result<serde_json::Value, Error>>,
) -> (Vec<serde_json::Value>, u32, Vec<Error>) {
    let _guard = LOCK.lock().unwrap();
    http::REQUESTS.lock().unwrap().clear();
    *http::RESPONSES.lock().unwrap() = outcomes.into();
    let shared = Arc::new(Shared {
        config: Config {
            version: CONFIG_VERSION,
            generation: 1,
            wifi_ssid: "fixture".into(),
            wifi_password: "fixture".into(),
            deepseek_key: "fixture".into(),
            deepseek_model: "fixture".into(),
            qq_app_id: "fixture".into(),
            qq_app_secret: "fixture".into(),
            allow_users: vec!["fixture".into()],
            allow_groups: vec![],
        },
        stop: AtomicBool::new(false),
        tokens: http::TokenCache,
        counters: Counters::default(),
        memory_client: None,
        errors: Mutex::new(Vec::new()),
        delivery: Default::default(),
        headroom: AtomicBool::new(true),
        current: AtomicBool::new(true),
    });
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    tx.send(Outbound {
        context: Arc::new(ReplyContext {
            target: protocol::Target::User("fixture".into()),
            message_id: "trigger".into(),
            reply_budget: Arc::new(protocol::reply_format::Budget::default()),
        }),
        body: media.map_or_else(|| OutboundBody::Text(text.into()), OutboundBody::Media),
    })
    .unwrap();
    drop(tx);
    sender::run(shared.clone(), rx);
    let requests = http::REQUESTS.lock().unwrap().clone();
    let errors = shared.errors.lock().unwrap().clone();
    (
        requests,
        shared.counters.sent.load(Ordering::Relaxed),
        errors,
    )
}

#[cfg(test)]
mod stream_tests {
    use super::*;
    use serde_json::json;
    use std::time::{Duration, Instant};
    fn setup(outcomes: Vec<Result<serde_json::Value, Error>>) -> (Shared, Arc<ReplyContext>) {
        http::REQUESTS.lock().unwrap().clear();
        *http::RESPONSES.lock().unwrap() = outcomes.into();
        let shared = Shared {
            config: Config {
                version: CONFIG_VERSION,
                generation: 1,
                wifi_ssid: "fixture".into(),
                wifi_password: "fixture".into(),
                deepseek_key: "fixture".into(),
                deepseek_model: "fixture".into(),
                qq_app_id: "fixture".into(),
                qq_app_secret: "fixture".into(),
                allow_users: vec!["fixture".into()],
                allow_groups: vec![],
            },
            stop: AtomicBool::new(false),
            tokens: http::TokenCache,
            counters: Counters::default(),
            memory_client: None,
            errors: Mutex::new(Vec::new()),
            delivery: Default::default(),
            headroom: AtomicBool::new(true),
            current: AtomicBool::new(true),
        };
        let ctx = Arc::new(ReplyContext {
            target: protocol::Target::User("fixture".into()),
            message_id: "trigger".into(),
            reply_budget: Arc::new(protocol::reply_format::Budget::default()),
        });
        (shared, ctx)
    }
    #[test]
    fn typing_and_stream_share_quota_coalesce_throttle_and_finish() {
        let _lock = LOCK.lock().unwrap();
        let (s, ctx) = setup(vec![
            Ok(json!({})),
            Ok(json!({"id":"stream"})),
            Ok(json!({"id":"stream"})),
            Ok(json!({"id":"stream"})),
        ]);
        sender::send_reply(
            &s,
            Outbound {
                context: ctx.clone(),
                body: OutboundBody::Typing,
            },
        )
        .unwrap();
        let m = s.delivery.acquire().unwrap();
        let mut actor = stream_sender::Session::new(ctx, m.clone());
        let now = Instant::now();
        let a = "A".repeat(150);
        m.offer(&a, &[], now);
        assert!(!actor.poll_at(&s, now));
        m.offer(&(a.clone() + "B"), &[], now + Duration::from_secs(1));
        assert!(!actor.poll_at(&s, now + Duration::from_millis(200)));
        assert_eq!(http::REQUESTS.lock().unwrap().len(), 2);
        assert!(!actor.poll_at(&s, now + Duration::from_secs(1)));
        m.finish(&(a + "BC"), true);
        assert!(!actor.poll_at(&s, now + Duration::from_millis(1100)));
        assert!(actor.poll_at(&s, now + Duration::from_millis(2100)));
        let r = http::REQUESTS.lock().unwrap();
        assert_eq!(r[0]["msg_type"], 6);
        assert_eq!(r[0]["msg_seq"], 1);
        for (i, v) in r.iter().skip(1).enumerate() {
            assert_eq!(v["msg_seq"], 2);
            assert_eq!(v["index"], i);
        }
        assert_eq!(r[3]["input_state"], 10);
        assert_eq!(r[3]["stream_msg_id"], "stream");
        assert_eq!(s.delivery.stream_completed.load(Ordering::Relaxed), 1);
        assert_eq!(s.counters.sent.load(Ordering::Relaxed), 1);
    }
    #[test]
    fn explicit_initial_rejection_waits_for_final_and_uses_new_sequence() {
        let _lock = LOCK.lock().unwrap();
        let (s, ctx) = setup(vec![
            Err(Error::Rejected {
                status: 404,
                code: 1,
            }),
            Ok(json!({"id":"ordinary"})),
        ]);
        let m = s.delivery.acquire().unwrap();
        let mut actor = stream_sender::Session::new(ctx, m.clone());
        m.offer(&"A".repeat(150), &[], Instant::now());
        assert!(!actor.poll(&s));
        assert!(!actor.poll(&s));
        assert_eq!(http::REQUESTS.lock().unwrap().len(), 1);
        m.finish("final answer", true);
        assert!(actor.poll(&s));
        let r = http::REQUESTS.lock().unwrap();
        assert_eq!(r[0]["msg_seq"], 1);
        assert_eq!(r[1]["msg_seq"], 2);
        assert_eq!(r[1]["msg_type"], 2);
        assert!(!m.aborted());
    }
    #[test]
    fn ambiguous_or_throttled_stream_never_replays() {
        let _lock = LOCK.lock().unwrap();
        for error in [
            Err(Error::Transport),
            Err(Error::Rejected {
                status: 429,
                code: 1,
            }),
            Err(Error::RemoteCode(50002)),
            Ok(json!({})),
        ] {
            let (s, ctx) = setup(vec![error]);
            let m = s.delivery.acquire().unwrap();
            let mut actor = stream_sender::Session::new(ctx, m.clone());
            m.offer(&"A".repeat(150), &[], Instant::now());
            assert!(actor.poll(&s));
            assert!(m.aborted());
            assert_eq!(http::REQUESTS.lock().unwrap().len(), 1);
            assert_eq!(s.delivery.stream_unknown.load(Ordering::Relaxed), 1);
        }
    }
    #[test]
    fn cancellation_closes_confirmed_prefix_and_never_sends_pending_text() {
        let _lock = LOCK.lock().unwrap();
        let (s, ctx) = setup(vec![Ok(json!({"id":"stream"})), Ok(json!({"id":"stream"}))]);
        let m = s.delivery.acquire().unwrap();
        let mut actor = stream_sender::Session::new(ctx, m.clone());
        let now = Instant::now();
        m.offer(&"A".repeat(150), &[], now);
        assert!(!actor.poll_at(&s, now));
        m.offer(
            &("A".repeat(150) + "pending private material"),
            &[],
            now + Duration::from_secs(1),
        );
        s.current.store(false, Ordering::Relaxed);
        assert!(actor.poll_at(&s, now + Duration::from_secs(1)));
        let r = http::REQUESTS.lock().unwrap();
        assert_eq!(r[1]["input_state"], 10);
        assert!(!r[1]["content_raw"].as_str().unwrap().contains("pending"));
        assert!(r[1]["content_raw"].as_str().unwrap().contains("已停止"));
        assert_eq!(s.delivery.stream_cancelled.load(Ordering::Relaxed), 1);
    }
    #[test]
    fn rejected_update_gets_one_closing_attempt_with_new_index() {
        let _lock = LOCK.lock().unwrap();
        let (s, ctx) = setup(vec![
            Ok(json!({"id":"stream"})),
            Err(Error::Rejected {
                status: 400,
                code: 1,
            }),
            Ok(json!({"id":"stream"})),
        ]);
        let m = s.delivery.acquire().unwrap();
        let mut actor = stream_sender::Session::new(ctx, m.clone());
        let now = Instant::now();
        m.offer(&"A".repeat(150), &[], now);
        assert!(!actor.poll_at(&s, now));
        m.offer(
            &("A".repeat(150) + "pending"),
            &[],
            now + Duration::from_secs(1),
        );
        assert!(!actor.poll_at(&s, now + Duration::from_secs(1)));
        assert!(m.aborted());
        assert!(actor.poll_at(&s, now + Duration::from_secs(2)));
        let r = http::REQUESTS.lock().unwrap();
        assert_eq!(r.len(), 3);
        assert_eq!(r[2]["index"], 2);
        assert_eq!(r[2]["input_state"], 10);
        assert!(!r[2]["content_raw"].as_str().unwrap().contains("pending"));
        assert_eq!(s.delivery.stream_failed.load(Ordering::Relaxed), 1);
    }
    #[test]
    fn low_memory_defers_stream_and_skips_typing_without_spending_quota() {
        let _lock = LOCK.lock().unwrap();
        let (s, ctx) = setup(vec![Ok(json!({"id":"stream"}))]);
        s.headroom.store(false, Ordering::Relaxed);
        sender::send_reply(
            &s,
            Outbound {
                context: ctx.clone(),
                body: OutboundBody::Typing,
            },
        )
        .unwrap();
        let m = s.delivery.acquire().unwrap();
        let mut actor = stream_sender::Session::new(ctx, m.clone());
        m.finish("final answer", true);
        assert!(!actor.poll(&s));
        assert!(!m.aborted());
        assert!(http::REQUESTS.lock().unwrap().is_empty());
        s.headroom.store(true, Ordering::Relaxed);
        assert!(actor.poll(&s));
        assert_eq!(http::REQUESTS.lock().unwrap()[0]["msg_seq"], 1);
    }
    #[test]
    fn group_typing_is_ignored_and_stale_stream_never_posts() {
        let _lock = LOCK.lock().unwrap();
        let (s, ctx) = setup(vec![]);
        let group = ReplyContext {
            target: protocol::Target::Group("group".into()),
            message_id: "trigger".into(),
            reply_budget: ctx.reply_budget.clone(),
        };
        sender::send_reply(
            &s,
            Outbound {
                context: Arc::new(group),
                body: OutboundBody::Typing,
            },
        )
        .unwrap();
        s.current.store(false, Ordering::Relaxed);
        let m = s.delivery.acquire().unwrap();
        m.finish("private", true);
        let mut actor = stream_sender::Session::new(ctx, m.clone());
        assert!(actor.poll(&s));
        assert!(m.aborted());
        assert!(http::REQUESTS.lock().unwrap().is_empty());
    }
    #[test]
    fn cancelled_low_memory_stream_releases_slot_after_bounded_cleanup() {
        let _lock = LOCK.lock().unwrap();
        let (s, ctx) = setup(vec![Ok(json!({"id":"stream"}))]);
        let m = s.delivery.acquire().unwrap();
        let mut actor = stream_sender::Session::new(ctx, m.clone());
        let now = Instant::now();
        m.offer(&"A".repeat(150), &[], now);
        assert!(!actor.poll_at(&s, now));
        s.current.store(false, Ordering::Relaxed);
        s.headroom.store(false, Ordering::Relaxed);
        assert!(!actor.poll_at(&s, now + Duration::from_secs(1)));
        assert!(actor.poll_at(&s, now + Duration::from_secs(17)));
        assert!(m.aborted());
        assert_eq!(http::REQUESTS.lock().unwrap().len(), 1);
    }
    #[test]
    fn picture_markdown_verifies_image_and_never_falls_back_on_final_failure() {
        let _lock = LOCK.lock().unwrap();
        for result in [
            Err(Error::Transport),
            Err(Error::Rejected {
                status: 400,
                code: 40035009,
            }),
        ] {
            let (s, ctx) = setup(vec![result]);
            assert!(sender::send_picture_markdown(
                &s,
                &ctx,
                "![图片](qqbot-media://image/v1/fixture)"
            )
            .is_err());
            let r = http::REQUESTS.lock().unwrap();
            assert_eq!(r.len(), 1);
            assert_eq!(r[0]["force_verify_image_resource"], true);
            assert_eq!(r[0]["msg_type"], 2);
            assert_eq!(s.counters.sent.load(Ordering::Relaxed), 0);
        }
    }
    #[test]
    fn staged_picture_stream_flushes_preview_before_final_image_and_closes() {
        let _lock = LOCK.lock().unwrap();
        let (s, ctx) = setup(vec![Ok(json!({"id":"stream"})), Ok(json!({"id":"stream"}))]);
        let m = s.delivery.acquire().unwrap();
        m.preview("图文回复\n\n");
        let text = "图文回复\n\n![图片](qqbot-media://image/v1/fixture)";
        let mut actor = stream_sender::Session::staged(ctx, m.clone(), text.into());
        let now = Instant::now();
        assert!(!actor.poll_at(&s, now));
        assert!(!actor.poll_at(&s, now + Duration::from_millis(500)));
        assert!(actor.poll_at(&s, now + Duration::from_secs(2)));
        let r = http::REQUESTS.lock().unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0]["input_state"], 1);
        assert!(!r[0]["content_raw"]
            .as_str()
            .unwrap()
            .contains("qqbot-media"));
        assert_eq!(r[1]["input_state"], 10);
        assert_eq!(r[1]["content_raw"], text);
        assert_eq!(r[1]["msg_seq"], r[0]["msg_seq"]);
        assert_eq!(r[1]["index"], 1);
    }
    #[test]
    fn rejected_picture_stream_falls_back_only_to_verified_markdown_once() {
        let _lock = LOCK.lock().unwrap();
        let (s, ctx) = setup(vec![
            Err(Error::Rejected {
                status: 404,
                code: 1,
            }),
            Err(Error::Rejected {
                status: 400,
                code: 40035009,
            }),
        ]);
        let m = s.delivery.acquire().unwrap();
        m.preview("图文回复");
        let mut actor = stream_sender::Session::staged(
            ctx,
            m,
            "![图片](qqbot-media://image/v1/fixture)".into(),
        );
        assert!(!actor.poll(&s));
        assert!(actor.poll(&s));
        let r = http::REQUESTS.lock().unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[1]["force_verify_image_resource"], true);
        assert_eq!(r[1]["msg_type"], 2);
        assert_eq!(r[1]["msg_seq"], 2);
    }
}

mod materials {
    use super::*;
    pub struct Prepared {
        pub text: String,
    }
    pub fn list(_: &()) -> Result<String, Error> {
        Err(Error::Config)
    }
    pub fn prepare(
        _: &Config,
        _: &http::TokenCache,
        _: &(),
        _: &str,
        _: &str,
        _: &AtomicBool,
        _: impl Fn() -> bool,
    ) -> Result<Prepared, Error> {
        Err(Error::Config)
    }
}
