//! Pinned ESP-IDF digest owners, called only by the SD worker.
use esp_idf_svc::sys;
use qq_deepseek_core::{material::Digest, memory::MemoryError};
pub struct ImageDigest {
    md5: Box<sys::md5_context_t>,
    sha1: Box<sys::mbedtls_sha1_context>,
}
fn check(code: i32) -> Result<(), MemoryError> {
    if code == 0 {
        Ok(())
    } else {
        Err(MemoryError::Invalid)
    }
}
impl ImageDigest {
    pub fn new() -> Result<Self, MemoryError> {
        let mut md5 = Box::<sys::md5_context_t>::default();
        let mut sha1 = Box::<sys::mbedtls_sha1_context>::default();
        unsafe {
            sys::esp_rom_md5_init(md5.as_mut());
            sys::mbedtls_sha1_init(sha1.as_mut());
        }
        let mut result = Self { md5, sha1 };
        check(unsafe { sys::mbedtls_sha1_starts(result.sha1.as_mut()) })?;
        Ok(result)
    }
}
impl Digest for ImageDigest {
    fn update(&mut self, data: &[u8]) -> Result<(), MemoryError> {
        unsafe {
            sys::esp_rom_md5_update(self.md5.as_mut(), data.as_ptr().cast(), data.len() as u32);
            check(sys::mbedtls_sha1_update(
                self.sha1.as_mut(),
                data.as_ptr(),
                data.len(),
            ))
        }
    }
    fn finish(mut self) -> Result<(String, String), MemoryError> {
        let mut md5 = [0u8; 16];
        let mut sha1 = [0u8; 20];
        unsafe {
            sys::esp_rom_md5_final(md5.as_mut_ptr(), self.md5.as_mut());
            check(sys::mbedtls_sha1_finish(
                self.sha1.as_mut(),
                sha1.as_mut_ptr(),
            ))?;
        }
        Ok((
            md5.iter().map(|v| format!("{v:02x}")).collect(),
            sha1.iter().map(|v| format!("{v:02x}")).collect(),
        ))
    }
}
impl Drop for ImageDigest {
    fn drop(&mut self) {
        unsafe {
            sys::mbedtls_sha1_free(self.sha1.as_mut());
        }
    }
}
