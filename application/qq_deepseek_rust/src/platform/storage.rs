//! Two validated slots: SDK set_blob erases its target key before replacing it.
//! Therefore only the INACTIVE slot may be written; the active copy stays intact.
use esp_idf_svc::nvs::{EspDefaultNvs, EspDefaultNvsPartition};
use qq_deepseek_core::{
    record::{decode_config, encode_config, MAX_RECORD_BYTES},
    Config,
};
use serde_json::{json, Value};
use std::sync::Arc;

use super::AppResult;

const NAMESPACE: &str = "qqds_v1";
const SLOTS: [&str; 2] = ["cfg0", "cfg1"];
const PROBE_KEY: &str = "probe";
const BOT_MODE_KEY: &str = "bot_mode";
const MANAGEMENT_KEY: &str = "admin_key";
const MAX_BOT_MODE: u8 = 2;

pub struct Store {
    nvs: EspDefaultNvs,
    current: Option<Arc<Config>>,
    active: Option<usize>,
    invalid_slots: usize,
}

impl Store {
    pub fn open() -> AppResult<Self> {
        // Never call take(): that default erases the partition on selected errors.
        let partition = EspDefaultNvsPartition::take_with(false).map_err(|e| e.to_string())?;
        let nvs = EspDefaultNvs::new(partition, NAMESPACE, true).map_err(|e| e.to_string())?;
        let mut this = Self {
            nvs,
            current: None,
            active: None,
            invalid_slots: 0,
        };
        for (index, slot) in SLOTS.iter().enumerate() {
            let len = match this.nvs.blob_len(slot) {
                Ok(Some(len)) => len,
                Ok(None) => continue,
                // A damaged/wrongly typed slot must not hide its valid peer.
                Err(_) => {
                    this.invalid_slots += 1;
                    continue;
                }
            };
            if len > MAX_RECORD_BYTES {
                this.invalid_slots += 1;
                continue;
            }
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(len)
                .map_err(|_| "NVS read allocation failed")?;
            bytes.resize(len, 0);
            let data = match this.nvs.get_blob(slot, &mut bytes) {
                Ok(Some(data)) => data,
                Ok(None) | Err(_) => {
                    bytes.fill(0);
                    this.invalid_slots += 1;
                    continue;
                }
            };
            match decode_config(data) {
                Ok(config) => {
                    let replace = this
                        .current
                        .as_ref()
                        .is_none_or(|old| config.generation > old.generation);
                    if replace {
                        this.current = Some(Arc::from(config));
                        this.active = Some(index);
                    }
                }
                Err(_) => this.invalid_slots += 1,
            }
            bytes.fill(0);
        }
        Ok(this)
    }

    pub fn current(&self) -> Option<Arc<Config>> {
        self.current.clone()
    }

    /// Independent random LAN management credential. Call after Wi-Fi enables
    /// the hardware entropy source. Never include this in general status.
    pub fn management_token(&self) -> AppResult<String> {
        let mut key = [0_u8; 16];
        match self
            .nvs
            .get_blob(MANAGEMENT_KEY, &mut key)
            .map_err(|e| e.to_string())?
        {
            Some(bytes) if bytes.len() == 16 => (),
            Some(_) => return Err("invalid management credential record; data preserved".into()),
            None => {
                unsafe {
                    esp_idf_svc::sys::esp_fill_random(key.as_mut_ptr().cast(), key.len());
                }
                self.nvs
                    .set_blob(MANAGEMENT_KEY, &key)
                    .map_err(|e| e.to_string())?;
                let mut verify = [0_u8; 16];
                if self
                    .nvs
                    .get_blob(MANAGEMENT_KEY, &mut verify)
                    .map_err(|e| e.to_string())?
                    != Some(key.as_slice())
                {
                    return Err("management credential NVS readback mismatch".into());
                }
            }
        }
        let token = key.iter().map(|byte| format!("{byte:02x}")).collect();
        key.fill(0);
        Ok(token)
    }

    /// Persisted user intent, separate from the versioned credential records.
    /// Missing means off; an invalid stored value is reported, never promoted
    /// to an enabled mode. 0 = off, 1 = echo, 2 = DeepSeek.
    pub fn bot_mode(&self) -> AppResult<u8> {
        let mode = self
            .nvs
            .get_u8(BOT_MODE_KEY)
            .map_err(|e| e.to_string())?
            .unwrap_or(0);
        if mode > MAX_BOT_MODE {
            return Err("invalid persisted bot mode".into());
        }
        Ok(mode)
    }

    pub fn set_bot_mode(&self, mode: u8) -> AppResult<()> {
        if mode > MAX_BOT_MODE {
            return Err("invalid bot mode; expected 0, 1, or 2".into());
        }
        self.nvs
            .set_u8(BOT_MODE_KEY, mode)
            .map_err(|e| e.to_string())?;
        if self.nvs.get_u8(BOT_MODE_KEY).map_err(|e| e.to_string())? != Some(mode) {
            return Err("bot mode NVS readback mismatch".into());
        }
        Ok(())
    }

    pub fn import(&mut self, mut config: Box<Config>) -> AppResult<Value> {
        config.validate().map_err(|e| e.to_string())?;
        config.generation = self
            .current
            .as_ref()
            .map_or(Some(1), |c| c.generation.checked_add(1))
            .ok_or("config generation exhausted")?;
        let target = match self.active {
            Some(0) => 1,
            _ => 0,
        };
        let mut bytes = encode_config(&config).map_err(|e| e.to_string())?;
        let result = self
            .nvs
            .set_blob(SLOTS[target], &bytes)
            .map_err(|e| e.to_string());
        if let Err(error) = result {
            bytes.fill(0);
            return Err(error);
        }
        let mut readback = Vec::new();
        readback
            .try_reserve_exact(bytes.len())
            .map_err(|_| "NVS verify allocation failed")?;
        readback.resize(bytes.len(), 0);
        let check = self
            .nvs
            .get_blob(SLOTS[target], &mut readback)
            .map_err(|e| e.to_string())?
            .ok_or("NVS readback missing")?;
        if check != bytes {
            bytes.fill(0);
            readback.fill(0);
            return Err("NVS readback mismatch".into());
        }
        decode_config(check).map_err(|e| e.to_string())?;
        bytes.fill(0);
        readback.fill(0);
        self.current = Some(Arc::from(config));
        self.active = Some(target);
        Ok(self.status())
    }

    pub fn test(&self) -> AppResult<Value> {
        let old = self
            .nvs
            .get_u64(PROBE_KEY)
            .map_err(|e| e.to_string())?
            .unwrap_or(0);
        let sequence = ((old >> 32) as u32)
            .checked_add(1)
            .ok_or("probe sequence exhausted")?;
        let nonce = unsafe { esp_idf_svc::sys::esp_random() };
        let expected = (u64::from(sequence) << 32) | u64::from(nonce);
        self.nvs
            .set_u64(PROBE_KEY, expected)
            .map_err(|e| e.to_string())?;
        let actual = self.nvs.get_u64(PROBE_KEY).map_err(|e| e.to_string())?;
        if actual != Some(expected) {
            return Err("NVS test readback mismatch".into());
        }
        Ok(
            json!({"write_readback_ok":true,"sequence":sequence,"nonce":nonce,
            "reboot_persistence": "verify_same_value_after_reboot"}),
        )
    }

    pub fn status(&self) -> Value {
        let probe = self.nvs.get_u64(PROBE_KEY);
        json!({"namespace": NAMESPACE, "automatic_erase": false,
            "active_slot": self.active, "invalid_slots_at_boot": self.invalid_slots,
            "bot_mode": match self.bot_mode() { Ok(mode) => json!(mode),
                Err(_) => json!({"error":"bot_mode_read_failed"}) },
            "config": self.current.as_ref().map(|c| c.status()),
            "probe": match probe { Ok(Some(v)) => json!({"sequence":v >> 32,"nonce":v as u32}),
                Ok(None) => Value::Null, Err(_) => json!({"error":"probe_read_failed"}) }})
    }
}
