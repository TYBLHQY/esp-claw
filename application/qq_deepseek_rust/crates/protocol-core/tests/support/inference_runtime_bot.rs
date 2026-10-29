use qq_deepseek_core::Config;
use std::sync::{
    atomic::{AtomicBool, AtomicU32},
    Arc,
};
#[path = "../../../../src/bot/attachments.rs"]
pub mod attachments;
#[path = "../../../../src/bot/inference.rs"]
// Firmware declares Rust 1.82; keep its modulo check portable to that MSRV.
#[allow(clippy::manual_is_multiple_of)]
pub mod inference;
#[path = "../../../../src/bot/memory.rs"]
pub mod memory;
#[path = "../../../../src/bot/protocol.rs"]
pub mod protocol;
#[path = "../../../../src/bot/vision.rs"]
pub mod vision;
#[derive(Clone, Copy, Debug)]
pub enum Error {
    Transport,
    Protocol,
    Auth,
    State,
    Limit,
    Allocation,
    Cancelled,
    Timeout,
    Config,
    Thread,
    RemoteCode(i64),
    Diagnostic {
        stage: &'static str,
        code: i32,
    },
    Sse {
        stage: &'static str,
        error: qq_deepseek_core::SseError,
    },
}
impl Error {
    pub fn label(self) -> &'static str {
        match self {
            Self::Diagnostic { stage, .. } | Self::Sse { stage, .. } => stage,
            _ => "error",
        }
    }
    fn number(self) -> u32 {
        1
    }
    fn at(self, stage: &'static str) -> Self {
        Self::Diagnostic { stage, code: 1 }
    }
}
#[derive(Default)]
struct Counters {
    inference_started: AtomicU32,
    inference_completed: AtomicU32,
    inference_failed: AtomicU32,
    inference_cancelled: AtomicU32,
}
struct ReplyContext {
    target: protocol::Target,
    key: String,
    generation: u64,
    deadline: u64,
}
struct InferenceJob {
    context: Arc<ReplyContext>,
    input: String,
    images: Vec<protocol::ImageAttachment>,
    files: Vec<protocol::file_attachment::FileAttachment>,
    reference: String,
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
struct Finished {
    key: String,
    generation: u64,
}
struct Shared {
    stop: Arc<AtomicBool>,
    config: Arc<Config>,
    counters: Counters,
    memory_client: Option<crate::platform::sdcard::MemoryClient>,
    memory_stats: memory::Stats,
    vision_support: AtomicU32,
    delivery: protocol::streaming::DeliveryStats,
}
impl Shared {
    fn current(&self, _: &ReplyContext) -> bool {
        true
    }
    fn error(&self, _: &'static str, _: Error) {}
}
fn unix_now() -> Option<u64> {
    Some(1000)
}
pub mod http {
    use super::Error;
    use serde_json::Value;
    use std::{collections::VecDeque, sync::Mutex, time::Instant};
    pub const DEEPSEEK_API: &str = "https://api.deepseek.com/chat/completions";
    pub const BODY_LIMIT: usize = 16384;
    static RESPONSES: Mutex<VecDeque<Vec<u8>>> = Mutex::new(VecDeque::new());
    static REQUESTS: Mutex<Vec<Value>> = Mutex::new(Vec::new());
    pub struct Connection {
        body: Vec<u8>,
        offset: usize,
    }
    impl Connection {
        pub fn status(&self) -> u16 {
            200
        }
        pub fn header(&self, _: &str) -> Option<&str> {
            Some("text/event-stream")
        }
    }
    pub fn open(
        _: crate::http::Method,
        _: &str,
        _: Option<&str>,
        _: Option<&str>,
        body: &[u8],
        _: Instant,
    ) -> Result<Connection, Error> {
        if !body.is_empty() {
            REQUESTS
                .lock()
                .unwrap()
                .push(serde_json::from_slice(body).unwrap());
        }
        let body = RESPONSES
            .lock()
            .unwrap()
            .pop_front()
            .ok_or(Error::Transport)?;
        Ok(Connection { body, offset: 0 })
    }
    pub fn read_chunk(c: &mut Connection, buffer: &mut [u8]) -> Result<Option<usize>, Error> {
        let n = buffer.len().min(c.body.len() - c.offset);
        buffer[..n].copy_from_slice(&c.body[c.offset..c.offset + n]);
        c.offset += n;
        Ok(Some(n))
    }
    pub fn push(text: &str) {
        let json = serde_json::json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":"stop"}]});
        RESPONSES
            .lock()
            .unwrap()
            .push_back(format!("data: {json}\n\ndata: [DONE]\n\n").into_bytes());
    }
    pub fn raw(text: &str) {
        RESPONSES
            .lock()
            .unwrap()
            .push_back(text.as_bytes().to_vec());
    }
    pub fn requests() -> Vec<Value> {
        REQUESTS.lock().unwrap().clone()
    }
    pub fn clear() {
        RESPONSES.lock().unwrap().clear();
        REQUESTS.lock().unwrap().clear();
    }
}
pub mod web_tools {
    use super::Error;
    use qq_deepseek_core::{
        web_tools::{Retrieval, WebTask, WebToolError},
        Config,
    };
    use std::time::Instant;
    pub fn input_error(_: WebToolError) -> Error {
        Error::Protocol
    }
    pub fn retrieve(
        _: &Config,
        task: &WebTask,
        _: Instant,
        _: impl Fn() -> bool,
    ) -> Result<Retrieval, Error> {
        Ok(Retrieval {
            kind: "fixture",
            context: if matches!(task,WebTask::Search(q) if q=="escaped-material") {
                "\0".repeat(4096)
            } else {
                format!("Public fixture result: {task:?}")
            },
            sources: vec![qq_deepseek_core::web_tools::Source {
                title: "Example".into(),
                url: "https://example.com/".into(),
            }],
            truncated: false,
        })
    }
}

/// Exercise the actual worker entry, including its default delivery selection.
pub fn run_one(
    config: Arc<Config>,
    target: protocol::Target,
    input: &str,
) -> Vec<serde_json::Value> {
    let shared = Arc::new(Shared {
        stop: Arc::new(AtomicBool::new(false)),
        config,
        counters: Counters::default(),
        memory_client: None,
        memory_stats: memory::Stats::default(),
        vision_support: AtomicU32::new(1),
        delivery: Default::default(),
    });
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    assert!(tx
        .send(InferenceJob {
            context: Arc::new(ReplyContext {
                target,
                key: "u:default-fixture".into(),
                generation: 1,
                deadline: 1120,
            }),
            input: input.into(),
            images: vec![],
            files: vec![],
            reference: String::new(),
        })
        .is_ok());
    drop(tx);
    let (out, replies) = std::sync::mpsc::sync_channel(4);
    let (finished, _completion) = std::sync::mpsc::sync_channel(1);
    inference::run(shared, rx, out, finished);
    replies.into_iter().map(|reply| match reply.body {
        OutboundBody::Typing => serde_json::json!({"kind":"typing"}),
        OutboundBody::Text(text) => serde_json::json!({"kind":"text","text":text}),
        OutboundBody::Stream(m) => {
            let s=m.snapshot().unwrap();
            serde_json::json!({"kind":"stream","text":s.text,"complete":s.terminal==protocol::streaming::Terminal::Success,"after_tools":s.after_tools})
        }
        OutboundBody::Media(_) => serde_json::json!({"kind":"media"}),
        OutboundBody::Picture(_) => serde_json::json!({"kind":"picture"}),
    }).collect()
}
