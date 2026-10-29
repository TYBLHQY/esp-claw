pub mod console;
pub mod diagnostics;
pub mod led;
pub mod network;
pub mod sdcard;
pub mod storage;

pub type AppResult<T> = Result<T, String>;

pub fn check(code: i32, stage: &str) -> AppResult<()> {
    if code == esp_idf_svc::sys::ESP_OK {
        Ok(())
    } else {
        Err(format!("{stage}: SDK error 0x{code:x}"))
    }
}
