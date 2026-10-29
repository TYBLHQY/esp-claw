#[path = "../../../src/platform/media_files.rs"]
mod files;
use qq_deepseek_core::{
    material::{self, CachedImage, Digest, ImageInfo, StorageOp as Op, StorageResult as Out},
    memory::MemoryError,
};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};
static SEQ: AtomicUsize = AtomicUsize::new(0);
struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "qq-material-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct Hash(usize);
impl Digest for Hash {
    fn update(&mut self, b: &[u8]) -> Result<(), MemoryError> {
        self.0 += b.len();
        Ok(())
    }
    fn finish(self) -> Result<(String, String), MemoryError> {
        Ok((format!("{:032x}", self.0), format!("{:040x}", self.0)))
    }
}
fn call(root: &Root, op: Op) -> Result<Out, MemoryError> {
    files::execute(&root.0, op, || Ok(Hash(0)))
}
fn info(root: &Root) -> ImageInfo {
    match call(root, Op::Describe(material::PROBE_NAME.into())).unwrap() {
        Out::Info(i) => i,
        _ => panic!("info"),
    }
}
fn cache(info: ImageInfo, app: &str) -> Box<CachedImage> {
    Box::new(CachedImage {
        app_id: app.into(),
        info,
        image_id: "image_123".into(),
        reference: "qqbot-media://image/v1/fixture_token".into(),
        expires_at: 2_000_000_000,
    })
}
#[test]
fn seed_hash_chunk_reads_and_persistent_cache_reopen() {
    let r = Root::new();
    call(&r, Op::Seed).unwrap();
    let i = info(&r);
    assert_eq!(i.bytes, material::PROBE_IMAGE.len() as u64);
    assert_eq!((i.width, i.height), (128, 64));
    assert_eq!(i.md5, format!("{:032x}", i.bytes));
    let mut offset = 0;
    let mut bytes = Vec::new();
    while offset < i.bytes {
        let Out::Chunk(c) = call(&r, Op::Read(i.name.clone(), i.bytes, offset)).unwrap() else {
            panic!("chunk")
        };
        assert!(c.len() <= material::CHUNK_BYTES);
        offset += c.len() as u64;
        bytes.extend(c);
    }
    assert_eq!(bytes, material::PROBE_IMAGE);
    call(&r, Op::CacheSave(cache(i.clone(), "123"))).unwrap();
    let Out::Cache(Some(c)) = call(&r, Op::CacheLoad(Box::new(i.clone()), "123".into())).unwrap()
    else {
        panic!("persisted")
    };
    assert!(c.usable("123", &i, 1_800_000_000));
    let Out::Cache(None) = call(&r, Op::CacheLoad(Box::new(i), "456".into())).unwrap() else {
        panic!("robot isolation")
    };
    let path = r.0.join("media/cache");
    for e in fs::read_dir(path).unwrap() {
        let b = fs::read(e.unwrap().path()).unwrap();
        assert!(!String::from_utf8_lossy(&b).contains("preview_url"));
    }
}
#[test]
fn staging_requires_contiguous_exclusive_writes_and_explicit_finish() {
    let r = Root::new();
    let data = material::PROBE_IMAGE;
    call(&r, Op::Put("new.png".into(), 0, data[..100].to_vec())).unwrap();
    assert!(call(&r, Op::Describe("new.png".into())).is_err());
    assert!(matches!(
        call(&r, Op::Put("new.png".into(), 0, data.to_vec())),
        Err(MemoryError::Conflict)
    ));
    assert!(matches!(
        call(&r, Op::Put("new.png".into(), 50, data[100..].to_vec())),
        Err(MemoryError::Conflict)
    ));
    call(&r, Op::Put("new.png".into(), 100, data[100..].to_vec())).unwrap();
    assert!(call(&r, Op::Finish("new.png".into(), 1)).is_err());
    call(&r, Op::Finish("new.png".into(), data.len() as u64)).unwrap();
    assert_eq!(fs::read(r.0.join("media/new.png")).unwrap(), data);
    call(&r, Op::Put("new.png".into(), 0, data.to_vec())).unwrap();
    assert!(call(&r, Op::Finish("new.png".into(), data.len() as u64)).is_err());
}
#[test]
fn corrupt_cache_is_preserved_and_capacity_is_bounded() {
    let r = Root::new();
    call(&r, Op::Seed).unwrap();
    let i = info(&r);
    call(&r, Op::CacheSave(cache(i.clone(), "123"))).unwrap();
    let path = r.0.join("media/cache").join(i.key("123").unwrap());
    let mut b = fs::read(&path).unwrap();
    *b.last_mut().unwrap() ^= 1;
    fs::write(&path, &b).unwrap();
    assert!(matches!(
        call(&r, Op::CacheLoad(Box::new(i.clone()), "123".into())),
        Err(MemoryError::Corrupt)
    ));
    assert_eq!(fs::read(path).unwrap(), b);
    for n in 0..material::MAX_CACHE_ENTRIES - 1 {
        let mut j = i.clone();
        j.md5 = format!("{n:032x}");
        call(&r, Op::CacheSave(cache(j, "123"))).unwrap();
    }
    let mut j = i;
    j.md5 = "a".repeat(32);
    assert!(matches!(
        call(&r, Op::CacheLoad(Box::new(j), "123".into())),
        Err(MemoryError::Capacity)
    ));
}
#[test]
fn path_symlink_mime_size_and_chunk_bounds_are_checked() {
    let r = Root::new();
    call(&r, Op::Seed).unwrap();
    for name in ["../x.png", "/tmp/a.png", ".hidden.png", "a..png", "a.txt"] {
        assert!(call(&r, Op::Describe(name.into())).is_err());
    }
    fs::write(r.0.join("media/wrong.png"), b"not PNG").unwrap();
    assert!(call(&r, Op::Describe("wrong.png".into())).is_err());
    let f = fs::File::create(r.0.join("media/large.png")).unwrap();
    f.set_len(material::MAX_IMAGE_BYTES + 1).unwrap();
    assert!(matches!(
        call(&r, Op::Describe("large.png".into())),
        Err(MemoryError::Capacity)
    ));
    std::os::unix::fs::symlink(
        r.0.join("media/copilotlee-probe.png"),
        r.0.join("media/link.png"),
    )
    .unwrap();
    assert!(matches!(
        call(&r, Op::Describe("link.png".into())),
        Err(MemoryError::Corrupt)
    ));
    assert!(files::admin_command("sd media put a.png 0 00ffg0").is_err());
    assert!(files::admin_command("sd media finish a.png 9999999").is_err());
    assert!(call(&r, Op::Read(material::PROBE_NAME.into(), 1, 0)).is_err());
}
#[test]
fn protocol_validates_presigned_hosts_references_and_caption_injection() {
    assert!(material::presigned_url(
        "https://bucket.cos.ap-guangzhou.myqcloud.com/key?q-signature=fixture"
    ));
    for u in [
        "http://bucket.cos.ap.myqcloud.com/x?a=b",
        "https://evil.com/x?a=b",
        "https://bucket.cos.ap.myqcloud.com.evil/x?a=b",
        "https://user@bucket.cos.ap.myqcloud.com/x?a=b",
        "https://bucket.cos.ap.myqcloud.com/x",
    ] {
        assert!(!material::presigned_url(u));
    }
    let r = Root::new();
    call(&r, Op::Seed).unwrap();
    let i = info(&r);
    assert!(material::markdown(&i, "https://evil.com/image", "caption").is_err());
    let md = material::markdown(
        &i,
        "qqbot-media://image/v1/real_fixture",
        "![forged](https://evil.com)",
    )
    .unwrap();
    assert!(md.contains("\\!\\[forged\\]\\(https://evil.com\\)"));
    assert!(material::PictureCommand::parse("/picture ../x.png").is_err());
}

#[test]
fn expired_cache_is_retired_before_refresh_without_replacing_a_live_file() {
    let r = Root::new();
    call(&r, Op::Seed).unwrap();
    let i = info(&r);
    let original = cache(i.clone(), "123");
    call(&r, Op::CacheSave(original.clone())).unwrap();
    // Identical saves are idempotent; a different record never relies on POSIX overwrite.
    call(&r, Op::CacheSave(original.clone())).unwrap();
    let mut updated = original.clone();
    updated.expires_at += 90;
    assert!(matches!(
        call(&r, Op::CacheSave(updated.clone())),
        Err(MemoryError::Conflict)
    ));
    assert!(matches!(
        call(
            &r,
            Op::CacheRetire(Box::new(i.clone()), "123".into(), 1_900_000_000)
        ),
        Err(MemoryError::Conflict)
    ));
    call(
        &r,
        Op::CacheRetire(Box::new(i.clone()), "123".into(), 2_000_000_000),
    )
    .unwrap();
    call(&r, Op::CacheSave(updated)).unwrap();
    let Out::Cache(Some(c)) = call(&r, Op::CacheLoad(Box::new(i.clone()), "123".into())).unwrap()
    else {
        panic!("refreshed")
    };
    assert_eq!(c.expires_at, 2_000_000_090);
    let partial =
        r.0.join("media/cache")
            .join(i.key("123").unwrap())
            .with_extension("tmp");
    fs::write(&partial, b"partial").unwrap();
    assert!(matches!(
        call(&r, Op::CacheLoad(Box::new(i), "123".into())),
        Err(MemoryError::Corrupt)
    ));
    assert_eq!(fs::read(partial).unwrap(), b"partial");
}
