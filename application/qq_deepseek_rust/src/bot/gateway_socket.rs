//! The only WebSocket FFI boundary. This preserves SDK fragment metadata missing from svc 0.53.
use super::protocol::{FrameChunk, MAX_GATEWAY_MESSAGE};
use super::Error;
use esp_idf_svc::hal::delay::TickType;
use esp_idf_svc::sys::{self, ws};
use std::ffi::{c_void, CString};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::Arc;
use std::time::Duration;

const CLEANUP_DEADLINE: Duration = Duration::from_secs(12);
const CLEANUP_STACK_BYTES: usize = 8192;

pub enum Event {
    Connected,
    Disconnected,
    Data(FrameChunk),
    Failed(Error),
    Closed(u16),
}

struct Callback {
    events: SyncSender<Event>,
    lost: Arc<AtomicBool>,
}

pub struct Socket {
    handle: ws::esp_websocket_client_handle_t,
    callback: Option<Box<Callback>>,
    // Keep the URI alive even if a future SDK version stops copying it during init.
    uri: Option<CString>,
    fatal_stop: Arc<AtomicBool>,
    cleanup_error: Arc<AtomicI32>,
    cleanup_active: Arc<AtomicBool>,
}

impl Socket {
    pub fn connect(
        uri: &str,
        events: SyncSender<Event>,
        lost: Arc<AtomicBool>,
        fatal_stop: Arc<AtomicBool>,
        cleanup_error: Arc<AtomicI32>,
        cleanup_active: Arc<AtomicBool>,
    ) -> Result<Self, Error> {
        let uri = CString::new(uri).map_err(|_| Error::Protocol.at("ws_uri"))?;
        let mut callback = Box::new(Callback { events, lost });
        // Initialize the SDK configuration in its heap allocation, not as a large stack value.
        let mut conf = Box::<ws::esp_websocket_client_config_t>::new_uninit();
        unsafe {
            conf.as_mut_ptr().write_bytes(0, 1);
        }
        let mut conf = unsafe { conf.assume_init() };
        conf.uri = uri.as_ptr();
        conf.disable_auto_reconnect = true;
        conf.task_prio = 5;
        conf.task_stack = 6144;
        conf.buffer_size = 2048;
        conf.transport = ws::esp_websocket_transport_t_WEBSOCKET_TRANSPORT_OVER_SSL;
        conf.crt_bundle_attach = Some(sys::esp_crt_bundle_attach);
        conf.skip_cert_common_name_check = false;
        conf.network_timeout_ms = 8000;
        conf.ping_interval_sec = 20;
        conf.pingpong_timeout_sec = 10;
        let handle = unsafe { ws::esp_websocket_client_init(conf.as_ref()) };
        if handle.is_null() {
            return Err(Error::Allocation.at("ws_init"));
        }
        let result = unsafe {
            ws::esp_websocket_register_events(
                handle,
                ws::esp_websocket_event_id_t_WEBSOCKET_EVENT_ANY,
                Some(callback_event),
                callback.as_mut() as *mut Callback as *mut c_void,
            )
        };
        if result != sys::ESP_OK {
            unsafe {
                ws::esp_websocket_client_destroy(handle);
            }
            return Err(Error::Diagnostic {
                stage: "ws_register",
                code: result,
            });
        }
        // Own the callback before starting, so every failure follows one cleanup path.
        let socket = Self {
            handle,
            callback: Some(callback),
            uri: Some(uri),
            fatal_stop,
            cleanup_error,
            cleanup_active,
        };
        let result = unsafe { ws::esp_websocket_client_start(handle) };
        if result != sys::ESP_OK {
            return Err(Error::Diagnostic {
                stage: "ws_start",
                code: result,
            });
        }
        Ok(socket)
    }

    pub fn send_text(&mut self, data: &[u8]) -> Result<(), Error> {
        if data.len() > 4096 {
            return Err(Error::Limit);
        }
        let timeout = TickType::from(Duration::from_secs(1)).ticks();
        let written = unsafe {
            ws::esp_websocket_client_send_text(
                self.handle,
                data.as_ptr() as _,
                data.len() as _,
                timeout,
            )
        };
        if written < 0 {
            return Err(Error::Diagnostic {
                stage: "ws_send",
                code: written,
            });
        }
        if written as usize != data.len() {
            return Err(Error::Protocol.at("ws_partial_send"));
        }
        Ok(())
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        // SDK 1.8.0 close sends with portMAX_DELAY, then can wait indefinitely
        // for STOPPED_BIT even after its timeout. stop/destroy can also wait
        // indefinitely. Give their sole ownership to a cleanup task; never free
        // its callback or URI from this task if the SDK fails to return.
        let owner = CleanupOwner {
            handle: self.handle as usize,
            callback: self.callback.take(),
            uri: self.uri.take(),
        };
        self.handle = std::ptr::null_mut();
        let (tx, rx) = sync_channel(1);
        let cleanup_active = self.cleanup_active.clone();
        cleanup_active.store(true, Ordering::Release);
        let worker = std::thread::Builder::new()
            .name("qq-ws-cleanup".into())
            .stack_size(CLEANUP_STACK_BYTES)
            .spawn(move || {
                let mut owner = owner;
                let result = owner.stop_and_destroy();
                drop(owner);
                cleanup_active.store(false, Ordering::Release);
                let _ = tx.try_send(result);
            });
        let result = match worker {
            Ok(_detached) => match rx.recv_timeout(CLEANUP_DEADLINE) {
                Ok(code) => code,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => sys::ESP_ERR_TIMEOUT,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => sys::ESP_FAIL,
            },
            // The rejected closure drops its CleanupOwner, which deliberately
            // retains C-owned pointers. No second task attempts their cleanup.
            Err(_) => {
                self.cleanup_active.store(false, Ordering::Release);
                sys::ESP_ERR_NO_MEM
            }
        };
        if result != sys::ESP_OK {
            self.cleanup_error.store(result, Ordering::Release);
            self.fatal_stop.store(true, Ordering::Relaxed);
            log::error!("bot stage=ws_cleanup code={result}; reboot required");
        }
    }
}

/// Exactly one cleanup task owns this C client, including on deadline expiry.
/// The integer address crosses the task boundary without claiming the Socket
/// is generally Send; no other task may access the handle after transfer.
struct CleanupOwner {
    handle: usize,
    callback: Option<Box<Callback>>,
    uri: Option<CString>,
}

impl CleanupOwner {
    fn stop_and_destroy(&mut self) -> i32 {
        let handle = self.handle as ws::esp_websocket_client_handle_t;
        // stop closes TCP directly and avoids close's unbounded send/handshake.
        // An already-stopped client returns ESP_FAIL; destroy handles that case.
        unsafe { ws::esp_websocket_client_stop(handle) };
        let result = unsafe { ws::esp_websocket_client_destroy(handle) };
        if result == sys::ESP_OK {
            // The SDK has stopped its task and released the handle. Only now
            // can the callback target and retained URI be released normally.
            self.handle = 0;
        }
        result
    }
}

impl Drop for CleanupOwner {
    fn drop(&mut self) {
        if self.handle != 0 {
            if let Some(callback) = self.callback.take() {
                Box::leak(callback);
            }
            if let Some(uri) = self.uri.take() {
                let _ = uri.into_raw();
            }
        }
    }
}

unsafe extern "C" fn callback_event(
    argument: *mut c_void,
    _base: sys::esp_event_base_t,
    event: i32,
    data: *mut c_void,
) {
    let Some(callback) = (unsafe { (argument as *const Callback).as_ref() }) else {
        return;
    };
    let value = if event == ws::esp_websocket_event_id_t_WEBSOCKET_EVENT_CONNECTED as i32 {
        Some(Event::Connected)
    } else if event == ws::esp_websocket_event_id_t_WEBSOCKET_EVENT_DISCONNECTED as i32
        || event == ws::esp_websocket_event_id_t_WEBSOCKET_EVENT_CLOSED as i32
    {
        Some(Event::Disconnected)
    } else if event == ws::esp_websocket_event_id_t_WEBSOCKET_EVENT_ERROR as i32 {
        let failure = if let Some(data) =
            unsafe { (data as *const ws::esp_websocket_event_data_t).as_ref() }
        {
            let error = &data.error_handle;
            if error.esp_ws_handshake_status_code != 0 {
                Error::Diagnostic {
                    stage: "ws_handshake",
                    code: error.esp_ws_handshake_status_code,
                }
            } else if error.esp_tls_last_esp_err != 0 {
                Error::Diagnostic {
                    stage: "ws_tls",
                    code: error.esp_tls_last_esp_err,
                }
            } else if error.esp_tls_cert_verify_flags != 0 {
                Error::Diagnostic {
                    stage: "ws_tls_verify",
                    code: error.esp_tls_cert_verify_flags,
                }
            } else if error.esp_tls_stack_err != 0 {
                Error::Diagnostic {
                    stage: "ws_tls_stack",
                    code: error.esp_tls_stack_err,
                }
            } else {
                Error::Diagnostic {
                    stage: "ws_socket_errno",
                    code: error.esp_transport_sock_errno,
                }
            }
        } else {
            Error::Transport.at("ws_error_event")
        };
        Some(Event::Failed(failure))
    } else if event == ws::esp_websocket_event_id_t_WEBSOCKET_EVENT_DATA as i32 {
        let Some(data) = (unsafe { (data as *const ws::esp_websocket_event_data_t).as_ref() })
        else {
            callback.lost.store(true, Ordering::Relaxed);
            return;
        };
        if data.op_code == 8 {
            let code = if data.data_len >= 2 && !data.data_ptr.is_null() {
                let bytes = unsafe { std::slice::from_raw_parts(data.data_ptr as *const u8, 2) };
                u16::from_be_bytes([bytes[0], bytes[1]])
            } else {
                1005
            };
            if callback.events.try_send(Event::Closed(code)).is_err() {
                callback.lost.store(true, Ordering::Relaxed);
            }
            return;
        }
        if !matches!(data.op_code, 0 | 1) {
            return;
        }
        if data.data_len < 0
            || data.data_len > 2048
            || data.payload_len < 0
            || data.payload_len as usize > MAX_GATEWAY_MESSAGE
            || data.payload_offset < 0
            || data.payload_offset > data.payload_len
            || data.data_len > data.payload_len - data.payload_offset
            || (data.data_len != 0 && data.data_ptr.is_null())
        {
            callback.lost.store(true, Ordering::Relaxed);
            return;
        }
        let mut bytes = Vec::new();
        if bytes.try_reserve_exact(data.data_len as usize).is_err() {
            callback.lost.store(true, Ordering::Relaxed);
            return;
        }
        if data.data_len != 0 {
            // The C event owns this slice only during callback execution. Copy it before return.
            let input = unsafe {
                std::slice::from_raw_parts(data.data_ptr as *const u8, data.data_len as usize)
            };
            bytes.extend_from_slice(input);
        }
        Some(Event::Data(FrameChunk {
            opcode: data.op_code,
            fin: data.fin,
            frame_len: data.payload_len as usize,
            offset: data.payload_offset as usize,
            bytes,
        }))
    } else {
        None
    };
    if let Some(value) = value {
        if callback.events.try_send(value).is_err() {
            callback.lost.store(true, Ordering::Relaxed);
        }
    }
}
