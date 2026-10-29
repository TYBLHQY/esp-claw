#![allow(dead_code)]
// Production inference/cache code with deterministic HTTP and SD-client mocks.
// No socket, serial port, credential fixture, or device SDK is accessed.
extern crate self as esp_idf_svc;
pub mod http {
    #[derive(Clone, Copy)]
    pub enum Method {
        Get,
        Post,
    }
}
pub mod platform {
    pub mod sdcard {
        use qq_deepseek_core::memory::*;
        use std::{
            collections::HashMap,
            sync::{
                atomic::{AtomicBool, Ordering},
                Arc, Mutex,
            },
        };
        #[derive(Clone, Default)]
        pub struct MemoryClient(
            pub Arc<Mutex<HashMap<String, (u64, SessionMemory)>>>,
            pub Arc<AtomicBool>,
            pub Arc<Mutex<HashMap<String, Vec<qq_deepseek_core::history::ArchivedTurn>>>>,
        );
        impl MemoryClient {
            pub fn load(&self, key: &str) -> Result<LoadedMemory, MemoryError> {
                if self.1.swap(false, Ordering::Relaxed) {
                    return Err(MemoryError::Unavailable);
                }
                let map = self.0.lock().unwrap();
                Ok(match map.get(key) {
                    Some((r, d)) => LoadedMemory {
                        revision: *r,
                        data: Some(d.clone()),
                    },
                    None => LoadedMemory {
                        revision: 0,
                        data: None,
                    },
                })
            }
            pub fn save(&self, r: u64, d: SessionMemory) -> Result<u64, MemoryError> {
                d.validate()?;
                let mut map = self.0.lock().unwrap();
                if map.get(&d.key).map_or(0, |v| v.0) != r {
                    return Err(MemoryError::Conflict);
                }
                map.insert(d.key.clone(), (r + 1, d));
                Ok(r + 1)
            }
            pub fn archive(
                &self,
                key: &str,
                _revision: u64,
                turn: &MemoryTurn,
            ) -> Result<(), MemoryError> {
                let mut h = self.2.lock().unwrap();
                let rows = h.entry(key.into()).or_default();
                let seq = rows.last().map_or(1, |r| r.sequence + 1);
                let wire = qq_deepseek_core::history::encode(key, seq, 1000, turn)?;
                rows.push(qq_deepseek_core::history::decode(&wire, key)?);
                if rows.len() > 32 {
                    rows.remove(0);
                }
                Ok(())
            }
            pub fn search_history(
                &self,
                key: &str,
                query: &str,
            ) -> Result<qq_deepseek_core::history::SearchResult, MemoryError> {
                let mut rank = qq_deepseek_core::history::Ranker::new(query)?;
                if let Some(rows) = self.2.lock().unwrap().get(key) {
                    for row in rows {
                        rank.consider(row.clone());
                    }
                }
                Ok(rank.finish())
            }
            pub fn reset(&self, key: &str, keep: bool) -> Result<u64, MemoryError> {
                let mut map = self.0.lock().unwrap();
                let (r, mut d) = map
                    .get(key)
                    .cloned()
                    .unwrap_or((0, SessionMemory::empty(key)));
                d.summary.clear();
                d.turns.clear();
                d.summarized_turns = 0;
                if !keep {
                    d.facts.clear();
                }
                self.2.lock().unwrap().remove(key);
                map.insert(key.into(), (r + 2, d));
                Ok(r + 2)
            }
        }
    }
}

#[path = "support/inference_runtime_bot.rs"]
mod bot;
#[cfg(test)]
mod tests {
    use super::*;
    use bot::{http as fake, inference};
    use platform::sdcard::MemoryClient;
    use qq_deepseek_core::{
        memory::{MemoryTurn, SessionMemory},
        Config, CONFIG_VERSION,
    };
    use std::sync::{atomic::Ordering, Arc, Mutex};
    static TEST_LOCK: Mutex<()> = Mutex::new(());
    fn cfg() -> Arc<Config> {
        Arc::new(Config {
            version: CONFIG_VERSION,
            generation: 1,
            wifi_ssid: "fixture".into(),
            wifi_password: "fixture-wifi-pass".into(),
            deepseek_key: "fixture-ds-key".into(),
            deepseek_model: "deepseek-v4-flash".into(),
            qq_app_id: "fixture-app".into(),
            qq_app_secret: "fixture-qq-secret".into(),
            allow_users: vec!["fixture-user".into()],
            allow_groups: vec![],
        })
    }
    #[test]
    fn persisted_history_and_redaction_use_production_pipeline() {
        let _guard = TEST_LOCK.lock().unwrap();
        fake::clear();
        let c = MemoryClient::default();
        fake::push("Je suis CopilotLee.");
        let v = inference::memory_ask(
            cfg(),
            Some(c.clone()),
            "u:host",
            "Bonjour, je m'appelle Élodie.",
        )
        .unwrap();
        assert_eq!(v["memory"]["persisted"], true);
        fake::push("Élodie.");
        inference::memory_ask(cfg(), Some(c.clone()), "u:host", "Quel est mon nom ?").unwrap();
        let requests = fake::requests();
        assert!(requests[1].to_string().contains("Élodie"));
        assert!(requests[1]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("CopilotLee"));
        fake::push("fixture-ds-key fixture-qq-secret fixture-wifi-pass");
        inference::memory_ask(
            cfg(),
            Some(c.clone()),
            "u:host",
            "Please retain fixture-ds-key.",
        )
        .unwrap();
        let data = c.load("u:host").unwrap().data.unwrap();
        assert!(!serde_json::to_string(&data)
            .unwrap()
            .contains("fixture-ds-key"));
    }
    fn seeded(n: usize) -> MemoryClient {
        let c = MemoryClient::default();
        let mut d = SessionMemory::empty("u:host");
        for i in 0..n {
            d.turns.push(MemoryTurn {
                user: format!("User {i}"),
                assistant: format!("Answer {i}"),
            });
        }
        c.save(0, d).unwrap();
        c
    }
    #[test]
    fn failed_summary_keeps_six_turns_but_refuses_full_eight() {
        let _guard = TEST_LOCK.lock().unwrap();
        fake::clear();
        let c = seeded(6);
        fake::raw("invalid SSE");
        fake::push("A normal answer.");
        let v =
            inference::memory_ask(cfg(), Some(c.clone()), "u:host", "Another question.").unwrap();
        assert_eq!(v["memory"]["turn_count"], 7);
        let d = c.load("u:host").unwrap().data.unwrap();
        assert!(d.summary.is_empty());
        assert_eq!(d.turns[0].user, "User 0");
        fake::clear();
        let c = seeded(8);
        fake::raw("invalid SSE");
        let e = inference::memory_ask(cfg(), Some(c.clone()), "u:host", "Another question.")
            .unwrap_err();
        assert_eq!(e.label(), "memory_context_capacity");
        assert_eq!(c.load("u:host").unwrap().data.unwrap().turns.len(), 8);
    }
    #[test]
    fn successful_summary_removes_only_oldest_completed_prefix() {
        let _guard = TEST_LOCK.lock().unwrap();
        fake::clear();
        let c = seeded(6);
        fake::push("User has discussed six numbered topics; recent questions remain pending.");
        fake::push("New answer.");
        let v = inference::memory_ask(cfg(), Some(c.clone()), "u:host", "New question.").unwrap();
        assert_eq!(v["memory"]["summarized_turns"], 4);
        assert_eq!(v["memory"]["turn_count"], 3);
        let d = c.load("u:host").unwrap().data.unwrap();
        assert_eq!(d.turns[0].user, "User 4");
        let requests = fake::requests();
        assert_eq!(requests[0]["max_tokens"], 768);
        assert_eq!(requests[0]["messages"][1]["role"], "user");
    }
    #[test]
    fn temporary_mutation_failure_is_explicit_and_forget_mirrors() {
        let _guard = TEST_LOCK.lock().unwrap();
        fake::clear();
        assert!(inference::memory_diagnostic(cfg(), None, "u:host", "remember Rust").is_err());
        let c = MemoryClient::default();
        inference::memory_diagnostic(cfg(), Some(c.clone()), "u:host", "remember Rust").unwrap();
        let prior = c.load("u:host").unwrap().revision;
        inference::memory_diagnostic(cfg(), Some(c.clone()), "u:host", "forget 1").unwrap();
        let after = c.load("u:host").unwrap();
        assert_eq!(after.revision, prior + 2);
        assert!(after.data.unwrap().facts.is_empty());
    }
    #[test]
    fn vision_probe_uses_metadata_and_actual_image_without_expected_marker() {
        let _guard = TEST_LOCK.lock().unwrap();
        fake::clear();
        fake::raw(r#"{"data":[{"id":"deepseek-flash","input_modalities":["text","image"]}]}"#);
        fake::push("RANDOM_VISIBLE_VALUE");
        let result = inference::test_vision(cfg(), "https://example.com/fixture.png").unwrap();
        assert_eq!(result, "RANDOM_VISIBLE_VALUE");
        let request = fake::requests().pop().unwrap();
        assert!(!request.to_string().contains("RANDOM_VISIBLE_VALUE"));
        assert_eq!(
            request["messages"][1]["content"][1]["image_url"]["detail"],
            "auto"
        );
        fake::clear();
        fake::raw(r#"{"data":[{"id":"deepseek-flash","input_modalities":["text"]}]}"#);
        assert!(inference::test_vision(cfg(), "https://example.com/fixture.png").is_err());
        assert!(fake::requests().is_empty());
    }
    #[test]
    fn remember_encoded_capacity_failure_keeps_valid_working_copy() {
        let client = MemoryClient::default();
        let mut original = SessionMemory::empty("u:host");
        original.summary = "\0".repeat(qq_deepseek_core::memory::MAX_SUMMARY_BYTES);
        let rejected = loop {
            let fact = format!("{}{}", original.facts.len(), "\0".repeat(511));
            let mut next = original.clone();
            next.facts.push(fact.clone());
            if next.validate().is_err() {
                break fact;
            }
            original = next;
        };
        client.save(0, original.clone()).unwrap();
        let stats = bot::memory::Stats::default();
        let mut cache = bot::memory::Cache::new(Some(client));
        let mut entry = cache.take("u:host", &stats);
        let result = bot::memory::simple_command(
            &cache,
            &mut entry,
            &format!("/remember {rejected}"),
            &stats,
        );
        assert!(matches!(result, Some(Err(_))));
        assert_eq!(entry.data, original);
        assert!(!entry.dirty);
        assert!(entry.data.validate().is_ok());
    }
    #[test]
    fn reset_with_unknown_disk_facts_preserves_authoritative_disk_values() {
        let client = MemoryClient::default();
        let mut original = SessionMemory::empty("u:host");
        original.facts.push("Disk-only fact".into());
        client.save(0, original).unwrap();
        client.1.store(true, Ordering::Relaxed);
        let stats = bot::memory::Stats::default();
        let mut cache = bot::memory::Cache::new(Some(client.clone()));
        let mut entry = cache.take("u:host", &stats);
        assert!(entry.data.facts.is_empty());
        entry.data.turns.push(MemoryTurn {
            user: "Offline question".into(),
            assistant: "Offline answer".into(),
        });
        entry.dirty = true;
        cache.reset(&mut entry, true, &stats);
        assert_eq!(entry.data.facts, ["Disk-only fact"]);
        assert!(entry.data.turns.is_empty());
        assert!(bot::memory::persisted(&entry));
        // Known loaded facts modified offline are not discarded either.
        entry.data.facts.push("Known local fact".into());
        entry.dirty = true;
        cache.reset(&mut entry, true, &stats);
        assert_eq!(entry.data.facts, ["Disk-only fact", "Known local fact"]);
        assert_eq!(
            client.load("u:host").unwrap().data.unwrap().facts,
            entry.data.facts
        );
    }
    fn tool(name: &str, args: serde_json::Value) {
        let chunk = serde_json::json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_fixture","type":"function","function":{"name":name,"arguments":args.to_string()}}]},"finish_reason":"tool_calls"}]});
        fake::raw(&format!("data: {chunk}\n\ndata: [DONE]\n\n"));
    }
    #[test]
    fn natural_search_fetch_budget_and_sources_use_production_loop() {
        let _guard = TEST_LOCK.lock().unwrap();
        fake::clear();
        tool("search_web", serde_json::json!({"query":"example domain"}));
        tool(
            "fetch_page",
            serde_json::json!({"url":"https://example.com/"}),
        );
        fake::push("Here is the fixture summary [1].");
        let r = inference::memory_ask(
            cfg(),
            Some(MemoryClient::default()),
            "u:host",
            "Please look up Example Domain and read the source page.",
        )
        .unwrap();
        assert!(r["answer"]
            .as_str()
            .unwrap()
            .contains("https://example.com/"));
        let requests = fake::requests();
        assert_eq!(requests.len(), 3);
        assert!(
            requests[0]["tools"][0]["function"]["parameters"]["properties"]["query"].is_object()
        );
        assert_eq!(requests[2]["tool_choice"], "none");
        assert_eq!(
            requests[1]["messages"].as_array().unwrap().last().unwrap()["role"],
            "tool"
        );
    }
    #[test]
    fn recall_uses_only_current_session_and_never_stores_tool_wrappers() {
        let _guard = TEST_LOCK.lock().unwrap();
        fake::clear();
        let c = MemoryClient::default();
        c.archive(
            "u:host",
            1,
            &MemoryTurn {
                user: "My older comet was Cedar-821".into(),
                assistant: "Noted".into(),
            },
        )
        .unwrap();
        c.archive(
            "g:group:host",
            1,
            &MemoryTurn {
                user: "Group secret Cedar-999".into(),
                assistant: "Noted".into(),
            },
        )
        .unwrap();
        tool("recall_history", serde_json::json!({"query":"Cedar"}));
        fake::push("Your earlier comet was Cedar-821.");
        inference::memory_ask(
            cfg(),
            Some(c.clone()),
            "u:host",
            "What comet did we discuss before?",
        )
        .unwrap();
        let requests = fake::requests();
        assert!(!requests[0].to_string().contains("Cedar-821"));
        let result = requests[1]["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap();
        assert!(result.contains("Cedar-821"));
        assert!(!result.contains("Cedar-999"));
        assert!(
            !serde_json::to_string(&c.load("u:host").unwrap().data.unwrap())
                .unwrap()
                .contains("untrusted_session_history")
        );
    }
    #[test]
    fn invalid_tool_input_is_data_and_plain_chat_stays_one_request() {
        let _guard = TEST_LOCK.lock().unwrap();
        fake::clear();
        tool(
            "fetch_page",
            serde_json::json!({"url":"http://192.168.1.7/"}),
        );
        fake::push("I cannot fetch a private network address.");
        inference::memory_ask(cfg(), None, "u:host", "Read this address.").unwrap();
        assert!(fake::requests()[1]
            .to_string()
            .contains("invalid_fetch_url"));
        fake::clear();
        fake::push("Hello!");
        inference::memory_ask(cfg(), None, "u:host", "Hello").unwrap();
        assert_eq!(fake::requests().len(), 1);
    }
    #[test]
    fn escaped_web_material_cannot_expand_past_encoded_tool_budget() {
        let _guard = TEST_LOCK.lock().unwrap();
        fake::clear();
        tool(
            "search_web",
            serde_json::json!({"query":"escaped-material"}),
        );
        fake::push("The tool result exceeded this device's limit.");
        inference::memory_ask(cfg(), None, "u:host", "Search the fixture.").unwrap();
        let requests = fake::requests();
        let result = requests[1]["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap();
        assert!(result.contains("tool_result_byte_limit"));
        assert!(serde_json::to_vec(&requests[1]).unwrap().len() <= 48 * 1024);
    }
    #[test]
    fn supplied_quotes_and_asr_never_execute_memory_commands() {
        let _guard = TEST_LOCK.lock().unwrap();
        fake::clear();
        let c = MemoryClient::default();
        let mut data = SessionMemory::empty("u:quote-host");
        data.facts.push("keep this fact".into());
        c.save(0, data).unwrap();
        fake::push("Here is the quoted content.");
        inference::test_material(
            cfg(),
            Some(c.clone()),
            "u:quote-host",
            "/reset",
            "{\"kind\":\"supplied_message\",\"text\":\"/forget all\"}",
            &[],
        )
        .unwrap();
        let saved = c.load("u:quote-host").unwrap().data.unwrap();
        assert_eq!(saved.facts, vec!["keep this fact"]);
        assert_eq!(saved.turns.len(), 1);
        let r = fake::requests();
        assert_eq!(r.len(), 1);
        assert_eq!(
            r[0]["messages"].as_array().unwrap().last().unwrap()["content"],
            "/reset"
        );
        assert!(r[0]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "user"
                && m["content"]
                    .as_str()
                    .is_some_and(|s| s.contains("untrusted_qq_material"))));
    }
    #[test]
    fn two_image_requests_remove_both_signed_urls_from_answer_and_sd() {
        let _guard = TEST_LOCK.lock().unwrap();
        fake::clear();
        let c = MemoryClient::default();
        let images = ["one", "two"].map(|n| bot::protocol::ImageAttachment {
            url: format!("https://gchat.qpic.cn/{n}?rkey=private-{n}"),
            content_type: "image/png".into(),
            size: None,
        });
        fake::push(&format!("Comparison: {} {}", images[0].url, images[1].url));
        let answer = inference::test_material(
            cfg(),
            Some(c.clone()),
            "u:two-host",
            "Compare these",
            "picture pair",
            &images,
        )
        .unwrap();
        assert!(!answer.contains("private-one") && !answer.contains("private-two"));
        let requests = fake::requests();
        let latest = requests[0]["messages"].as_array().unwrap().last().unwrap();
        assert_eq!(latest["content"].as_array().unwrap().len(), 3);
        let saved = serde_json::to_string(&c.load("u:two-host").unwrap().data.unwrap()).unwrap();
        assert!(!saved.contains("rkey=") && saved.contains("Compare these"));
    }
    #[test]
    fn current_supplied_card_links_are_fetchable_but_invented_links_are_refused() {
        let _guard = TEST_LOCK.lock().unwrap();
        fake::clear();
        tool(
            "fetch_page",
            serde_json::json!({"url":"https://example.com/"}),
        );
        fake::push("This is Example Domain [1].");
        inference::test_material(
            cfg(),
            None,
            "u:card-fetch",
            "Read the supplied card.",
            "{\"kind\":\"jump_url\",\"text\":\"https://example.com/\"}",
            &[],
        )
        .unwrap();
        let r = fake::requests();
        assert!(r[1].to_string().contains("untrusted_web_data"));
        assert!(!r[1].to_string().contains("tool_input_not_allowed"));
        fake::clear();
        tool(
            "fetch_page",
            serde_json::json!({"url":"https://example.com/invented"}),
        );
        fake::push("No supplied URL.");
        inference::test_material(
            cfg(),
            None,
            "u:card-fetch",
            "Summarize the card.",
            "{\"kind\":\"title\",\"text\":\"Just a title\"}",
            &[],
        )
        .unwrap();
        assert!(fake::requests()[1]
            .to_string()
            .contains("tool_input_not_allowed"));
    }
    #[test]
    fn streaming_observer_uses_content_only_redacts_secrets_and_commits_complete_turn() {
        let _guard = TEST_LOCK.lock().unwrap();
        fake::clear();
        let c = MemoryClient::default();
        let stats = bot::protocol::streaming::DeliveryStats::default();
        let m = stats.acquire().unwrap();
        let text = format!(
            "{} fixture-ds-key {}",
            "Visible answer. ".repeat(12),
            "useful tail ".repeat(8)
        );
        let json = serde_json::json!({"choices":[{"index":0,"delta":{"content":text,"reasoning_content":"hidden chain of thought"},"finish_reason":"stop"}]});
        fake::raw(&format!("data: {json}\n\ndata: [DONE]\n\n"));
        let answer = inference::test_material_progress(
            cfg(),
            Some(c.clone()),
            "u:stream-host",
            "Explain briefly.",
            "",
            &[],
            Some(&m),
        )
        .unwrap();
        assert!(!answer.contains("fixture-ds-key"));
        assert!(answer.contains("[redacted]"));
        let progress = m.snapshot().unwrap();
        assert!(!progress.text.is_empty());
        assert!(!progress.text.contains("fixture-ds-key"));
        assert!(!progress.text.contains("hidden chain"));
        assert!(answer.starts_with(&progress.text));
        assert_eq!(
            c.load("u:stream-host").unwrap().data.unwrap().turns.len(),
            1
        );
        assert_eq!(fake::requests()[0]["tool_choice"], "auto");
    }
    #[test]
    fn incomplete_stream_never_commits_partial_model_output() {
        let _guard = TEST_LOCK.lock().unwrap();
        fake::clear();
        let c = MemoryClient::default();
        let stats = bot::protocol::streaming::DeliveryStats::default();
        let m = stats.acquire().unwrap();
        let json = serde_json::json!({"choices":[{"index":0,"delta":{"content":"partial ".repeat(30)},"finish_reason":null}]});
        fake::raw(&format!("data: {json}\n\n"));
        assert!(inference::test_material_progress(
            cfg(),
            Some(c.clone()),
            "u:stream-fail",
            "Explain.",
            "",
            &[],
            Some(&m)
        )
        .is_err());
        assert!(c.load("u:stream-fail").unwrap().data.is_none());
    }
    #[test]
    fn plain_private_input_streams_by_default_and_group_delivery_stays_plain() {
        let _guard = TEST_LOCK.lock().unwrap();
        for (target, expected) in [
            (
                bot::protocol::Target::User("fixture".into()),
                vec!["typing", "stream"],
            ),
            (bot::protocol::Target::Group("fixture".into()), vec!["text"]),
        ] {
            fake::clear();
            fake::push("CopilotLee replies normally.");
            let replies = bot::run_one(cfg(), target, "Who are you?");
            assert_eq!(
                replies
                    .iter()
                    .map(|v| v["kind"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                expected
            );
            assert!(replies.last().unwrap()["text"]
                .as_str()
                .unwrap()
                .contains("CopilotLee"));
            assert_eq!(fake::requests()[0]["tool_choice"], "auto");
        }
        fake::clear();
        let replies = bot::run_one(
            cfg(),
            bot::protocol::Target::User("fixture".into()),
            "/memory",
        );
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0]["kind"], "text");
        assert!(fake::requests().is_empty());
    }
    #[test]
    fn default_stream_keeps_natural_search_and_fetch_with_sources() {
        let _guard = TEST_LOCK.lock().unwrap();
        fake::clear();
        tool(
            "search_web",
            serde_json::json!({"query":"ESP32 official documents"}),
        );
        tool(
            "fetch_page",
            serde_json::json!({"url":"https://example.com/"}),
        );
        fake::push("The official documentation provides the details.");
        let replies = bot::run_one(
            cfg(),
            bot::protocol::Target::User("fixture".into()),
            "Search then read the source about ESP32.",
        );
        assert_eq!(replies[1]["kind"], "stream");
        assert_eq!(replies[1]["complete"], true);
        assert_eq!(replies[1]["after_tools"], true);
        assert!(replies[1]["text"]
            .as_str()
            .unwrap()
            .contains("https://example.com/"));
        let requests = fake::requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[2]["tool_choice"], "none");
        assert!(requests[2].to_string().contains("untrusted_web_data"));
    }
}
