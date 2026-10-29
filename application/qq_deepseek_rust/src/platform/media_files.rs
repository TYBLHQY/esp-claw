//! All image/cache filesystem operations execute exclusively on the SD owner.
use qq_deepseek_core::material::{StorageOp as Operation, StorageResult as Response};
use qq_deepseek_core::{
    material::{self, CachedImage, Digest, ImageInfo},
    memory::MemoryError,
};
use serde_json::json;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
fn io(stage: &'static str, e: std::io::Error) -> MemoryError {
    MemoryError::Io {
        stage,
        code: e.raw_os_error().unwrap_or(-3),
    }
}
fn dir(path: &Path, create: bool) -> Result<bool, MemoryError> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() && !m.file_type().is_symlink() => Ok(true),
        Ok(_) => Err(MemoryError::Corrupt),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if create {
                fs::create_dir(path).map_err(|e| io("media_directory_create", e))?;
                Ok(true)
            } else {
                Ok(false)
            }
        }
        Err(e) => Err(io("media_directory_stat", e)),
    }
}
fn folder(root: &Path, create: bool) -> Result<PathBuf, MemoryError> {
    if !dir(root, create)? {
        return Err(MemoryError::Unavailable);
    }
    let p = root.join("media");
    if !dir(&p, create)? {
        return Err(MemoryError::Unavailable);
    }
    Ok(p)
}
fn image_path(root: &Path, name: &str) -> Result<PathBuf, MemoryError> {
    if material::mime(name).is_none() {
        return Err(MemoryError::Invalid);
    }
    Ok(folder(root, false)?.join(name))
}
fn file_size(path: &Path) -> Result<u64, MemoryError> {
    let m = fs::symlink_metadata(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            MemoryError::Invalid
        } else {
            io("media_file_stat", e)
        }
    })?;
    if !m.is_file() || m.file_type().is_symlink() {
        return Err(MemoryError::Corrupt);
    }
    if m.len() == 0 || m.len() > material::MAX_IMAGE_BYTES {
        return Err(MemoryError::Capacity);
    }
    Ok(m.len())
}
fn cache_dir(root: &Path) -> Result<PathBuf, MemoryError> {
    let p = folder(root, true)?.join("cache");
    dir(&p, true)?;
    Ok(p)
}
fn cache_load(
    root: &Path,
    info: &ImageInfo,
    app: &str,
) -> Result<Option<Box<CachedImage>>, MemoryError> {
    let p = cache_dir(root)?.join(info.key(app)?);
    match fs::symlink_metadata(p.with_extension("tmp")) {
        Ok(_) => return Err(MemoryError::Corrupt),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(io("media_cache_partial_stat", e)),
    }
    match fs::symlink_metadata(&p) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Ok(m)
            if m.is_file()
                && !m.file_type().is_symlink()
                && m.len() <= material::MAX_CACHE_BYTES as u64 + 12 => {}
        Ok(_) => return Err(MemoryError::Corrupt),
        Err(e) => return Err(io("media_cache_stat", e)),
    }
    let mut bytes = Vec::new();
    File::open(&p)
        .map_err(|e| io("media_cache_open", e))?
        .take(material::MAX_CACHE_BYTES as u64 + 13)
        .read_to_end(&mut bytes)
        .map_err(|e| io("media_cache_read", e))?;
    let r = material::decode(&bytes)?;
    if r.app_id != app || r.info.md5 != info.md5 || r.info.sha1 != info.sha1 {
        return Err(MemoryError::Conflict);
    }
    Ok(Some(r))
}
fn capacity(dir: &Path, new: bool) -> Result<(), MemoryError> {
    let mut n = 0;
    for (i, e) in fs::read_dir(dir)
        .map_err(|e| io("media_cache_list", e))?
        .enumerate()
    {
        if i >= material::MAX_CACHE_ENTRIES * 3 {
            return Err(MemoryError::Capacity);
        }
        let e = e.map_err(|e| io("media_cache_entry", e))?;
        if e.file_name().to_string_lossy().ends_with(".qmi") {
            n += 1;
        }
    }
    if new && n >= material::MAX_CACHE_ENTRIES {
        Err(MemoryError::Capacity)
    } else {
        Ok(())
    }
}
pub fn execute<D: Digest>(
    root: &Path,
    op: Operation,
    new_digest: impl FnOnce() -> Result<D, MemoryError>,
) -> Result<Response, MemoryError> {
    match op {
        Operation::Describe(name) => {
            let p = image_path(root, &name)?;
            let bytes = file_size(&p)?;
            let mut f = File::open(&p).map_err(|e| io("media_image_open", e))?;
            let mime = material::mime(&name).ok_or(MemoryError::Invalid)?;
            let mut buffer = vec![0; material::CHUNK_BYTES];
            let mut total = 0;
            let mut dims = None;
            let mut digest = new_digest()?;
            let start = Instant::now();
            loop {
                if start.elapsed() > Duration::from_secs(2) {
                    return Err(MemoryError::Timeout);
                }
                let n = f.read(&mut buffer).map_err(|e| io("media_hash_read", e))?;
                if n == 0 {
                    break;
                }
                if total == 0 {
                    dims = material::image_header(mime, &buffer[..n]);
                }
                total += n as u64;
                if total > bytes {
                    return Err(MemoryError::Conflict);
                }
                digest.update(&buffer[..n])?;
            }
            if total != bytes {
                return Err(MemoryError::Conflict);
            }
            let (width, height) = dims.ok_or(MemoryError::Invalid)?;
            let (md5, sha1) = digest.finish()?;
            Ok(Response::Info(ImageInfo {
                name,
                bytes,
                content_type: mime.into(),
                md5,
                sha1,
                width,
                height,
            }))
        }
        Operation::Read(name, bytes, offset) => {
            let p = image_path(root, &name)?;
            if file_size(&p)? != bytes || offset >= bytes {
                return Err(MemoryError::Conflict);
            }
            let mut f = File::open(p).map_err(|e| io("media_chunk_open", e))?;
            f.seek(SeekFrom::Start(offset))
                .map_err(|e| io("media_chunk_seek", e))?;
            let n = (bytes - offset).min(material::CHUNK_BYTES as u64) as usize;
            let mut b = vec![0; n];
            f.read_exact(&mut b)
                .map_err(|e| io("media_chunk_read", e))?;
            Ok(Response::Chunk(b))
        }
        Operation::CacheLoad(info, app) => {
            let found = cache_load(root, &info, &app)?;
            capacity(&cache_dir(root)?, found.is_none())?;
            Ok(Response::Cache(found))
        }
        Operation::CacheRetire(info, app, cutoff) => {
            if let Some(previous) = cache_load(root, &info, &app)? {
                if previous.expires_at > cutoff {
                    return Err(MemoryError::Conflict);
                }
                // Only verified, unusable metadata is retired. User images and platform assets remain.
                let path = cache_dir(root)?.join(info.key(&app)?);
                fs::remove_file(path).map_err(|e| io("media_cache_retire", e))?;
            }
            Ok(Response::Done)
        }
        Operation::CacheSave(r) => {
            let bytes = material::encode(&r)?;
            let dir = cache_dir(root)?;
            let p = dir.join(r.info.key(&r.app_id)?);
            let previous = cache_load(root, &r.info, &r.app_id)?;
            if let Some(previous) = previous {
                if material::encode(&previous)? == bytes {
                    return Ok(Response::Done);
                }
                return Err(MemoryError::Conflict);
            }
            capacity(&dir, true)?;
            let tmp = p.with_extension("tmp");
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
                .map_err(|e| {
                    if e.kind() == std::io::ErrorKind::AlreadyExists {
                        MemoryError::Corrupt
                    } else {
                        io("media_cache_create", e)
                    }
                })?;
            f.write_all(&bytes)
                .and_then(|_| f.sync_all())
                .map_err(|e| io("media_cache_sync", e))?;
            drop(f);
            let readback = fs::read(&tmp).map_err(|e| io("media_cache_verify", e))?;
            if readback != bytes {
                return Err(MemoryError::Corrupt);
            }
            material::decode(&readback)?;
            fs::rename(tmp, &p).map_err(|e| io("media_cache_commit", e))?;
            if cache_load(root, &r.info, &r.app_id)?.is_none() {
                return Err(MemoryError::Corrupt);
            }
            Ok(Response::Done)
        }
        Operation::Seed => {
            let p = folder(root, true)?.join(material::PROBE_NAME);
            if p.exists() {
                if file_size(&p)? == material::PROBE_IMAGE.len() as u64
                    && fs::read(&p).map_err(|e| io("media_seed_verify", e))?
                        == material::PROBE_IMAGE
                {
                    return Ok(Response::Done);
                }
                return Err(MemoryError::Conflict);
            }
            let mut f = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(p)
                .map_err(|e| io("media_seed_create", e))?;
            f.write_all(material::PROBE_IMAGE)
                .and_then(|_| f.sync_all())
                .map_err(|e| io("media_seed_sync", e))?;
            Ok(Response::Done)
        }
        Operation::Put(name, offset, bytes) => {
            if material::mime(&name).is_none()
                || bytes.is_empty()
                || bytes.len() > material::CHUNK_BYTES
                || offset + bytes.len() as u64 > material::MAX_IMAGE_BYTES
            {
                return Err(MemoryError::Invalid);
            }
            let dir = folder(root, true)?;
            let p = dir.join(format!("{name}.part"));
            let mut opt = OpenOptions::new();
            opt.write(true);
            if offset == 0 {
                opt.create_new(true);
            } else {
                if file_size(&p)? != offset {
                    return Err(MemoryError::Conflict);
                }
                opt.append(true);
            }
            let mut f = opt.open(p).map_err(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    MemoryError::Conflict
                } else {
                    io("media_stage_open", e)
                }
            })?;
            f.write_all(&bytes)
                .and_then(|_| f.sync_all())
                .map_err(|e| io("media_stage_write", e))?;
            Ok(Response::Done)
        }
        Operation::Finish(name, bytes) => {
            if material::mime(&name).is_none() {
                return Err(MemoryError::Invalid);
            }
            let dir = folder(root, false)?;
            let p = dir.join(format!("{name}.part"));
            let target = dir.join(&name);
            if file_size(&p)? != bytes || target.exists() {
                return Err(MemoryError::Conflict);
            }
            let mut b = vec![0; 64];
            let n = File::open(&p)
                .map_err(|e| io("media_stage_verify", e))?
                .read(&mut b)
                .map_err(|e| io("media_stage_read", e))?;
            material::image_header(material::mime(&name).ok_or(MemoryError::Invalid)?, &b[..n])
                .ok_or(MemoryError::Invalid)?;
            fs::rename(p, target).map_err(|e| io("media_stage_commit", e))?;
            Ok(Response::Done)
        }
        Operation::List => {
            let p = folder(root, true)?;
            let mut names = Vec::new();
            let mut truncated = false;
            for (i, e) in fs::read_dir(p)
                .map_err(|e| io("media_list", e))?
                .enumerate()
            {
                if i >= 128 || names.len() >= 32 {
                    truncated = true;
                    break;
                }
                let e = e.map_err(|e| io("media_list_entry", e))?;
                let name = e.file_name().to_string_lossy().into_owned();
                if material::mime(&name).is_some() {
                    let bytes = file_size(&e.path())?;
                    names.push(json!({"name":name,"bytes":bytes}));
                }
            }
            Ok(Response::Listing(
                json!({"images":names,"truncated":truncated,"image_limit_bytes":material::MAX_IMAGE_BYTES}),
            ))
        }
    }
}

pub fn admin_command(text: &str) -> Result<Option<Operation>, MemoryError> {
    let Some(rest) = text.strip_prefix("sd media ") else {
        return Ok(None);
    };
    if rest == "seed" {
        return Ok(Some(Operation::Seed));
    }
    if rest == "list" {
        return Ok(Some(Operation::List));
    }
    let args: Vec<_> = rest.split_whitespace().collect();
    match args.as_slice() {
        ["info", name] if material::mime(name).is_some() => {
            Ok(Some(Operation::Describe((*name).into())))
        }
        ["put", name, offset, data] => {
            if material::mime(name).is_none()
                || data.is_empty()
                || data.len() > material::CHUNK_BYTES * 2
                || data.len() % 2 != 0
                || !data.bytes().all(|b| b.is_ascii_hexdigit())
            {
                return Err(MemoryError::Invalid);
            }
            let offset = offset.parse::<u64>().map_err(|_| MemoryError::Invalid)?;
            let mut bytes = Vec::with_capacity(data.len() / 2);
            for i in (0..data.len()).step_by(2) {
                bytes.push(
                    u8::from_str_radix(&data[i..i + 2], 16).map_err(|_| MemoryError::Invalid)?,
                );
            }
            if offset > material::MAX_IMAGE_BYTES {
                return Err(MemoryError::Capacity);
            }
            Ok(Some(Operation::Put((*name).into(), offset, bytes)))
        }
        ["finish", name, bytes] => {
            let bytes = bytes.parse::<u64>().map_err(|_| MemoryError::Invalid)?;
            if material::mime(name).is_none() || bytes == 0 || bytes > material::MAX_IMAGE_BYTES {
                return Err(MemoryError::Invalid);
            }
            Ok(Some(Operation::Finish((*name).into(), bytes)))
        }
        _ => Err(MemoryError::Invalid),
    }
}
