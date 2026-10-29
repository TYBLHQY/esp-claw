//! Transient text-file references and strict QQ CDN checks; never a general URL fetcher.
use super::{metadata_number, validate_image_url, ImageError};
use serde_json::Value;

pub const MAX_FILE_BYTES: usize = 16 * 1024;
pub const MAX_FILES: usize = 2;
pub const FILE_CONTEXT_BYTES: usize = 3072;
#[derive(Clone)]
pub struct FileAttachment {
    pub url: String,
    pub name: String,
    pub size: Option<u64>,
}
impl FileAttachment {
    pub fn parse(item: &Value) -> Result<Self, &'static str> {
        let name = item["filename"]
            .as_str()
            .or_else(|| item["file_name"].as_str())
            .ok_or("missing_filename")?;
        if name.is_empty()
            || name.len() > 128
            || name.chars().any(char::is_control)
            || name.contains(['/', '\\'])
        {
            return Err("invalid_filename");
        }
        let ext = name
            .rsplit_once('.')
            .map(|(_, s)| s.to_ascii_lowercase())
            .ok_or("unsupported_file_type")?;
        if !matches!(
            ext.as_str(),
            "txt"
                | "md"
                | "csv"
                | "json"
                | "log"
                | "py"
                | "rs"
                | "c"
                | "h"
                | "cpp"
                | "toml"
                | "yaml"
                | "yml"
                | "ini"
                | "js"
                | "ts"
                | "html"
                | "css"
                | "sql"
                | "xml"
                | "sh"
        ) {
            return Err("unsupported_file_type");
        }
        let size = metadata_number(item, "size").map_err(|_| "invalid_file_size")?;
        if size.is_some_and(|n| n > MAX_FILE_BYTES as u64) {
            return Err("file_exceeds_16kib");
        }
        let raw = item["url"].as_str().ok_or("missing_file_url")?;
        let url = if raw.starts_with("//") {
            format!("https:{raw}")
        } else {
            raw.to_owned()
        };
        validate_cdn(&url).map_err(|_| "untrusted_file_origin")?;
        Ok(Self {
            url,
            name: name.into(),
            size,
        })
    }
    pub fn validate(&self) -> Result<(), &'static str> {
        Self::parse(&serde_json::json!({"url":self.url,"filename":self.name,"size":self.size}))
            .map(|_| ())
    }
}
pub fn validate_cdn(url: &str) -> Result<(), ImageError> {
    validate_image_url(url)?;
    let authority = url[8..]
        .split(['/', '?', '#'])
        .next()
        .ok_or(ImageError::Url)?;
    let host = authority
        .strip_suffix(":443")
        .unwrap_or(authority)
        .to_ascii_lowercase();
    if ["qq.com", "qq.com.cn", "qpic.cn"].iter().any(|suffix| {
        host == *suffix || host.strip_suffix(suffix).is_some_and(|s| s.ends_with('.'))
    }) {
        Ok(())
    } else {
        Err(ImageError::Url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn text_files_require_bounded_names_sizes_and_real_qq_cdn_origins() {
        let item = json!({"filename":"notes.txt","url":"https://gzc-download.ftn.qq.com/a?token=transient","size":100});
        assert!(FileAttachment::parse(&item).is_ok());
        for (key, val) in [
            ("filename", json!("report.pdf")),
            ("filename", json!("../notes.txt")),
            ("size", json!(MAX_FILE_BYTES + 1)),
            ("url", json!("https://qq.com.evil.example/x")),
            ("url", json!("https://example.com/x")),
            ("url", json!("http://gzc-download.ftn.qq.com/x")),
            ("url", json!("https://127.0.0.1/x")),
        ] {
            let mut bad = item.clone();
            bad[key] = val;
            assert!(FileAttachment::parse(&bad).is_err());
        }
        for url in [
            "https://multimedia.nt.qq.com.cn/a",
            "https://gchat.qpic.cn/a",
            "https://c2cpicdw.qpic.cn/a",
        ] {
            assert!(validate_cdn(url).is_ok());
        }
    }
}
