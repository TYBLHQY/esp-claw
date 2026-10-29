//! Administrative edits retain omitted/empty credentials and reject stale forms.
use crate::Config;
use serde_json::Value;

pub fn apply(
    current: &Config,
    generation: u32,
    patch: &Value,
) -> Result<Box<Config>, &'static str> {
    if generation != current.generation {
        return Err("配置已发生变化，请重新读取后再保存");
    }
    let fields = patch.as_object().ok_or("配置必须是对象")?;
    let mut value = serde_json::to_value(current).map_err(|_| "无法读取当前配置")?;
    for (key, field) in fields {
        match key.as_str() {
            "wifi_password" | "deepseek_key" | "qq_app_secret" => {
                let text = field.as_str().ok_or("密钥字段必须是文本")?;
                if text.is_empty() {
                    continue;
                }
            }
            "wifi_ssid" | "deepseek_model" | "qq_app_id" => {
                if !field.is_string() {
                    return Err("配置字段必须是文本");
                }
            }
            "allow_users" | "allow_groups" => {
                if !field.is_array() {
                    return Err("允许名单必须是数组");
                }
            }
            _ => return Err("配置中含有未开放字段"),
        }
        value[key] = field.clone();
    }
    let bytes = serde_json::to_vec(&value).map_err(|_| "配置编码失败")?;
    Config::from_json_boxed(&bytes).map_err(|_| "配置字段、长度或格式不正确")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn config() -> Config {
        Config::from_json(&serde_json::to_vec(&json!({"version":1,"generation":3,"wifi_ssid":"network","wifi_password":"fake-password","deepseek_key":"fake-key","deepseek_model":"model","qq_app_id":"app","qq_app_secret":"fake-secret","allow_users":["owner"],"allow_groups":[]})).unwrap()).unwrap()
    }
    #[test]
    fn empty_secrets_retain_values_and_regular_fields_change() {
        let c = config();
        let p=apply(&c,3,&json!({"wifi_password":"","deepseek_key":"","qq_app_secret":"","deepseek_model":"new-model"})).unwrap();
        assert_eq!(p.wifi_password, c.wifi_password);
        assert_eq!(p.deepseek_key, c.deepseek_key);
        assert_eq!(p.qq_app_secret, c.qq_app_secret);
        assert_eq!(p.deepseek_model, "new-model");
    }
    #[test]
    fn rejects_stale_forms_and_readonly_fields() {
        let c = config();
        assert!(apply(&c, 2, &json!({})).is_err());
        assert!(apply(&c, 3, &json!({"generation":99})).is_err());
        assert!(apply(&c, 3, &json!({"version":2})).is_err());
    }
    #[test]
    fn validates_merged_config_and_value_types() {
        let c = config();
        assert!(apply(&c, 3, &json!({"wifi_password":"short"})).is_err());
        assert!(apply(&c, 3, &json!({"wifi_password":null})).is_err());
        assert!(apply(&c, 3, &json!({"allow_users":[42]})).is_err());
        let p = apply(
            &c,
            3,
            &json!({"allow_groups":["group"],"wifi_password":"another-fake-pass"}),
        )
        .unwrap();
        assert_eq!(p.allow_groups, ["group"]);
    }
}
