//! One physical WS2812. The RMT channel and its payload have exactly one owner.
use esp_idf_svc::sys::*;
use qq_deepseek_core::command::Rgb;
use serde_json::{json, Value};
use std::ptr;

use super::{check, AppResult};

const GPIO: i32 = 48;
const RESOLUTION_HZ: u32 = 10_000_000;
const SYMBOLS: usize = 25; // 24 GRB bits and a 300 us reset/latch symbol.
const TX_TIMEOUT_MS: i32 = 100;

pub struct Led {
    channel: rmt_channel_handle_t,
    encoder: rmt_encoder_handle_t,
    words: Vec<u32>,
    applied: Rgb,
    requested: Rgb,
    fault: Option<String>,
    poisoned: bool,
}

impl Led {
    pub fn new() -> AppResult<Self> {
        // No other module initializes GPIO48 or creates an RMT driver.
        let config = rmt_tx_channel_config_t {
            gpio_num: GPIO,
            clk_src: soc_periph_rmt_clk_src_t_RMT_CLK_SRC_DEFAULT,
            resolution_hz: RESOLUTION_HZ,
            mem_block_symbols: 64,
            trans_queue_depth: 1,
            ..Default::default()
        };
        let mut channel = ptr::null_mut();
        // SAFETY: the SDK copies config; channel is exclusively owned below.
        check(
            unsafe { rmt_new_tx_channel(&config, &mut channel) },
            "rmt channel",
        )?;
        let mut encoder = ptr::null_mut();
        let encoder_config = rmt_copy_encoder_config_t::default();
        let code = unsafe { rmt_new_copy_encoder(&encoder_config, &mut encoder) };
        if let Err(err) = check(code, "rmt encoder") {
            unsafe { rmt_del_channel(channel) };
            return Err(err);
        }
        if let Err(err) = check(unsafe { rmt_enable(channel) }, "rmt enable") {
            unsafe {
                rmt_del_encoder(encoder);
                rmt_del_channel(channel);
            }
            return Err(err);
        }
        let mut words = Vec::new();
        if words.try_reserve_exact(SYMBOLS).is_err() {
            unsafe {
                rmt_disable(channel);
                rmt_del_encoder(encoder);
                rmt_del_channel(channel);
            }
            return Err("LED buffer allocation failed".into());
        }
        words.resize(SYMBOLS, 0);
        let off = Rgb { r: 0, g: 0, b: 0 };
        let mut this = Self {
            channel,
            encoder,
            words,
            applied: off,
            requested: off,
            fault: None,
            poisoned: false,
        };
        this.set(off)?;
        Ok(this)
    }

    pub fn set(&mut self, color: Rgb) -> AppResult<()> {
        if self.poisoned {
            return Err("RMT fault is latched; reboot required before reusing its payload".into());
        }
        Rgb::try_new(color.r, color.g, color.b).map_err(|e| e.to_string())?;
        self.requested = color;
        let grb = (u32::from(color.g) << 16) | (u32::from(color.r) << 8) | u32::from(color.b);
        for bit in 0..24 {
            let one = grb & (1 << (23 - bit)) != 0;
            let (high, low) = if one { (9_u32, 3_u32) } else { (3, 9) };
            // rmt_symbol_word_t: duration0:15, level0:1, duration1:15, level1:1.
            self.words[bit] = high | (1 << 15) | (low << 16);
        }
        self.words[24] = 1500 | (1500 << 16);
        let mut tx_config = rmt_transmit_config_t::default();
        tx_config.loop_count = 0;
        // SAFETY: words is heap-owned and cannot move/reallocate until wait_all_done.
        // On timeout it is never mutated again; Drop disables the channel first.
        let result = check(
            unsafe {
                rmt_transmit(
                    self.channel,
                    self.encoder,
                    self.words.as_ptr().cast(),
                    self.words.len() * size_of::<u32>(),
                    &tx_config,
                )
            },
            "rmt transmit",
        )
        .and_then(|_| {
            check(
                unsafe { rmt_tx_wait_all_done(self.channel, TX_TIMEOUT_MS) },
                "rmt wait",
            )
        });
        match result {
            Ok(()) => {
                self.applied = color;
                self.fault = None;
                Ok(())
            }
            Err(e) => {
                self.poisoned = true;
                self.fault = Some(e.clone());
                Err(e)
            }
        }
    }

    pub fn status(&self) -> Value {
        json!({"gpio": GPIO, "type": "WS2812", "order": "GRB",
            "requested": [self.requested.r,self.requested.g,self.requested.b],
            "driver_applied": [self.applied.r,self.applied.g,self.applied.b],
            "physical_observation": "not_measured", "fault": self.fault})
    }
}

impl Drop for Led {
    fn drop(&mut self) {
        // An ISR must never retain a reference to freed payload/encoder storage.
        if unsafe { rmt_disable(self.channel) } == ESP_OK {
            unsafe {
                rmt_del_encoder(self.encoder);
                rmt_del_channel(self.channel);
            }
        } else {
            // Fail safe: retain SDK resources and the payload if disable failed.
            std::mem::forget(std::mem::take(&mut self.words));
        }
    }
}
