//! Pure serialization adapter for transient QQ image references.
//! This module does no network I/O, file access, downloads or persistence.
use super::protocol::{ChatMessage, ImageAttachment, ImageError, MAX_INPUT};
use serde_json::json;

const MAX_USER_MESSAGE_BYTES: usize = 40 * 1024;

/// Build an ordinary text message or a mixed user message for DeepSeek Flash.
/// Pure image input is legal; callers may add their own neutral description prompt.
pub fn user_message(text: String, images: &[ImageAttachment]) -> Result<ChatMessage, ImageError> {
    if text.len() > MAX_INPUT || (text.trim().is_empty() && images.is_empty()) {
        return Err(ImageError::PromptLimit);
    }
    if images.len() > super::protocol::message_content::MAX_IMAGES {
        return Err(ImageError::TooMany);
    }
    let content = if !images.is_empty() {
        let mut parts = Vec::with_capacity(1 + images.len());
        if !text.trim().is_empty() {
            parts.push(json!({"type":"text","text":text}));
        }
        for image in images {
            image.validate()?;
            parts.push(json!({
                "type":"image_url",
                "image_url":{"url":image.url,"detail":"auto"}
            }));
        }
        parts.into()
    } else {
        text.into()
    };
    let message = ChatMessage {
        role: "user".into(),
        content,
    };
    if serde_json::to_vec(&message)
        .map_err(|_| ImageError::PromptLimit)?
        .len()
        > MAX_USER_MESSAGE_BYTES
    {
        return Err(ImageError::PromptLimit);
    }
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> ImageAttachment {
        ImageAttachment {
            url: "https://gchat.qpic.cn/test.png?signature=fixture".into(),
            content_type: "image/png".into(),
            size: Some(1024),
        }
    }

    #[test]
    fn ordinary_text_remains_a_json_string() {
        let message = user_message("你好".into(), &[]).unwrap();
        let value = serde_json::to_value(message).unwrap();
        assert_eq!(value, json!({"role":"user","content":"你好"}));
    }

    #[test]
    fn mixed_user_message_matches_deepseek_image_schema_without_downloading() {
        let image = fixture();
        let message = user_message("识别图片文字".into(), std::slice::from_ref(&image)).unwrap();
        let value = serde_json::to_value(message).unwrap();
        assert_eq!(
            value,
            json!({
                "role":"user","content":[
                    {"type":"text","text":"识别图片文字"},
                    {"type":"image_url","image_url":{"url":image.url,"detail":"auto"}}
                ]
            })
        );
        assert!(!value.to_string().contains("base64"));
        assert!(value.get("file_id").is_none());
    }

    #[test]
    fn image_only_input_is_preserved_without_fabricating_a_caption() {
        let message = user_message(String::new(), &[fixture()]).unwrap();
        assert_eq!(message.content.as_array().unwrap().len(), 1);
        assert_eq!(message.content[0]["type"], "image_url");
        assert!(user_message(String::new(), &[]).is_err());
    }

    #[test]
    fn manually_constructed_unsafe_images_and_oversized_json_are_rejected() {
        let mut image = fixture();
        image.url = "https://127.0.0.1/private".into();
        assert!(user_message("read".into(), std::slice::from_ref(&image)).is_err());
        image = fixture();
        image.content_type = "image/svg+xml".into();
        assert!(user_message("read".into(), std::slice::from_ref(&image)).is_err());
        assert!(user_message("x".repeat(MAX_INPUT + 1), &[fixture()]).is_err());
        assert!(user_message("\0".repeat(MAX_INPUT), &[fixture()]).is_err());
    }
}
