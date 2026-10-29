//! Main-loop bridge for the authenticated web manager. No SDK handle crosses tasks.
use crate::{
    platform::AppResult,
    web::{WebAction, WebCommand, WebServer},
    Device,
};
use qq_deepseek_core::Config;
use serde_json::{json, Value};
use std::{
    sync::{
        mpsc::{sync_channel, Receiver, SyncSender},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

pub struct Management {
    server: Option<WebServer>,
    token: Option<String>,
    tx: SyncSender<WebCommand>,
    rx: Receiver<WebCommand>,
    snapshot: Arc<Mutex<Value>>,
    next_snapshot: Instant,
    next_start: Instant,
    start_attempts: u8,
}
impl Management {
    pub fn new() -> Self {
        let (tx, rx) = sync_channel(4);
        Self {
            server: None,
            token: None,
            tx,
            rx,
            snapshot: Arc::new(Mutex::new(Value::Null)),
            next_snapshot: Instant::now(),
            next_start: Instant::now(),
            start_attempts: 0,
        }
    }
    pub fn poll(&mut self, device: &mut Device) {
        let online = device.network.as_ref().is_some_and(|n| n.online());
        if self.server.is_none()
            && online
            && self.start_attempts < 3
            && Instant::now() >= self.next_start
        {
            self.start_attempts += 1;
            self.next_start = Instant::now() + Duration::from_secs(30);
            let result = device
                .store
                .as_ref()
                .ok_or("NVS unavailable".to_owned())
                .and_then(|s| s.management_token())
                .and_then(|token| {
                    let server =
                        WebServer::start(self.tx.clone(), self.snapshot.clone(), token.clone())?;
                    self.token = Some(token);
                    self.server = Some(server);
                    Ok(())
                });
            device.web_status = json!({"enabled":self.server.is_some(),"token_configured":self.token.is_some(),"fault":result.err()});
            device.web_pairing_token = self.token.clone();
        }
        for _ in 0..2 {
            let Ok(command) = self.rx.try_recv() else {
                break;
            };
            let result = match command.action {
                WebAction::Command(text) => device.execute_line(&text),
                WebAction::ConfigPatch {
                    expected_generation,
                    patch,
                } => Self::configure(device, expected_generation, &patch),
            };
            let _ = command.respond_to.try_send(result);
            self.next_snapshot = Instant::now();
        }
        if Instant::now() >= self.next_snapshot {
            self.next_snapshot = Instant::now() + Duration::from_millis(500);
            let value = json!({"status":device.status(),"config":device.store.as_ref().and_then(|s|s.current()).map(|c|public_config(&c))});
            if let Ok(mut state) = self.snapshot.lock() {
                *state = value;
            }
        }
    }
    fn configure(device: &mut Device, generation: u32, patch: &Value) -> AppResult<Value> {
        #[cfg(feature = "bot")]
        if device.services.busy_for_configuration() {
            return Err("请先停止机器人，并等待当前操作完成后保存配置".into());
        }
        let store = device.store.as_mut().ok_or("NVS unavailable")?;
        let current = store.current().ok_or("configuration required")?;
        let next = qq_deepseek_core::config_patch::apply(&current, generation, patch)
            .map_err(str::to_owned)?;
        if serde_json::to_vec(&next).map_err(|_| "配置编码失败")?
            == serde_json::to_vec(current.as_ref()).map_err(|_| "配置编码失败")?
        {
            return Ok(json!({"persisted":true,"unchanged":true,"generation":current.generation}));
        }
        let wifi_changed =
            next.wifi_ssid != current.wifi_ssid || next.wifi_password != current.wifi_password;
        let status = store.import(next)?;
        if wifi_changed {
            device.reconfigure_network = Some(Instant::now() + Duration::from_secs(2));
        }
        Ok(
            json!({"persisted":true,"nvs":status,"wifi_reconnect_pending":wifi_changed,"message":"配置已保存；需要时重新启动机器人"}),
        )
    }
}
fn public_config(config: &Config) -> Value {
    json!({"generation":config.generation,"wifi_ssid":config.wifi_ssid,"deepseek_model":config.deepseek_model,
        "qq_app_id":config.qq_app_id,"allow_users":config.allow_users,"allow_groups":config.allow_groups,
        "wifi_password_present":!config.wifi_password.is_empty(),"deepseek_configured":config.deepseek_ready(),"qq_configured":config.qq_ready()})
}
