//! Bounded UART input; never echo a line, since it may contain credentials.
use esp_idf_svc::sys::*;
use qq_deepseek_core::record::crc32;
use serde_json::Value;
use std::io::Write;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    mpsc::{sync_channel, Receiver, SyncSender},
    Arc,
};

use super::{check, AppResult};

pub const MAX_LINE: usize = 6144;
const RX_BUFFER: i32 = 4096;
const MAX_FRAME: usize = 24 * 1024;

/// Serialize into one VFS write: SDK logs share its UART write lock. `println!`
/// writes formatting fragments separately and previously allowed C logs inside JSON.
pub fn frame(kind: &str, value: &Value) -> Option<Vec<u8>> {
    let Ok(body) = serde_json::to_vec(value) else {
        return None;
    };
    if body.len() > MAX_FRAME {
        return None;
    }
    let mut frame = format!("QD2_{kind} {:04x} {:08x} ", body.len(), crc32(&body)).into_bytes();
    frame.extend_from_slice(&body);
    frame.push(b'\n');
    Some(frame)
}

pub fn write_frame(frame: &[u8]) {
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(frame);
    let _ = stdout.flush();
}

pub fn emit(kind: &str, value: &Value) {
    if let Some(frame) = frame(kind, value) {
        write_frame(&frame);
    }
}

pub fn event(value: &Value) {
    emit("EVENT", value);
}
pub fn reply(value: &Value) {
    emit("REPLY", value);
}

/// Explicit maintenance test; never prints input, configuration or credentials.
pub fn stress() -> AppResult<()> {
    std::thread::Builder::new()
        .name("uart-log-test".into())
        .stack_size(3072)
        .spawn(|| {
            for n in 0..40 {
                log::warn!("serial framing test {n}");
                std::thread::sleep(std::time::Duration::from_millis(17));
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

pub enum Input {
    Line(String),
    Invalid,
}

pub struct Console {
    pub receiver: Receiver<Input>,
    pub overruns: Arc<AtomicU32>,
}

pub fn start() -> AppResult<Console> {
    // UART0 parameters/pins come from the fixed board SDK configuration.
    check(
        unsafe { uart_driver_install(0, RX_BUFFER, 0, 0, std::ptr::null_mut(), 0) },
        "console uart",
    )?;
    // Route all SDK and Rust stdout/stderr through the same driver/VFS write lock.
    unsafe {
        esp_vfs_dev_uart_use_driver(0);
    }
    let (tx, rx) = sync_channel(4);
    let overruns = Arc::new(AtomicU32::new(0));
    let counter = overruns.clone();
    std::thread::Builder::new()
        .name("console-rx".into())
        .stack_size(4096)
        .spawn(move || receive(tx, counter))
        .map_err(|e| e.to_string())?;
    Ok(Console {
        receiver: rx,
        overruns,
    })
}

fn deliver(tx: &SyncSender<Input>, overruns: &AtomicU32, input: Input) {
    if tx.try_send(input).is_err() {
        overruns.fetch_add(1, Ordering::Relaxed);
    }
}

fn receive(tx: SyncSender<Input>, overruns: Arc<AtomicU32>) {
    let mut line = Vec::new();
    if line.try_reserve_exact(MAX_LINE).is_err() {
        deliver(&tx, &overruns, Input::Invalid);
        return;
    }
    let mut invalid = false;
    let mut previous_cr = false;
    loop {
        let mut byte = 0_u8;
        let count = unsafe { uart_read_bytes(0, (&mut byte as *mut u8).cast(), 1, 10) };
        if count != 1 {
            std::thread::yield_now();
            continue;
        }
        if byte == b'\r' || byte == b'\n' {
            if byte == b'\n' && previous_cr {
                previous_cr = false;
                continue;
            }
            previous_cr = byte == b'\r';
            if invalid {
                deliver(&tx, &overruns, Input::Invalid);
            } else if !line.is_empty() {
                match String::from_utf8(line.clone()) {
                    Ok(text) => {
                        deliver(&tx, &overruns, Input::Line(text));
                    }
                    Err(_) => {
                        deliver(&tx, &overruns, Input::Invalid);
                    }
                }
            }
            // The retained capacity bounds memory; input bytes are never logged.
            line.fill(0);
            line.clear();
            invalid = false;
        } else if !invalid {
            previous_cr = false;
            if line.len() == MAX_LINE {
                invalid = true;
            } else {
                line.push(byte);
            }
        }
    }
}
