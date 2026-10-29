//! Ordinary QQ device status must never forward private management results.
pub fn qq_status(mut status: serde_json::Value) -> serde_json::Value {
    if let Some(services) = status.get_mut("services").and_then(|v| v.as_object_mut()) {
        services.remove("diagnostic_result");
    }
    status
}
#[cfg(test)]
mod tests {
    #[test]
    fn private_memory_preview_is_not_forwarded_to_other_qq_contexts() {
        let status = serde_json::json!({"services":{"diagnostic_result":{"summary":"private-owner-summary","facts":["private-fact"]},"memory":{"saves":1},"bot":{"gateway_ready":true}},"led":{"gpio":48}});
        let public = super::qq_status(status).to_string();
        assert!(!public.contains("private-owner-summary"));
        assert!(!public.contains("private-fact"));
        assert!(public.contains("gateway_ready"));
        assert!(public.contains("gpio"));
    }
}
