# QQ and DeepSeek runtime

This directory implements the application protocol from scratch. It does not depend on the old ESP-Claw agent loop or board manager. It is inactive until the firmware explicitly creates a runtime.

The hardware acceptance build can omit `mod bot`. Enabling the module requires the pinned Espressif `esp_websocket_client` component, with `ws_bindings.h` generated into `esp_idf_svc::sys::ws`. The small WebSocket adapter preserves `payload_offset`, `payload_len`, continuation opcode and FIN before UTF-8 decoding. The released svc 0.53 WebSocket event API does not expose those fields.

## Board bridge

Create a bounded `sync_channel::<bot::ControlRequest>(4)` and pass its sender to:

```rust,ignore
let runtime = bot::BotRuntime::start(
    config_arc,
    control_sender,
    bot::BotMode::EchoOnly,
    optional_sd_memory_client,
)?;
```

The main board loop polls the receiver and executes only `DeviceCommand::Status` or `DeviceCommand::Led(LedCommand)`, then uses `respond_to.try_send(ControlReply { success, text })`. That loop remains the sole RMT owner. Memory uses a bounded client for the separate SD owner; the bot never accesses GPIO or filesystem handles directly. Do not allow remote configuration import, reboot or arbitrary GPIO commands through this bridge.

`stop()` requests cooperative shutdown. Keep the runtime alive and poll `is_stopped()` before starting another instance or replacing its configuration. Active HTTP header/handshake operations have a 12 second timeout; response reads use one second timeouts and check cancellation between reads. An API call cannot be forcibly stopped by dropping its Rust future.

## Modes

`EchoOnly` provides `/ping`, `/echo TEXT`, `/status`, `/led on|off|rgb R G B|status`, `/stop` and `/help`. Ordinary messages do not call DeepSeek in this mode; memory commands require `DeepSeek` mode.

`DeepSeek` additionally sends ordinary text to the configured DeepSeek model. It uses the core SSE decoder, non-thinking mode, an output token cap, a 120 second total request limit and the original QQ passive-reply deadline. A completed reply must contain a `stop` finish reason and `[DONE]`.

Both modes require a nonempty user allow-list. A group message requires both its sender and its group to be allowed. Both group event types are supported; all-message events additionally require the server's `mentions[].is_you=true` or a complete AppID mention marker. Ordinary group messages remain silent. Private and per-sender group sessions remain separate.

CopilotLee follows the user's language. v0.3 stores explicit facts, a summary and recent completed turns in bounded dual-slot SD records, with a 48 KiB RAM text cache fallback. After six turns or byte pressure, valid automatic summaries replace older completed turns. `/remember`, `/memory`, `/memory compact` and `/forget INDEX|all` manage memory. `/reset` clears summary/recent turns while preserving explicit facts and invalidates pending results; `/stop` cancels pending results while preserving history. Report actual SD durability and retain unknown disk data on conflicts. See the application `AGENTS.md` and bilingual reference guide for limits and acceptance evidence.

DeepSeek Flash image input uses up to two transient QQ HTTPS attachments; signed URLs never enter stored history. The model must advertise image input. Camera capture remains disabled. Search and Fetch use native DeepSeek endpoints. v0.4.0 adds bounded model-selected search_web/fetch_page/recall_history for ordinary text, with at most two tool executions per request and one active network request at a time. Explicit commands remain available. Tool outputs are untrusted transient data. SD history stores at most 32 turns per exact session, clipped to 1024 user + 2048 assistant UTF-8 bytes per turn, and ranks records one at a time with at most three 768-byte snippets. /memory search QUERY provides explicit retrieval; /reset and /forget all clear the archive. There is no resident embedding/index service. Old text already replaced by a v0.3 summary cannot be recovered.


v0.5.0 normalizes supplied quotes/forwards (depth 3, nodes 16), card metadata, QQ face tags, and QQ-provided voice transcripts. Original text plus material is bounded at 8 KiB, with material capped at 4 KiB and at most four attachments inspected. Only picture-card previews become vision input; link-card covers do not. Only live typed input without material can dispatch commands. Supplied material remains a separate untrusted user message before the original latest input. Unsupported audio/file/video contents are explicitly unread. v0.5.1 adds bounded text-file reads and explicit URL-backed image/file replies below; typing, streaming and buttons remain pending.

Replies use custom Markdown (msg_type=2), splitting at UTF-8/paragraph boundaries and balancing code fences where practical. Every attempted send reserves a unique per-trigger sequence from a shared four-attempt budget; unknown deliveries are never replayed. Only a completely received explicit HTTP 400/403/415/422 rejection permits one plain-text fallback for that part; later parts use plain text. Credential/rate-limit/server errors, malformed replies, timeouts and transport failures do not trigger retries. Oversized individual inline Markdown constructs can still be split; QQ clients do not guarantee every GitHub Markdown feature.

Independent diagnostics can run on a background worker before QQ is enabled:

- `probe_deepseek(&Config)` makes only an authenticated `GET /models` and returns bounded model IDs.
- `test_deepseek(Arc<Config>)` performs one short, billed inference with a fixed `RUST_ESP_OK` prompt and a 32 token output cap. It returns only after a complete SSE result and never sends QQ messages.

The board loop must not invoke these blocking functions directly. They do not print keys or response bodies.

`BotRuntime::notify_owner(text)` is a separate local-only, one-shot proactive private notification API. It requires exactly one configured user, validates an entire encoded body of at most 1800 bytes, and starts one bounded notification worker using the runtime's existing token cache. The request contains only `content` and `msg_type`, never a forged passive message ID. It returns a receiver containing the platform message ID or a safe error. HTTP errors preserve QQ's numerical business code when available. There is no automatic retry, schedule or boot trigger; local UART code must explicitly invoke it. A failed/unknown POST must not be silently replayed.

Gateway handling, inference and QQ REST sending have separate workers. Gateway callbacks only copy bounded chunks and use `try_send`. An overflow forces a reconnect/resume instead of silently continuing with a missing fragment. Reply work retains the original message ID, per-trigger reply sequence and expiration time. Unconfirmed message POSTs are not replayed automatically.

## Verification

Pure wire parsing tests run without an ESP toolchain:

```sh
cargo +stable test -p qq-deepseek-core --locked --target x86_64-unknown-linux-gnu
cargo +stable clippy -p qq-deepseek-core --locked --target x86_64-unknown-linux-gnu --all-targets -- -D warnings
```

Run this command from `application/qq_deepseek_rust`. The firmware still needs an ESP build and real QQ/DeepSeek verification. Host parsing tests are not evidence of a successful device connection or delivered QQ message.


v0.5.1 reads at most two 16 KiB UTF-8 text/code attachments from validated QQ/qpic HTTPS CDN origins, without credentials or redirects, sequentially on the inference worker before model TLS. Each file has a 20-second deadline and a <=3 KiB excerpt; escaped material remains inside the existing combined 4 KiB reference budget. Download URLs are omitted from model/history/logs; code is not executed and raw files are not written to SD. PDF/Office/video remain unread. /image HTTPS_URL and /file HTTPS_URL use a server-side QQ URL upload (srv_send_msg=false), then one passive msg_type=7 message. Unknown final sends are never replayed, and upload failure reports an error without claiming delivery. Actual current card/file/quote links are eligible bounded Fetch targets through DeepSeek.
