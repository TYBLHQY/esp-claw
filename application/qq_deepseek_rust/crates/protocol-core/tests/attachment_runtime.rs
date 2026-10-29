#![allow(dead_code)]
extern crate self as esp_idf_svc;
#[path = "../../../src/bot/attachments.rs"]
mod attachments;
#[path = "../../../src/bot/protocol.rs"]
mod protocol;
#[derive(Clone, Copy, Debug)]
pub enum Error {
    Protocol,
    Cancelled,
    Limit,
    Timeout,
    Allocation,
}
impl Error {
    fn label(self) -> &'static str {
        "fixture_error"
    }
    fn at(self, _: &'static str) -> Self {
        self
    }
}
pub mod http {
    use super::*;
    use std::{
        sync::{
            atomic::{AtomicU32, Ordering},
            Mutex,
        },
        time::Instant,
    };
    pub enum Method {
        Get,
    }
    #[derive(Default)]
    pub struct Response {
        pub body: Vec<u8>,
        pub status: u16,
        pub length: Option<String>,
        pub encoding: Option<String>,
    }
    pub static RESPONSE: Mutex<Option<Response>> = Mutex::new(None);
    pub static OPENS: AtomicU32 = AtomicU32::new(0);
    pub struct Connection {
        response: Response,
        offset: usize,
    }
    impl Connection {
        pub fn status(&self) -> u16 {
            self.response.status
        }
        pub fn header(&self, key: &str) -> Option<&str> {
            match key {
                "Content-Length" => self.response.length.as_deref(),
                "Content-Encoding" => self.response.encoding.as_deref(),
                _ => None,
            }
        }
    }
    pub fn open(
        _: Method,
        uri: &str,
        auth: Option<&str>,
        app: Option<&str>,
        body: &[u8],
        deadline: Instant,
    ) -> Result<Connection, Error> {
        assert!(auth.is_none() && app.is_none() && body.is_empty());
        assert!(uri.starts_with("https://gzc-download.ftn.qq.com/"));
        if Instant::now() >= deadline {
            return Err(Error::Timeout);
        }
        OPENS.fetch_add(1, Ordering::Relaxed);
        Ok(Connection {
            response: RESPONSE.lock().unwrap().take().unwrap(),
            offset: 0,
        })
    }
    pub fn read_chunk(c: &mut Connection, buf: &mut [u8]) -> Result<Option<usize>, Error> {
        let n = buf.len().min(c.response.body.len() - c.offset);
        buf[..n].copy_from_slice(&c.response.body[c.offset..c.offset + n]);
        c.offset += n;
        Ok(Some(n))
    }
}
use std::{
    sync::{atomic::Ordering, Mutex},
    time::{Duration, Instant},
};
static LOCK: Mutex<()> = Mutex::new(());
fn file(size: Option<u64>) -> protocol::file_attachment::FileAttachment {
    protocol::file_attachment::FileAttachment {
        url: "https://gzc-download.ftn.qq.com/a?signature=transient".into(),
        name: "notes.txt".into(),
        size,
    }
}
fn response(body: &[u8]) {
    *http::RESPONSE.lock().unwrap() = Some(http::Response {
        body: body.into(),
        status: 200,
        ..Default::default()
    });
}
#[test]
fn production_reader_reads_utf8_without_forwarding_credentials_or_file_url() {
    let _guard = LOCK.lock().unwrap();
    response("你好，这是笔记。".as_bytes());
    let raw = attachments::read_text(
        &file(None),
        Instant::now() + Duration::from_secs(2),
        4096,
        || true,
    )
    .unwrap();
    let data: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(data["content"], "你好，这是笔记。");
    assert!(!raw.contains("signature="));
    assert_eq!(data["reference_only"], true);
}
#[test]
fn encoded_material_budget_handles_unicode_newlines_and_bom() {
    let _guard = LOCK.lock().unwrap();
    response(format!("\u{feff}{}", "你好\r\n".repeat(1000)).as_bytes());
    let raw = attachments::read_text(
        &file(None),
        Instant::now() + Duration::from_secs(2),
        512,
        || true,
    )
    .unwrap();
    assert!(raw.len() <= 512);
    let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(value["truncated"], true);
    assert!(value["content"].as_str().unwrap().starts_with("你好"));
}
#[test]
fn oversized_binary_wrong_size_and_compressed_inputs_fail_closed() {
    let _guard = LOCK.lock().unwrap();
    for body in [
        vec![b'a'; 16 * 1024 + 1],
        vec![255, 254, 0],
        b"a\0b".to_vec(),
    ] {
        response(&body);
        assert!(attachments::read_text(
            &file(None),
            Instant::now() + Duration::from_secs(2),
            4096,
            || true
        )
        .is_err());
    }
    response(b"abc");
    assert!(attachments::read_text(
        &file(Some(5)),
        Instant::now() + Duration::from_secs(2),
        4096,
        || true
    )
    .is_err());
    *http::RESPONSE.lock().unwrap() = Some(http::Response {
        body: vec![],
        status: 200,
        encoding: Some("gzip".into()),
        ..Default::default()
    });
    assert!(attachments::read_text(
        &file(None),
        Instant::now() + Duration::from_secs(2),
        4096,
        || true
    )
    .is_err());
    *http::RESPONSE.lock().unwrap() = Some(http::Response {
        body: vec![],
        status: 200,
        length: Some("999999".into()),
        ..Default::default()
    });
    assert!(attachments::read_text(
        &file(None),
        Instant::now() + Duration::from_secs(2),
        4096,
        || true
    )
    .is_err());
}
#[test]
fn unauthorized_origin_and_cancellation_never_open_a_connection() {
    let _guard = LOCK.lock().unwrap();
    let before = http::OPENS.load(Ordering::Relaxed);
    let mut bad = file(None);
    bad.url = "https://example.com/arbitrary".into();
    assert!(
        attachments::read_text(&bad, Instant::now() + Duration::from_secs(2), 4096, || true)
            .is_err()
    );
    assert!(attachments::read_text(
        &file(None),
        Instant::now() + Duration::from_secs(2),
        4096,
        || false
    )
    .is_err());
    assert_eq!(http::OPENS.load(Ordering::Relaxed), before);
}

#[test]
fn prepare_keeps_download_urls_out_of_model_material_and_commands() {
    let _guard = LOCK.lock().unwrap();
    let f = file(None);
    response(b"A fictional comet is named Cedar.");
    let input = format!("Read {}", f.url);
    let (input, reference) = attachments::prepare(
        std::slice::from_ref(&f),
        input,
        "text-file metadata\n".into(),
        Instant::now() + Duration::from_secs(2),
        || true,
    )
    .unwrap();
    assert!(!input.contains("signature=") && !reference.contains("signature="));
    assert!(reference.contains("Cedar"));
    assert!(reference.len() <= 4096);
}
