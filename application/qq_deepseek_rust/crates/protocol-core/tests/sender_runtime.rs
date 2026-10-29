#![allow(dead_code)]
extern crate self as esp_idf_svc;
pub mod http {
    pub enum Method {
        Post,
    }
}
#[path = "support/sender_runtime_bot.rs"]
mod bot;
use bot::{fixture, Error};
use serde_json::json;
#[test]
fn markdown_reply_uses_real_sender() {
    let (r, s, e) = fixture("# Hello **world**", vec![Ok(json!({"id":"accepted"}))]);
    assert_eq!(s, 1);
    assert!(e.is_empty());
    assert_eq!(r.len(), 1);
    assert_eq!(r[0]["msg_type"], 2);
    assert_eq!(r[0]["markdown"]["content"], "# Hello **world**");
}
#[test]
fn explicit_rejection_falls_back_once_with_new_sequence() {
    let (r, s, e) = fixture(
        "**test**",
        vec![
            Err(Error::Rejected {
                status: 400,
                code: 123,
            }),
            Ok(json!({"id":"plain"})),
        ],
    );
    assert_eq!(s, 1);
    assert!(e.is_empty());
    assert_eq!(r.len(), 2);
    assert_eq!(r[0]["msg_seq"], 1);
    assert_eq!(r[1]["msg_seq"], 2);
    assert_eq!(r[1]["msg_type"], 0);
    assert!(r[1].get("markdown").is_none());
}
#[test]
fn ambiguous_or_throttled_results_never_replay() {
    for outcome in [
        Err(Error::Transport),
        Err(Error::Rejected {
            status: 429,
            code: 429,
        }),
        Err(Error::Rejected {
            status: 500,
            code: 500,
        }),
        Ok(json!({"timestamp":1000})),
    ] {
        let (r, s, e) = fixture("text", vec![outcome]);
        assert_eq!(r.len(), 1);
        assert_eq!(s, 0);
        assert_eq!(e.len(), 1);
    }
}
#[test]
fn fallback_disables_markdown_for_remaining_parts_and_honors_quota() {
    let text = "你好\n".repeat(3000);
    let (r, s, e) = fixture(
        &text,
        vec![
            Err(Error::Rejected {
                status: 403,
                code: 123,
            }),
            Ok(json!({"id":"one"})),
            Ok(json!({"id":"two"})),
            Ok(json!({"id":"three"})),
        ],
    );
    assert_eq!(r.len(), 4);
    assert_eq!(s, 3);
    assert!(e.is_empty());
    for (n, v) in r.iter().enumerate() {
        assert_eq!(v["msg_seq"], n + 1);
    }
    assert!(r.iter().skip(1).all(|v| v["msg_type"] == 0));
    assert!(r.last().unwrap()["content"]
        .as_str()
        .unwrap()
        .ends_with("（回答已截断）"));
}
#[test]
fn failed_fallback_never_retries_again() {
    let (r, s, e) = fixture(
        "text",
        vec![
            Err(Error::Rejected {
                status: 400,
                code: 1,
            }),
            Err(Error::Transport),
        ],
    );
    assert_eq!(r.len(), 2);
    assert_eq!(s, 0);
    assert_eq!(e.len(), 1);
}

#[test]
fn late_fallback_marks_truncation_when_it_consumes_the_final_slot() {
    let (r, s, e) = fixture(
        &"x".repeat(15000),
        vec![
            Ok(json!({"id":"one"})),
            Ok(json!({"id":"two"})),
            Err(Error::Rejected {
                status: 400,
                code: 1,
            }),
            Ok(json!({"id":"three"})),
        ],
    );
    assert_eq!(r.len(), 4);
    assert_eq!(s, 3);
    assert!(e.is_empty());
    assert!(r[3]["content"]
        .as_str()
        .unwrap()
        .ends_with("（回答已截断）"));
}

#[test]
fn media_upload_is_not_proactive_and_file_info_is_sent_once() {
    let media =
        bot::protocol::media_reply::MediaReply::parse("/image https://example.com/photo.png")
            .unwrap();
    let (r, s, e) = bot::fixture_kind(
        "",
        media,
        vec![
            Ok(json!({"file_info":"transient-material","ttl":300})),
            Ok(json!({"id":"sent"})),
        ],
    );
    assert_eq!(r.len(), 2);
    assert_eq!(s, 1);
    assert!(e.is_empty());
    assert_eq!(r[0]["srv_send_msg"], false);
    assert_eq!(r[0]["file_type"], 1);
    assert_eq!(r[1]["msg_type"], 7);
    assert_eq!(r[1]["media"]["file_info"], "transient-material");
    assert_eq!(r[1]["msg_seq"], 1);
}
#[test]
fn failed_or_unknown_final_media_send_is_never_replayed() {
    let media =
        bot::protocol::media_reply::MediaReply::parse("/file https://example.com/a.pdf").unwrap();
    let (r, s, e) = bot::fixture_kind(
        "",
        media,
        vec![
            Ok(json!({"file_info":"transient-material","ttl":300})),
            Err(Error::Transport),
        ],
    );
    assert_eq!(r.len(), 2);
    assert_eq!(s, 0);
    assert_eq!(e.len(), 1);
}
#[test]
fn upload_failure_reports_an_error_without_claiming_a_media_send() {
    let media =
        bot::protocol::media_reply::MediaReply::parse("/file https://example.com/a.txt").unwrap();
    let (r, s, e) = bot::fixture_kind(
        "",
        media,
        vec![Err(Error::Transport), Ok(json!({"id":"error-notice"}))],
    );
    assert_eq!(r.len(), 2);
    assert_eq!(s, 1);
    assert_eq!(e.len(), 1);
    assert_eq!(r[1]["msg_type"], 2);
    assert!(r[1]["markdown"]["content"]
        .as_str()
        .unwrap()
        .contains("did not complete"));
}
