//! Optional SDMMC storage. All SDK handles and filesystem I/O belong to one worker.
//! A failed card never owns the board loop, QQ heartbeat, Wi-Fi or NVS configuration.
#[path = "sdcard_files.rs"]
mod files;
#[path = "history_files.rs"]
mod history_files;
#[path = "image_digest.rs"]
mod image_digest;
#[path = "media_files.rs"]
mod media_files;
#[path = "memory_files.rs"]
mod memory_files;
use qq_deepseek_core::{
    history::{self, SearchResult},
    memory::MemoryTurn,
};

use super::AppResult;
use esp_idf_svc::sys;
use serde_json::{json, Value};
use std::{
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub use files::JournalEvent;
pub use qq_deepseek_core::memory::{LoadedMemory, MemoryError, SessionMemory};

const MOUNT_PATH: &str = "/sdcard";
const MOUNT_PATH_C: &[u8] = b"/sdcard\0";
const APP_DIRECTORY: &str = "/sdcard/qq-deepseek";
const HOST_SLOT: i32 = 1;
const CLOCK_GPIO: i32 = 39;
const COMMAND_GPIO: i32 = 38;
const DATA_GPIO: i32 = 40;
const COMMAND_TIMEOUT_MS: i32 = 1000;
const OPERATION_DEADLINE: Duration = Duration::from_secs(20);
const MAX_MOUNT_ATTEMPTS: u32 = 3;
const WORKER_STACK_BYTES: usize = 12288;
const MEMORY_WAIT: Duration = Duration::from_secs(3);
// These BIT() macros are not emitted by bindgen. Values are from the pinned
// ESP-IDF 5.5.4 sd_protocol_types.h and driver/sdmmc_host.h, respectively.
const HOST_FLAG_1BIT: u32 = 1 << 0;
const HOST_FLAG_DEINIT_ARG: u32 = 1 << 5;
const SLOT_FLAG_INTERNAL_PULLUP: u32 = 1 << 0;

#[derive(Clone, Copy)]
struct Fault {
    stage: &'static str,
    code: i32,
}
struct Snapshot {
    phase: &'static str,
    mounted: bool,
    capacity_bytes: Option<u64>,
    filesystem_bytes: Option<u64>,
    free_bytes: Option<u64>,
    operation: Option<&'static str>,
    started: Option<Instant>,
    sequence: u32,
    mount_attempts: u32,
    successful_tests: u32,
    reboot_required: bool,
    fault: Option<Fault>,
    last_result: Value,
}
#[derive(Default)]
struct MemorySnapshot {
    loads: u64,
    saves: u64,
    resets: u64,
    history_appends: u64,
    history_searches: u64,
    failures: u64,
    timeouts: u64,
    last_revision: u64,
    last_error: Option<MemoryError>,
}
struct Shared {
    snapshot: Mutex<Snapshot>,
    memory: Mutex<MemorySnapshot>,
    busy: AtomicBool,
    poisoned: AtomicBool,
    stop: AtomicBool,
}

enum Request {
    Mount,
    Unmount,
    Test,
    List,
    Journal(JournalEvent),
    Memory(Box<MemoryRequest>),
    MediaAdmin(Box<qq_deepseek_core::material::StorageOp>),
}
impl Request {
    fn name(&self) -> &'static str {
        match self {
            Self::Mount => "mount",
            Self::Unmount => "unmount",
            Self::Test => "test",
            Self::List => "list",
            Self::Journal(_) => "journal",
            Self::Memory(request) => request.operation.name(),
            Self::MediaAdmin(_) => "media_admin",
        }
    }
}

pub struct SdCard {
    requests: SyncSender<Request>,
    shared: Arc<Shared>,
    _worker: JoinHandle<()>,
}
impl SdCard {
    /// Starts the owner without touching the card. Request one explicit boot mount
    /// when desired; there is no periodic mount or repair loop.
    pub fn start() -> AppResult<Self> {
        let shared = Arc::new(Shared {
            snapshot: Mutex::new(Snapshot {
                phase: "disabled",
                mounted: false,
                capacity_bytes: None,
                filesystem_bytes: None,
                free_bytes: None,
                operation: None,
                started: None,
                sequence: 0,
                mount_attempts: 0,
                successful_tests: 0,
                reboot_required: false,
                fault: None,
                last_result: Value::Null,
            }),
            memory: Mutex::new(MemorySnapshot::default()),
            busy: AtomicBool::new(false),
            poisoned: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        });
        let (tx, rx) = sync_channel(2);
        let state = shared.clone();
        let worker = thread::Builder::new()
            .name("sd-owner".into())
            .stack_size(WORKER_STACK_BYTES)
            .spawn(move || run(state, rx))
            .map_err(|_| "could not start SD worker")?;
        Ok(Self {
            requests: tx,
            shared,
            _worker: worker,
        })
    }

    pub fn memory_client(&self) -> MemoryClient {
        MemoryClient {
            requests: self.requests.clone(),
            shared: self.shared.clone(),
        }
    }

    pub fn status(&self) -> Value {
        let Ok(state) = self.shared.snapshot.lock() else {
            return json!({"phase":"state_failed","available":false,"reboot_required":true});
        };
        let elapsed = state.started.map(|start| start.elapsed());
        let timed_out = self.shared.busy.load(Ordering::Acquire)
            && elapsed.is_some_and(|elapsed| elapsed >= OPERATION_DEADLINE);
        if timed_out {
            // Do not force-delete a task or free a C handle while I/O may be active.
            self.shared.poisoned.store(true, Ordering::Release);
        }
        let poisoned = self.shared.poisoned.load(Ordering::Acquire);
        let fault = if timed_out {
            Some(Fault {
                stage: "worker_operation_deadline",
                code: sys::ESP_ERR_TIMEOUT,
            })
        } else {
            state.fault
        };
        json!({
            "phase":if timed_out {"operation_timed_out"} else {state.phase},
            "available":state.mounted && !poisoned && !state.reboot_required,
            "mounted":state.mounted,"busy":self.shared.busy.load(Ordering::Acquire),
            "io_disabled":poisoned,"reboot_required":state.reboot_required,
            "mount_path":MOUNT_PATH,"application_directory":APP_DIRECTORY,
            "bus":"SDMMC","slot":HOST_SLOT,"bus_width":1,"max_frequency_khz":sys::SDMMC_FREQ_DEFAULT,
            "pins":{"clk":CLOCK_GPIO,"cmd":COMMAND_GPIO,"d0":DATA_GPIO,"card_detect":null,"write_protect":null},
            "card_presence":"not_independently_detectable","automatic_format":false,"automatic_repair":false,
            "command_timeout_ms":COMMAND_TIMEOUT_MS,"operation_deadline_seconds":OPERATION_DEADLINE.as_secs(),
            "operation":state.operation,"operation_elapsed_ms":elapsed.map(|elapsed|elapsed.as_millis() as u64),
            "sequence":state.sequence,"mount_attempts":state.mount_attempts,
            "capacity_bytes":state.capacity_bytes,"filesystem_bytes":state.filesystem_bytes,"free_bytes":state.free_bytes,
            "successful_tests":state.successful_tests,"history_persistence":state.mounted && !poisoned && !state.reboot_required,
            "fault":fault.map(|fault|json!({"stage":fault.stage,"code":fault.code})),
            "last_result":state.last_result,
        })
    }

    pub fn command(&self, command: &str) -> AppResult<Value> {
        if let Some(op) = media_files::admin_command(command).map_err(|e| e.to_string())? {
            return self.submit(Request::MediaAdmin(Box::new(op)));
        }
        let request = match command.trim() {
            "sd status" => return Ok(self.status()),
            "sd mount" => Request::Mount,
            "sd unmount" => Request::Unmount,
            "sd test" => Request::Test,
            "sd list" => Request::List,
            _ => return Err("unknown SD command; use sd status|mount|unmount|test|list".into()),
        };
        self.submit(request)
    }

    /// Only typed, fixed diagnostics may enter the journal; never pass user text or keys.
    pub fn journal(&self, event: JournalEvent) -> AppResult<Value> {
        self.submit(Request::Journal(event))
    }

    fn submit(&self, request: Request) -> AppResult<Value> {
        // Also makes deadline expiry visible when a new command arrives before a status poll.
        let _ = self.status();
        if self.shared.stop.load(Ordering::Acquire) {
            return Err("SD owner is stopping".into());
        }
        let mut state = self
            .shared
            .snapshot
            .lock()
            .map_err(|_| "SD state unavailable")?;
        if state.reboot_required {
            return Err("SD cleanup failed; reboot before further SD operations".into());
        }
        if self.shared.busy.load(Ordering::Acquire) {
            return Err("SD operation already running; inspect sd status".into());
        }
        if !matches!(request, Request::Unmount) && self.shared.poisoned.load(Ordering::Acquire) {
            return Err("SD I/O disabled after failure; explicitly unmount before retrying".into());
        }
        match request {
            Request::Mount if state.mounted => return Err("SD is already mounted".into()),
            Request::Mount if state.mount_attempts >= MAX_MOUNT_ATTEMPTS => {
                return Err(
                    "SD mount attempt budget exhausted; reboot before another attempt".into(),
                );
            }
            Request::Unmount if !state.mounted => {
                self.shared.poisoned.store(false, Ordering::Release);
                state.phase = "disabled";
                state.fault = None;
                return Ok(json!({"unmounted":true}));
            }
            Request::Test | Request::List | Request::Journal(_) | Request::Memory(_)
                if !state.mounted =>
            {
                return Err("SD is not mounted; inspect sd status".into());
            }
            _ => (),
        }
        let operation = request.name();
        state.operation = Some(operation);
        state.started = Some(Instant::now());
        state.sequence = state.sequence.saturating_add(1);
        state.last_result = Value::Null;
        state.fault = None;
        state.phase = if matches!(request, Request::Mount) {
            "mounting"
        } else {
            "working"
        };
        if matches!(request, Request::Mount) {
            state.mount_attempts += 1;
        }
        self.shared.busy.store(true, Ordering::Release);
        let sequence = state.sequence;
        if self.requests.try_send(request).is_err() {
            self.shared.busy.store(false, Ordering::Release);
            state.phase = "worker_unavailable";
            state.reboot_required = true;
            return Err("SD worker unavailable; reboot required".into());
        }
        Ok(json!({"started":true,"operation":operation,"sequence":sequence}))
    }
}
impl Drop for SdCard {
    fn drop(&mut self) {
        // No main-thread join: the worker performs its own cleanup after active I/O returns.
        self.shared.stop.store(true, Ordering::Release);
    }
}

struct Mounted {
    card: *mut sys::sdmmc_card_t,
}
impl Mounted {
    fn mount() -> Result<Self, Fault> {
        // Use the generated SDK types and exact pinned SDMMC_HOST_DEFAULT() callback set.
        // Initialize the >128-byte descriptor directly in its heap allocation.
        let mut host = Box::<sys::sdmmc_host_t>::new_uninit();
        unsafe { host.as_mut_ptr().write_bytes(0, 1) };
        let mut host = unsafe { host.assume_init() };
        host.flags = HOST_FLAG_1BIT | HOST_FLAG_DEINIT_ARG;
        host.slot = HOST_SLOT;
        host.max_freq_khz = sys::SDMMC_FREQ_DEFAULT as i32;
        host.io_voltage = 3.3;
        host.driver_strength = sys::sdmmc_driver_strength_t_SDMMC_DRIVER_STRENGTH_B;
        host.current_limit = sys::sdmmc_current_limit_t_SDMMC_CURRENT_LIMIT_200MA;
        host.init = Some(sys::sdmmc_host_init);
        host.set_bus_width = Some(sys::sdmmc_host_set_bus_width);
        host.get_bus_width = Some(sys::sdmmc_host_get_slot_width);
        host.set_bus_ddr_mode = Some(sys::sdmmc_host_set_bus_ddr_mode);
        host.set_card_clk = Some(sys::sdmmc_host_set_card_clk);
        host.set_cclk_always_on = Some(sys::sdmmc_host_set_cclk_always_on);
        host.do_transaction = Some(sys::sdmmc_host_do_transaction);
        host.__bindgen_anon_1.deinit_p = Some(sys::sdmmc_host_deinit_slot);
        host.io_int_enable = Some(sys::sdmmc_host_io_int_enable);
        host.io_int_wait = Some(sys::sdmmc_host_io_int_wait);
        host.command_timeout_ms = COMMAND_TIMEOUT_MS;
        host.get_real_freq = Some(sys::sdmmc_host_get_real_freq);
        host.input_delay_phase = sys::sdmmc_delay_phase_t_SDMMC_DELAY_PHASE_0;
        host.set_input_delay = Some(sys::sdmmc_host_set_input_delay);
        host.check_buffer_alignment = Some(sys::sdmmc_host_check_buffer_alignment);
        host.is_slot_set_to_uhs1 = Some(sys::sdmmc_host_is_slot_set_to_uhs1);
        let mut slot = sys::sdmmc_slot_config_t {
            clk: CLOCK_GPIO,
            cmd: COMMAND_GPIO,
            d0: DATA_GPIO,
            d1: sys::gpio_num_t_GPIO_NUM_NC,
            d2: sys::gpio_num_t_GPIO_NUM_NC,
            d3: sys::gpio_num_t_GPIO_NUM_NC,
            d4: sys::gpio_num_t_GPIO_NUM_NC,
            d5: sys::gpio_num_t_GPIO_NUM_NC,
            d6: sys::gpio_num_t_GPIO_NUM_NC,
            d7: sys::gpio_num_t_GPIO_NUM_NC,
            width: 1,
            flags: SLOT_FLAG_INTERNAL_PULLUP,
            ..Default::default()
        };
        slot.__bindgen_anon_1.cd = sys::gpio_num_t_GPIO_NUM_NC;
        slot.__bindgen_anon_2.wp = sys::gpio_num_t_GPIO_NUM_NC;
        let config = sys::esp_vfs_fat_mount_config_t {
            format_if_mount_failed: false,
            max_files: 5,
            allocation_unit_size: 0,
            disk_status_check_enable: true,
            use_one_fat: false,
        };
        let mut card = std::ptr::null_mut();
        let code = unsafe {
            sys::esp_vfs_fat_sdmmc_mount(
                MOUNT_PATH_C.as_ptr().cast(),
                host.as_ref(),
                (&slot as *const sys::sdmmc_slot_config_t).cast(),
                &config,
                &mut card,
            )
        };
        if code != sys::ESP_OK {
            // The SDK mount failure path performs host/VFS cleanup. The out pointer
            // may have been assigned before a later allocation failed: never use it.
            return Err(Fault {
                stage: "sdmmc_or_filesystem_mount",
                code,
            });
        }
        if card.is_null() {
            return Err(Fault {
                stage: "sdmmc_mount_null_card",
                code: sys::ESP_ERR_INVALID_STATE,
            });
        }
        Ok(Self { card })
    }

    fn capacity(&self) -> Option<u64> {
        // The SDK-owned card remains valid until this same worker unmounts it.
        let card = unsafe { &*self.card };
        let sectors = u64::try_from(card.csd.capacity).ok()?;
        let bytes = u64::try_from(card.csd.sector_size).ok()?;
        sectors.checked_mul(bytes)
    }

    fn filesystem_info(&self) -> Result<(u64, u64), Fault> {
        let (mut total, mut free) = (0, 0);
        let code =
            unsafe { sys::esp_vfs_fat_info(MOUNT_PATH_C.as_ptr().cast(), &mut total, &mut free) };
        if code == sys::ESP_OK {
            Ok((total, free))
        } else {
            Err(Fault {
                stage: "filesystem_capacity",
                code,
            })
        }
    }

    fn unmount(self) -> Result<(), Fault> {
        // The SDK may free card/context even when a later unregister returns an error.
        // Consume this handle unconditionally and never reuse it or retry its cleanup.
        let code =
            unsafe { sys::esp_vfs_fat_sdcard_unmount(MOUNT_PATH_C.as_ptr().cast(), self.card) };
        if code == sys::ESP_OK {
            Ok(())
        } else {
            Err(Fault {
                stage: "sdmmc_unmount",
                code,
            })
        }
    }
}

fn run(shared: Arc<Shared>, requests: Receiver<Request>) {
    let mut mounted: Option<Mounted> = None;
    while !shared.stop.load(Ordering::Acquire) {
        let request = match requests.recv_timeout(Duration::from_millis(200)) {
            Ok(request) => request,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if let Request::Memory(request) = request {
            run_memory(&shared, mounted.is_some(), *request);
            continue;
        }
        let is_mount = matches!(request, Request::Mount);
        let is_unmount = matches!(request, Request::Unmount);
        let is_media = matches!(request, Request::MediaAdmin(_));
        let result = if let Request::MediaAdmin(op) = request {
            if mounted.is_none() {
                Err((
                    Fault {
                        stage: "media_sd_unavailable",
                        code: sys::ESP_ERR_INVALID_STATE,
                    },
                    false,
                ))
            } else {
                media_files::execute(
                    Path::new(APP_DIRECTORY),
                    *op,
                    image_digest::ImageDigest::new,
                )
                .map(|r| match r {
                    qq_deepseek_core::material::StorageResult::Listing(v) => v,
                    qq_deepseek_core::material::StorageResult::Info(v)=>json!({"name":v.name,"bytes":v.bytes,"md5":v.md5,"sha1":v.sha1,"width":v.width,"height":v.height}),
                    _ => json!({"media_operation_ok":true}),
                })
                .map_err(|e| {
                    (
                        Fault {
                            stage: match e {
                                MemoryError::Io { stage, .. } => stage,
                                _ => "media_admin",
                            },
                            code: match e {
                                MemoryError::Io { code, .. } => code,
                                _ => sys::ESP_ERR_INVALID_ARG,
                            },
                        },
                        matches!(e, MemoryError::Io { .. }),
                    )
                })
            }
        } else {
            execute(&shared, &mut mounted, &request)
        };
        if let Ok(mut state) = shared.snapshot.lock() {
            let expired = state
                .started
                .is_some_and(|start| start.elapsed() >= OPERATION_DEADLINE);
            if expired {
                shared.poisoned.store(true, Ordering::Release);
            }
            state.mounted = mounted.is_some();
            match result {
                Ok(value) => {
                    state.last_result = value;
                    state.phase = if shared.poisoned.load(Ordering::Acquire) {
                        "disabled_after_failure"
                    } else if mounted.is_some() {
                        "available"
                    } else {
                        "disabled"
                    };
                }
                Err((fault, writing)) => {
                    state.fault = Some(fault);
                    state.phase = if is_mount && mounted.is_none() {
                        "mount_failed"
                    } else if is_unmount {
                        state.reboot_required = true;
                        "cleanup_failed"
                    } else if writing {
                        "write_failed"
                    } else {
                        "read_failed"
                    };
                    if (mounted.is_some() && (!is_media || writing)) || is_unmount {
                        shared.poisoned.store(true, Ordering::Release);
                    }
                }
            }
            if expired {
                state.fault = Some(Fault {
                    stage: "worker_operation_deadline",
                    code: sys::ESP_ERR_TIMEOUT,
                });
            }
            state.started = None;
        }
        shared.busy.store(false, Ordering::Release);
    }
    if let Some(card) = mounted {
        let _ = card.unmount();
    }
}

fn execute(
    shared: &Shared,
    mounted: &mut Option<Mounted>,
    request: &Request,
) -> Result<Value, (Fault, bool)> {
    match request {
        Request::MediaAdmin(_) => {
            // Owned requests are consumed by run(); no filesystem handle leaves the owner.
            unreachable!("media admin handled by owner")
        }
        Request::Memory(_) => unreachable!("memory is handled by its typed owner path"),
        Request::Mount => {
            let card = Mounted::mount().map_err(|error| (error, false))?;
            let capacity = card.capacity();
            let info = card.filesystem_info();
            *mounted = Some(card);
            if let Ok(mut state) = shared.snapshot.lock() {
                state.capacity_bytes = capacity;
            }
            let (total, free) = info.map_err(|error| (error, false))?;
            if let Ok(mut state) = shared.snapshot.lock() {
                state.filesystem_bytes = Some(total);
                state.free_bytes = Some(free);
            }
            Ok(json!({"mounted":true,"existing_files_preserved":true,"formatted":false}))
        }
        Request::Unmount => {
            if let Some(card) = mounted.take() {
                card.unmount().map_err(|error| (error, false))?;
            }
            shared.poisoned.store(false, Ordering::Release);
            if let Ok(mut state) = shared.snapshot.lock() {
                state.capacity_bytes = None;
                state.filesystem_bytes = None;
                state.free_bytes = None;
            }
            Ok(json!({"unmounted":true}))
        }
        Request::Test => {
            let report = files::self_test(Path::new(APP_DIRECTORY), unsafe { sys::esp_random() })
                .map_err(|error| {
                (
                    Fault {
                        stage: error.stage,
                        code: error.code,
                    },
                    error.writing,
                )
            })?;
            if let Ok(mut state) = shared.snapshot.lock() {
                state.successful_tests = state.successful_tests.saturating_add(1);
            }
            Ok(
                json!({"write_sync_read_compare_delete":true,"test_bytes":report.bytes,"nonce":report.nonce,"directory":APP_DIRECTORY}),
            )
        }
        Request::List => {
            let listing = files::list(Path::new(APP_DIRECTORY)).map_err(|error| {
                (
                    Fault {
                        stage: error.stage,
                        code: error.code,
                    },
                    error.writing,
                )
            })?;
            let entries: Vec<Value> = listing.entries.into_iter()
                .map(|entry| json!({"name":entry.name,"bytes":entry.bytes,"directory":entry.directory})).collect();
            Ok(
                json!({"directory":APP_DIRECTORY,"directory_exists":listing.directory_exists,"entries":entries,"truncated":listing.truncated,"limit":files::MAX_LIST_ENTRIES}),
            )
        }
        Request::Journal(event) => {
            let time = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |time| time.as_secs());
            let report =
                files::append_event(Path::new(APP_DIRECTORY), time, *event).map_err(|error| {
                    (
                        Fault {
                            stage: error.stage,
                            code: error.code,
                        },
                        error.writing,
                    )
                })?;
            Ok(
                json!({"journal_bytes":report.bytes,"journal_full":report.full,"limit_bytes":files::MAX_JOURNAL_BYTES}),
            )
        }
    }
}

// A cloneable request handle, never a filesystem or SDK handle. Calls belong on
// inference/diagnostic workers; the main loop only reads the cached status.
#[derive(Clone)]
pub struct MemoryClient {
    requests: SyncSender<Request>,
    shared: Arc<Shared>,
}
enum MemoryOperation {
    Material(Box<qq_deepseek_core::material::StorageOp>),
    Load(String),
    Save(u64, Box<SessionMemory>),
    Reset(String, bool),
    Archive(String, u64, Box<MemoryTurn>),
    SearchHistory(String, String),
}
impl MemoryOperation {
    fn name(&self) -> &'static str {
        match self {
            Self::Material(_) => "image_material",
            Self::Load(_) => "memory_load",
            Self::Save(_, _) => "memory_save",
            Self::Reset(_, _) => "memory_reset",
            Self::Archive(_, _, _) => "history_append",
            Self::SearchHistory(_, _) => "history_search",
        }
    }
}
enum MemoryResponse {
    Material(Box<qq_deepseek_core::material::StorageResult>),
    Loaded(LoadedMemory),
    Revision(u64),
    History(SearchResult),
    Archived,
}
struct MemoryRequest {
    operation: MemoryOperation,
    reply: SyncSender<Result<MemoryResponse, MemoryError>>,
}
impl MemoryClient {
    pub fn material(
        &self,
        op: qq_deepseek_core::material::StorageOp,
    ) -> Result<qq_deepseek_core::material::StorageResult, MemoryError> {
        match self.call(MemoryOperation::Material(Box::new(op)))? {
            MemoryResponse::Material(r) => Ok(*r),
            _ => Err(MemoryError::Unavailable),
        }
    }
    pub fn load(&self, key: &str) -> Result<LoadedMemory, MemoryError> {
        if !qq_deepseek_core::memory::valid_key(key) {
            return Err(MemoryError::Invalid);
        }
        match self.call(MemoryOperation::Load(key.to_owned()))? {
            MemoryResponse::Loaded(value) => Ok(value),
            _ => Err(MemoryError::Unavailable),
        }
    }
    pub fn save(&self, expected_revision: u64, data: SessionMemory) -> Result<u64, MemoryError> {
        data.validate()?;
        match self.call(MemoryOperation::Save(expected_revision, Box::new(data)))? {
            MemoryResponse::Revision(value) => Ok(value),
            _ => Err(MemoryError::Unavailable),
        }
    }
    pub fn reset(&self, key: &str, keep_facts: bool) -> Result<u64, MemoryError> {
        if !qq_deepseek_core::memory::valid_key(key) {
            return Err(MemoryError::Invalid);
        }
        match self.call(MemoryOperation::Reset(key.to_owned(), keep_facts))? {
            MemoryResponse::Revision(value) => Ok(value),
            _ => Err(MemoryError::Unavailable),
        }
    }
    pub fn archive(&self, key: &str, revision: u64, turn: &MemoryTurn) -> Result<(), MemoryError> {
        if !qq_deepseek_core::memory::valid_key(key) {
            return Err(MemoryError::Invalid);
        }
        let turn = MemoryTurn {
            user: history::clip(&turn.user, history::USER_BYTES).into(),
            assistant: history::clip(&turn.assistant, history::ASSISTANT_BYTES).into(),
        };
        match self.call(MemoryOperation::Archive(
            key.into(),
            revision,
            Box::new(turn),
        ))? {
            MemoryResponse::Archived => Ok(()),
            _ => Err(MemoryError::Unavailable),
        }
    }
    pub fn search_history(&self, key: &str, query: &str) -> Result<SearchResult, MemoryError> {
        if !qq_deepseek_core::memory::valid_key(key) {
            return Err(MemoryError::Invalid);
        }
        history::validate_query(query)?;
        match self.call(MemoryOperation::SearchHistory(key.into(), query.into()))? {
            MemoryResponse::History(result) => Ok(result),
            _ => Err(MemoryError::Unavailable),
        }
    }
    pub fn status(&self) -> Value {
        let Ok(state) = self.shared.snapshot.lock() else {
            return json!({"available":false,"error":"state_unavailable"});
        };
        let Ok(memory) = self.shared.memory.lock() else {
            return json!({"available":false,"error":"state_unavailable"});
        };
        json!({"backend":"sd_dual_slot","available":state.mounted && !state.reboot_required && !self.shared.poisoned.load(Ordering::Acquire),
            "busy":self.shared.busy.load(Ordering::Acquire),"deadline_seconds":MEMORY_WAIT.as_secs(),
            "max_persisted_sessions":qq_deepseek_core::memory::MAX_PERSISTED_SESSIONS,
            "max_record_payload_bytes":qq_deepseek_core::memory::MAX_PAYLOAD_BYTES,
            "loads":memory.loads,"saves":memory.saves,"resets":memory.resets,
            "history_appends":memory.history_appends,"history_searches":memory.history_searches,
            "history_window_turns":history::HISTORY_SLOTS,"history_max_record_bytes":history::MAX_RECORD_BYTES,
            "failures":memory.failures,"timeouts":memory.timeouts,
            "last_revision":memory.last_revision,"last_error":memory.last_error})
    }
    fn call(&self, operation: MemoryOperation) -> Result<MemoryResponse, MemoryError> {
        let (tx, rx) = sync_channel(1);
        {
            let mut state = self
                .shared
                .snapshot
                .lock()
                .map_err(|_| MemoryError::Unavailable)?;
            if self.shared.stop.load(Ordering::Acquire)
                || state.reboot_required
                || !state.mounted
                || self.shared.poisoned.load(Ordering::Acquire)
            {
                return Err(MemoryError::Unavailable);
            }
            if self.shared.busy.load(Ordering::Acquire) {
                return Err(MemoryError::Busy);
            }
            state.operation = Some(operation.name());
            state.started = Some(Instant::now());
            state.sequence = state.sequence.saturating_add(1);
            state.phase = "working";
            state.last_result = Value::Null;
            state.fault = None;
            self.shared.busy.store(true, Ordering::Release);
            if self
                .requests
                .try_send(Request::Memory(Box::new(MemoryRequest {
                    operation,
                    reply: tx,
                })))
                .is_err()
            {
                self.shared.busy.store(false, Ordering::Release);
                state.phase = "worker_unavailable";
                state.reboot_required = true;
                return Err(MemoryError::Unavailable);
            }
        }
        match rx.recv_timeout(MEMORY_WAIT) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                // The sole SD owner still owns every pointer. An uncertain write
                // may complete later; do not issue another mutation or free it.
                self.shared.poisoned.store(true, Ordering::Release);
                if let Ok(mut state) = self.shared.snapshot.lock() {
                    state.fault = Some(Fault {
                        stage: "memory_client_deadline",
                        code: sys::ESP_ERR_TIMEOUT,
                    });
                    state.phase = "operation_timed_out";
                }
                if let Ok(mut memory) = self.shared.memory.lock() {
                    memory.timeouts = memory.timeouts.saturating_add(1);
                    memory.failures = memory.failures.saturating_add(1);
                    memory.last_error = Some(MemoryError::Timeout);
                }
                Err(MemoryError::Timeout)
            }
            Err(RecvTimeoutError::Disconnected) => Err(MemoryError::Unavailable),
        }
    }
}
fn run_memory(shared: &Shared, mounted: bool, request: MemoryRequest) {
    let name = request.operation.name();
    let result = if !mounted || shared.poisoned.load(Ordering::Acquire) {
        Err(MemoryError::Unavailable)
    } else {
        match request.operation {
            MemoryOperation::Material(op) => media_files::execute(
                Path::new(APP_DIRECTORY),
                *op,
                image_digest::ImageDigest::new,
            )
            .map(|r| MemoryResponse::Material(Box::new(r))),
            MemoryOperation::Load(key) => {
                memory_files::load(Path::new(APP_DIRECTORY), &key).map(MemoryResponse::Loaded)
            }
            MemoryOperation::Save(expected, data) => {
                memory_files::save(Path::new(APP_DIRECTORY), expected, &data)
                    .map(MemoryResponse::Revision)
            }
            MemoryOperation::Reset(key, keep) => {
                history_files::clear(Path::new(APP_DIRECTORY), &key)
                    .and_then(|()| memory_files::reset(Path::new(APP_DIRECTORY), &key, keep))
                    .map(MemoryResponse::Revision)
            }
            MemoryOperation::Archive(key, revision, turn) => {
                history_files::append(Path::new(APP_DIRECTORY), &key, revision, &turn)
                    .map(|()| MemoryResponse::Archived)
            }
            MemoryOperation::SearchHistory(key, query) => {
                history_files::search(Path::new(APP_DIRECTORY), &key, &query)
                    .map(MemoryResponse::History)
            }
        }
    };
    if name != "image_material" {
        if let Ok(mut memory) = shared.memory.lock() {
            match &result {
                Ok(value) => {
                    match name {
                        "memory_load" => memory.loads = memory.loads.saturating_add(1),
                        "memory_save" => memory.saves = memory.saves.saturating_add(1),
                        "memory_reset" => memory.resets = memory.resets.saturating_add(1),
                        "history_append" => {
                            memory.history_appends = memory.history_appends.saturating_add(1)
                        }
                        "history_search" => {
                            memory.history_searches = memory.history_searches.saturating_add(1)
                        }
                        _ => (),
                    }
                    memory.last_revision = match value {
                        MemoryResponse::Loaded(v) => v.revision,
                        MemoryResponse::Revision(v) => *v,
                        MemoryResponse::Archived
                        | MemoryResponse::History(_)
                        | MemoryResponse::Material(_) => memory.last_revision,
                    };
                    // A completed late operation does not remove its timeout warning.
                    if !shared.poisoned.load(Ordering::Acquire) {
                        memory.last_error = None;
                    }
                }
                Err(error) => {
                    memory.failures = memory.failures.saturating_add(1);
                    memory.last_error = Some(*error);
                }
            }
        }
    }
    if let Ok(mut state) = shared.snapshot.lock() {
        state.last_result = match &result {
            Ok(_) => json!({"memory_operation":name,"ok":true}),
            Err(error) => json!({"memory_operation":name,"ok":false,"error":error}),
        };
        if let Err(MemoryError::Io { stage, code }) = &result {
            state.fault = Some(Fault { stage, code: *code });
            shared.poisoned.store(true, Ordering::Release);
        }
        if state
            .started
            .is_some_and(|v| v.elapsed() >= OPERATION_DEADLINE)
        {
            state.fault = Some(Fault {
                stage: "worker_operation_deadline",
                code: sys::ESP_ERR_TIMEOUT,
            });
            shared.poisoned.store(true, Ordering::Release);
        }
        state.phase = if shared.poisoned.load(Ordering::Acquire) {
            "disabled_after_failure"
        } else {
            "available"
        };
        state.started = None;
    }
    // Publish readiness before sending the acknowledgment, so a following call
    // cannot observe a spurious busy state after a successful completed write.
    shared.busy.store(false, Ordering::Release);
    let _ = request.reply.try_send(result);
}
