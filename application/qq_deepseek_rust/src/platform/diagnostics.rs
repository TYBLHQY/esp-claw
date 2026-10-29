use esp_idf_svc::sys::*;
use serde_json::{json, Value};

use super::AppResult;

const EXPECTED_FLASH: u32 = 16 * 1024 * 1024;
const EXPECTED_PSRAM: usize = 8 * 1024 * 1024;
const PROBE_BYTES: usize = 4096;

pub fn status() -> Value {
    let mut flash_size = 0;
    // SAFETY: SDK fills a scalar; a null chip selects the default flash.
    let flash_result = unsafe { esp_flash_get_size(std::ptr::null_mut(), &mut flash_size) };
    json!({"board": "GOOUUU ESP32-S3-CAM N16R8", "firmware": env!("CARGO_PKG_VERSION"),
        "reset_reason": unsafe { esp_reset_reason() },
        "flash_bytes": flash_size, "flash_query_error": flash_result,
        "psram_bytes": unsafe { esp_psram_get_size() },
        "internal_free": unsafe { heap_caps_get_free_size(MALLOC_CAP_INTERNAL | MALLOC_CAP_8BIT) },
        "internal_minimum": unsafe { heap_caps_get_minimum_free_size(MALLOC_CAP_INTERNAL | MALLOC_CAP_8BIT) },
        "internal_largest_block": unsafe { heap_caps_get_largest_free_block(MALLOC_CAP_INTERNAL | MALLOC_CAP_8BIT) },
        "psram_free": unsafe { heap_caps_get_free_size(MALLOC_CAP_SPIRAM) },
        "stack_high_water_bytes": unsafe { uxTaskGetStackHighWaterMark(std::ptr::null_mut()) },
        "sd": "optional_sdmmc", "camera": "disabled"})
}

pub fn self_test() -> AppResult<Value> {
    let mut flash_size = 0;
    super::check(
        unsafe { esp_flash_get_size(std::ptr::null_mut(), &mut flash_size) },
        "flash size",
    )?;
    if flash_size != EXPECTED_FLASH {
        return Err("unexpected flash capacity".into());
    }
    if unsafe { esp_psram_get_size() } != EXPECTED_PSRAM {
        return Err("unexpected PSRAM capacity".into());
    }
    // SAFETY: explicitly allocate PSRAM, check null, bound every read/write, then free.
    let probe =
        unsafe { heap_caps_malloc(PROBE_BYTES, MALLOC_CAP_SPIRAM | MALLOC_CAP_8BIT) } as *mut u8;
    if probe.is_null() {
        return Err("PSRAM probe allocation failed".into());
    }
    for i in 0..PROBE_BYTES {
        // Volatile accesses force real memory operations rather than allowing
        // the optimizer to prove the comparison from the preceding writes.
        unsafe {
            probe
                .add(i)
                .write_volatile((i as u8).wrapping_mul(31) ^ 0xa5)
        };
    }
    let mut valid = true;
    for i in 0..PROBE_BYTES {
        valid &= unsafe { probe.add(i).read_volatile() } == (i as u8).wrapping_mul(31) ^ 0xa5;
    }
    unsafe { heap_caps_free(probe.cast()) };
    if !valid {
        return Err("PSRAM probe readback mismatch".into());
    }
    Ok(json!({"flash_capacity_ok": true, "psram_capacity_ok": true,
        "psram_test_bytes": PROBE_BYTES, "psram_readback_ok": true,
        "sd_required": false, "led_requires_visual_check": true}))
}
