//! Explicit QQ-hosted media replies. The ESP submits URLs, never downloads output media.
use super::validate_image_url;
use serde_json::{json, Value};
#[derive(Clone, Copy)]
pub enum Kind {
    Image,
    File,
}
#[derive(Clone)]
pub struct MediaReply {
    pub kind: Kind,
    pub url: String,
}
impl MediaReply {
    pub fn parse(text: &str) -> Result<Option<Self>, &'static str> {
        let mut parts = text.split_whitespace();
        let kind = match parts.next() {
            Some("/image") => Kind::Image,
            Some("/file") => Kind::File,
            _ => return Ok(None),
        };
        let url = parts
            .next()
            .ok_or("Use /image HTTPS_URL or /file HTTPS_URL.")?;
        if parts.next().is_some() || validate_image_url(url).is_err() {
            return Err(
                "Media needs one public HTTPS URL, without credentials or local addresses.",
            );
        }
        Ok(Some(Self {
            kind,
            url: url.into(),
        }))
    }
    pub fn upload_body(&self) -> Value {
        json!({"file_type":match self.kind {Kind::Image=>1,Kind::File=>4},"url":self.url,"srv_send_msg":false})
    }
}
pub fn message_body(file_info: &str, msg_id: &str, seq: u32) -> Value {
    json!({"msg_type":7,"media":{"file_info":file_info},"msg_id":msg_id,"msg_seq":seq})
}
pub fn file_info(response: &Value) -> Option<&str> {
    response["file_info"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 8192 && !s.chars().any(char::is_control))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn media_commands_validate_public_urls_and_keep_uploads_nonproactive() {
        let image = MediaReply::parse("/image https://example.com/photo.png")
            .unwrap()
            .unwrap();
        let body = image.upload_body();
        assert_eq!(body["file_type"], 1);
        assert_eq!(body["srv_send_msg"], false);
        assert_eq!(
            MediaReply::parse("/file https://example.com/a.pdf")
                .unwrap()
                .unwrap()
                .upload_body()["file_type"],
            4
        );
        for text in [
            "/image",
            "/file http://example.com/a",
            "/file https://127.0.0.1/a",
            "/image https://example.com/a extra",
        ] {
            assert!(MediaReply::parse(text).is_err());
        }
        assert!(MediaReply::parse("hello").unwrap().is_none());
        assert_eq!(message_body("transient", "trigger", 2)["msg_type"], 7);
    }
}
