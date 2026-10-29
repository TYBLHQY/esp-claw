//! Bounded QQ content normalization; no network, storage, commands or trust decisions.
use super::{
    image_content_type, metadata_number, ImageAttachment, ImageError, MAX_IMAGE_DIMENSION,
    MAX_INPUT,
};
use serde_json::{json, Value};

pub const MAX_IMAGES: usize = 2;
const MAX_DEPTH: usize = 3;
const MAX_NODES: usize = 16;
const MAX_ATTACHMENTS: usize = 4;
const MAX_REFERENCE_BYTES: usize = 4096;
const MAX_CARD_FIELD: usize = 512;

#[derive(Clone, Default)]
pub struct Content {
    /// Only the live outer message can become a command. ASR remains material.
    pub text: String,
    pub reference: String,
    pub images: Vec<ImageAttachment>,
    pub files: Vec<super::file_attachment::FileAttachment>,
    pub image_error: Option<ImageError>,
}
impl Content {
    pub fn has_material(&self) -> bool {
        !self.reference.is_empty()
            || !self.images.is_empty()
            || !self.files.is_empty()
            || self.image_error.is_some()
    }
}

pub fn normalize(data: &Value, text: &str) -> Option<Content> {
    if text.len() > MAX_INPUT {
        return None;
    }
    let mut state = State {
        output: Content {
            text: faces(if text.is_empty() {
                outer_asr(data)
                    .map(|s| clip(s, MAX_INPUT - 512))
                    .unwrap_or(text)
            } else {
                text
            }),
            ..Content::default()
        },
        nodes: 0,
        attachments: 0,
        clipped: false,
    };
    state.visit(data, 0);
    if state.clipped {
        let marker =
            "{\"kind\":\"limit\",\"text\":\"QQ material was truncated by device limits.\"}\n";
        if state.output.reference.len() + marker.len()
            <= MAX_REFERENCE_BYTES.min(MAX_INPUT.saturating_sub(state.output.text.len()))
        {
            state.output.reference.push_str(marker);
        }
    }
    if state.output.reference.is_empty()
        && (data.get("ark_data").is_some_and(|v| !v.is_null())
            || data["attachments"]
                .as_array()
                .is_some_and(|v| !v.is_empty())
            || data["msg_elements"]
                .as_array()
                .is_some_and(|v| !v.is_empty())
            || data["asr_refer_text"]
                .as_str()
                .is_some_and(|s| !s.is_empty()))
    {
        state.output.reference = if state.output.images.is_empty() {
            "{\"kind\":\"supplied_qq_material\",\"omitted\":true}"
        } else {
            "{\"kind\":\"qq_images\",\"urls_are_transient\":true}"
        }
        .into();
    }
    if state.output.image_error.is_some() {
        state.output.images.clear();
    }
    if state.output.text.is_empty() && !state.output.has_material() {
        return None;
    }
    Some(state.output)
}
struct State {
    output: Content,
    nodes: usize,
    attachments: usize,
    clipped: bool,
}
impl State {
    fn note(&mut self, kind: &str, text: &str) {
        // Bound before allocating/escaping; no arbitrary object serialization.
        let truncated = text.len() > MAX_CARD_FIELD;
        self.clipped |= truncated;
        let line = json!({"kind":kind,"text":clip(text, MAX_CARD_FIELD),"truncated":truncated})
            .to_string();
        let limit = MAX_REFERENCE_BYTES
            .min(MAX_INPUT.saturating_sub(self.output.text.len()))
            .saturating_sub(128);
        if self.output.reference.len() + line.len() < limit {
            self.output.reference.push_str(&line);
            self.output.reference.push('\n');
        } else {
            self.clipped = true;
        }
    }
    fn visit(&mut self, node: &Value, depth: usize) {
        if depth > MAX_DEPTH || self.nodes >= MAX_NODES {
            self.clipped = true;
            return;
        }
        self.nodes += 1;
        if depth > 0 {
            if let Some(text) = node["content"].as_str().filter(|s| !s.is_empty()) {
                self.note("supplied_message", &faces(clip(text, MAX_CARD_FIELD)));
            }
        }
        if let Some(asr) = node["asr_refer_text"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
        {
            self.note("voice_transcript", asr);
        }
        match node.get("attachments") {
            Some(Value::Array(items)) => {
                for item in items.iter().take(MAX_ATTACHMENTS) {
                    if self.attachments >= MAX_ATTACHMENTS {
                        self.clipped = true;
                        break;
                    }
                    self.attachments += 1;
                    self.attachment(
                        item,
                        node["asr_refer_text"]
                            .as_str()
                            .is_some_and(|s| !s.trim().is_empty()),
                    );
                }
                if items.len() > MAX_ATTACHMENTS {
                    self.clipped = true;
                }
            }
            None | Some(Value::Null) => {}
            _ => self.output.image_error = Some(ImageError::Metadata),
        }
        if let Some(card) = node.get("ark_data").filter(|v| !v.is_null()) {
            // Some payloads encode only this known card object as JSON text.
            if let Some(raw) = card.as_str().filter(|s| s.len() <= MAX_REFERENCE_BYTES) {
                let parsed = super::json_depth_allowed(raw.as_bytes(), 8)
                    .then(|| serde_json::from_str::<Value>(raw).ok())
                    .flatten();
                if let Some(parsed) = parsed {
                    self.card(&parsed);
                } else {
                    self.note(
                        "card_unavailable",
                        "The supplied card could not be decoded.",
                    );
                }
            } else {
                self.card(card);
            }
        }
        let kind = node["message_type"].as_u64().unwrap_or(0);
        if let Some(elements) = node["msg_elements"].as_array() {
            if elements.is_empty() && matches!(kind, 101..=103) {
                self.note(
                    "missing_body",
                    "QQ supplied no embedded message body. Ask for the original if needed.",
                );
            }
            for element in elements.iter().take(MAX_NODES) {
                self.visit(element, depth + 1);
            }
            if elements.len() > MAX_NODES {
                self.clipped = true;
            }
        } else if matches!(kind, 101..=103) {
            self.note("missing_body", "QQ supplied a quote/forward marker but no embedded message body. Ask for the original if needed.");
        }
        if node["message_scene"]["ext"].as_array().is_some_and(|ext| {
            ext.iter()
                .any(|v| v.as_str().is_some_and(|s| s.starts_with("ref_msg_idx=")))
        }) {
            // Read the key's presence only; never copy scene indices or auth tokens.
            self.note("quote_reference", "A QQ quote reference is present; use only supplied embedded text. If its body is absent, ask for the original.");
        }
        // Never copy message_scene.ext values, auth_token, author IDs or raw objects.
    }
    fn card(&mut self, card: &Value) {
        let kind = card["ark_type"].as_str().unwrap_or("unknown");
        self.note("card_type", kind);
        for field in ["title", "desc", "source", "jump_url"] {
            if let Some(value) = card["fields"][field].as_str() {
                self.note(field, value);
            }
        }
        // A link/mini-app/music cover is not the document. Only picture cards are images.
        if kind == "picture" {
            if let Some(url) = card["fields"]["preview"].as_str() {
                self.image(&json!({"url":url,"content_type":"image/jpeg"}));
            } else {
                self.output.image_error = Some(ImageError::Url);
            }
        }
    }
    fn attachment(&mut self, item: &Value, has_asr: bool) {
        let mime = item["content_type"].as_str().unwrap_or("");
        let mime = mime
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if mime.starts_with("image/") {
            self.image(item);
            return;
        }
        let asr = item["asr_refer_text"]
            .as_str()
            .filter(|s| !s.trim().is_empty());
        if let Some(asr) = asr {
            self.note("voice_transcript", asr);
        }
        let has_asr = has_asr || asr.is_some();
        let name = item["filename"]
            .as_str()
            .or_else(|| item["file_name"].as_str())
            .unwrap_or("unnamed");
        self.note("attachment_name", name);
        if mime != "voice" && !mime.starts_with("audio/") && !mime.starts_with("video/") {
            match super::file_attachment::FileAttachment::parse(item) {
                Ok(file) if self.output.files.len() < super::file_attachment::MAX_FILES => {
                    self.output.files.push(file);
                    self.note("text_file", "A small text file is attached. Only the separately supplied read result establishes its contents.");
                }
                Ok(_) => self.note(
                    "unread_file",
                    "Only two small text files can be read in one request.",
                ),
                Err(reason) => self.note("unread_file", reason),
            }
            return;
        }
        match mime.as_str() {
            "voice" | "audio/silk" | "audio/amr" | "audio/mpeg" | "audio/wav" => {
                if !has_asr { self.note("unsupported_voice", "QQ did not supply a transcript. This bot cannot transcribe audio; ask for text."); }
            }
            "video/mp4" => self.note("unsupported_video", "Video was received, but its contents have not been watched. Ask for key frames or a transcript."),
            _ => self.note("unsupported_attachment", "An unsupported attachment was received. Its contents have not been inspected."),
        }
    }
    fn image(&mut self, item: &Value) {
        if self.output.images.len() >= MAX_IMAGES {
            self.output.image_error = Some(ImageError::TooMany);
            return;
        }
        let result = (|| {
            let content_type = image_content_type(
                item["content_type"]
                    .as_str()
                    .ok_or(ImageError::UnsupportedFormat)?,
            )?;
            let raw = item["url"].as_str().ok_or(ImageError::Url)?;
            let url = if raw.starts_with("//") {
                format!("https:{raw}")
            } else {
                raw.to_owned()
            };
            for field in ["width", "height"] {
                if metadata_number(item, field)?.is_some_and(|side| side > MAX_IMAGE_DIMENSION) {
                    return Err(ImageError::Dimensions);
                }
            }
            let image = ImageAttachment {
                url,
                content_type: content_type.into(),
                size: metadata_number(item, "size")?,
            };
            image.validate()?;
            Ok(image)
        })();
        match result {
            Ok(image) => self.output.images.push(image),
            Err(error) => self.output.image_error = Some(error),
        }
    }
}
fn outer_asr(data: &Value) -> Option<&str> {
    data["asr_refer_text"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            data["attachments"]
                .as_array()?
                .iter()
                .take(MAX_ATTACHMENTS)
                .find_map(|a| {
                    a["asr_refer_text"]
                        .as_str()
                        .filter(|s| !s.trim().is_empty())
                })
        })
}
fn clip(s: &str, max: usize) -> &str {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Preserve unknown tags verbatim and never parse ordinary HTML/code as QQ markup.
fn faces(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        if let Some(tail) = rest.strip_prefix("[<face,id=") {
            if let Some(end) = tail.find(">]").filter(|n| *n <= 16) {
                let id = tail[..end].trim_end_matches('/');
                if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) {
                    out.push_str("[QQ face ");
                    out.push_str(id);
                    out.push(']');
                    rest = &tail[end + 2..];
                    continue;
                }
            }
        }
        if rest.starts_with("<faceType=") {
            if let Some(end) = rest.find('>').filter(|n| *n <= 512) {
                let tag = &rest[..end];
                if let Some(id) = tag
                    .split("faceId=\"")
                    .nth(1)
                    .and_then(|s| s.split('"').next())
                    .filter(|s| {
                        !s.is_empty()
                            && s.len() <= 32
                            && s.bytes().all(|b| b.is_ascii_alphanumeric())
                    })
                {
                    out.push_str("[QQ face ");
                    out.push_str(id);
                    if let Some(label) = face_label(tag) {
                        out.push_str(": ");
                        out.push_str(&label);
                    }
                    out.push(']');
                    rest = &rest[end + 1..];
                    continue;
                }
            }
        }
        let c = rest.chars().next().unwrap();
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// Decode only the tiny documented face ext; never media payloads or arbitrary HTML.
fn face_label(tag: &str) -> Option<String> {
    let encoded = tag.split("ext=\"").nth(1)?.split('"').next()?;
    if encoded.is_empty() || encoded.len() > 384 || encoded.len() % 4 != 0 {
        return None;
    }
    let mut decoded = Vec::with_capacity(encoded.len() / 4 * 3);
    for (index, chunk) in encoded.as_bytes().chunks_exact(4).enumerate() {
        let mut bits = 0u32;
        let mut pad = 0;
        for &b in chunk {
            let value = match b {
                b'A'..=b'Z' => b - b'A',
                b'a'..=b'z' => b - b'a' + 26,
                b'0'..=b'9' => b - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                b'=' => {
                    pad += 1;
                    0
                }
                _ => return None,
            };
            if pad > 0 && b != b'=' {
                return None;
            }
            bits = (bits << 6) | u32::from(value);
        }
        if pad > 2 || (pad > 0 && index + 1 != encoded.len() / 4) {
            return None;
        }
        decoded.push((bits >> 16) as u8);
        if pad < 2 {
            decoded.push((bits >> 8) as u8);
        }
        if pad == 0 {
            decoded.push(bits as u8);
        }
    }
    if !super::json_depth_allowed(&decoded, 4) {
        return None;
    }
    let data: Value = serde_json::from_slice(&decoded).ok()?;
    let text = data["text"].as_str()?.trim();
    if text.is_empty() || text.chars().any(char::is_control) {
        return None;
    }
    Some(clip(text, 96).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn image() -> Value {
        json!({"url":"https://gchat.qpic.cn/i?rkey=secret","content_type":"image/png"})
    }
    #[test]
    fn quotes_never_become_live_commands_or_auth() {
        let c = normalize(&json!({"message_type":103,"msg_elements":[{"content":"/reset","author":{"user_openid":"admin"}}],"message_scene":{"ext":["auth_token=secret"]}}), "Explain this").unwrap();
        assert_eq!(c.text, "Explain this");
        assert!(c.reference.contains("/reset"));
        assert!(!c.reference.contains("auth_token"));
        assert!(c.images.is_empty());
    }
    #[test]
    fn scene_only_quote_markers_do_not_become_commands_or_leak_tokens() {
        let c = normalize(
            &json!({"message_scene":{"ext":["msg_idx=1","ref_msg_idx=2","auth_token=secret"]}}),
            "/reset",
        )
        .unwrap();
        assert!(c.reference.contains("quote_reference"));
        assert!(!c.reference.contains("secret") && !c.reference.contains("ref_msg_idx=2"));
        let c = normalize(
            &json!({"message_scene":{"ext":["msg_idx=1","auth_token=secret"]}}),
            "/ping",
        )
        .unwrap();
        assert!(c.reference.is_empty());
    }

    #[test]
    fn nested_images_and_two_image_limit() {
        let c = normalize(
            &json!({"attachments":[image()],"msg_elements":[{"attachments":[image()]}]}),
            "compare",
        )
        .unwrap();
        assert_eq!(c.images.len(), 2);
        assert!(c.image_error.is_none());
        assert!(!c.reference.contains("secret"));
        let c = normalize(&json!({"attachments":[image(),image(),image()]}), "").unwrap();
        assert_eq!(c.image_error, Some(ImageError::TooMany));
        assert!(c.images.is_empty());
    }
    #[test]
    fn only_picture_preview_is_vision() {
        for kind in ["miniapp", "tuwen", "music_together", "map"] {
            let c=normalize(&json!({"ark_data":{"ark_type":kind,"fields":{"title":"Example","preview":"https://gchat.qpic.cn/i","jump_url":"https://example.com"}}}), "").unwrap();
            assert!(c.images.is_empty());
            assert!(c.reference.contains("Example"));
        }
        let c=normalize(&json!({"ark_data":"{\"ark_type\":\"picture\",\"fields\":{\"preview\":\"https://gchat.qpic.cn/i\"}}"}), "").unwrap();
        assert_eq!(c.images.len(), 1);
    }
    #[test]
    fn voice_asr_and_files_are_not_image_errors() {
        let c = normalize(
            &json!({"asr_refer_text":"你好","attachments":[{"content_type":"voice"}]}),
            "",
        )
        .unwrap();
        assert!(c.reference.contains("你好"));
        assert!(!c.reference.contains("cannot transcribe"));
        assert!(c.image_error.is_none());
        let c = normalize(
            &json!({"attachments":[{"content_type":"voice","asr_refer_text":"用中文回答"}]}),
            "",
        )
        .unwrap();
        assert_eq!(c.text, "用中文回答");
        assert!(!c.reference.contains("cannot transcribe"));
        for mime in ["voice", "file", "application/pdf", "video/mp4"] {
            let c = normalize(
                &json!({"attachments":[{"content_type":mime,"filename":"test"}]}),
                "",
            )
            .unwrap();
            assert!(c.image_error.is_none());
            assert!(!c.reference.is_empty());
        }
    }

    #[test]
    fn small_text_files_are_queued_without_leaking_download_urls_to_material() {
        let c=normalize(&json!({"attachments":[{"content_type":"application/vnd.ms-excel","filename":"notes.csv","url":"https://gzc-download.ftn.qq.com/notes?sig=hidden","size":120}]}),"Please summarize").unwrap();
        assert_eq!(c.files.len(), 1);
        assert!(!c.reference.contains("sig=hidden"));
        assert!(c.images.is_empty() && c.image_error.is_none());
        let c=normalize(&json!({"attachments":[{"content_type":"file","filename":"notes.pdf","url":"https://gzc-download.ftn.qq.com/a"}]}),"").unwrap();
        assert!(c.files.is_empty());
        assert!(c.reference.contains("unread_file"));
    }
    #[test]
    fn marker_without_body_is_explicit() {
        assert!(normalize(&json!({"message_type":103}), "")
            .unwrap()
            .reference
            .contains("no embedded message body"));
    }
    #[test]
    fn recursion_nodes_and_unicode_reference_are_bounded() {
        let mut node = json!({"content":"secret at excessive depth"});
        for _ in 0..10 {
            node = json!({"msg_elements":[node]});
        }
        let c = normalize(&node, "test").unwrap();
        assert!(!c.reference.contains("secret at excessive depth"));
        let c=normalize(&json!({"msg_elements":(0..100).map(|_| json!({"content":"你好".repeat(1000)})).collect::<Vec<_>>()}), "test").unwrap();
        assert!(c.reference.len() <= MAX_REFERENCE_BYTES);
        assert!(c.text.len() + c.reference.len() <= MAX_INPUT);
    }
    #[test]
    fn face_tags_preserve_unicode_unknown_tags_and_code() {
        assert_eq!(
            faces("<faceType=1,faceId=\"14\",ext=\"eyJ0ZXh0Ijoi5b6u56yRIn0=\">"),
            "[QQ face 14: 微笑]"
        );
        assert_eq!(
            faces("好[<face,id=14/>]🙂<faceType=1,faceId=\"123\",ext=\"abc\">"),
            "好[QQ face 14]🙂[QQ face 123]"
        );
        assert_eq!(
            faces("<div>[<face,id=x/>]</div>"),
            "<div>[<face,id=x/>]</div>"
        );
    }
}
