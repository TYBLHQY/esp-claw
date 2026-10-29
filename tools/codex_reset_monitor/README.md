# ESP-Claw Codex reset monitor

Runtime DATA skill source for the existing `codex_reset_status` installation.
It makes no LLM calls and requires no firmware rebuild or flash. Do not put
user-specific schedules or chat IDs in the SYSTEM recovery defaults.

- Poll `https://aihot.news/api/v1/codex-resets/recent` every **600 seconds**.
- Fetch the entire response to a bounded file, then parse the complete JSON.
  A truncated response never becomes a successful partial snapshot.
- Compare all retained post IDs/content and event states; notify announcements,
  progress (`action=null` included), confirmations, amendments and withdrawals.
  Distinguish a removed/merged record from a withdrawal and rolling-window expiry.
- Save a durable outbox before sending; checkpoint each success. Failed sends
  remain queued. Delivery is at least once: a power failure after QQ acceptance
  but before checkpointing can repeat that one message.
- The first successful poll silently establishes a baseline. Keep the old v1
  state and back up the current skill/scheduler before replacing anything.
- One scheduler is the only writer. Manual `report`/`check` reads the cache and
  does not spend another upstream request or alter delivery state.
- Read integer time with `os.time()` and persist epochs as decimal strings. The
  installed ESP Lua uses single-precision numbers: `system.time()` and JSON
  numeric epochs otherwise round modern timestamps by up to a minute and can
  turn a ten-minute fetch gate into a skipped twenty-minute polling cycle.
- Watermark/clock changes alone are silent. Reports distinguish collection time,
  the complete verification watermark and source-post timestamps.

Upstream contract checked 2026-10-15:
[OpenAPI](https://aihot.news/openapi-v1.json). Collection and client polling are
documented as ten minutes, even though `/recent` has a sixty-second HTTP cache.
Do not shorten the interval to one minute based on that cache header.

The currently installed `http_request` capability exposes status/body or a
saved file, **not response headers**. This runtime-only deployment therefore
cannot read ETag or Retry-After, and does not advertise conditional requests.
It avoids immediate retries, gates every fetch for at least 600 seconds, and
backs off one hour on 429/503. Adding response-header support requires a separate
firmware change. No compression header is sent because the installed capability
does not decode gzip/br; the complete recent JSON is still small enough for DATA
storage and PSRAM.

## Validation

```bash
python3 tools/codex_reset_monitor/tests/run.py
```

The runner uses `liblua5.4.so.0`; no Lua CLI or extra Python package is needed.
Scenarios cover rapid announcement→progress→completion, nullable actions,
same-ID corrections, withdrawals, merges/window expiry, schema drift, persistent
deduplication, initialization without history spam, the durable outbox, failed
delivery retry independent of fetching, rate limits and corrupt/incomplete JSON.

## Installation ownership

Copy `skill/codex_reset_status/` into the device's DATA `skills/` directory;
preserve the existing skill directory until the replacement is verified. The
script directory must contain both Lua files. Disable the old 07:00/13:00/18:00
schedules. Create exactly one enabled 600000 ms interval schedule publishing the
existing `codex_reset_daily_report` event with the same QQ chat destination.
The existing router rule remains compatible with the entrypoint.

Bootstrap once while schedulers are disabled with
`{"mode":"bootstrap","channel":"qq","chat_id":"<existing destination>"}`.
Then enable the interval schedule. Verify a real scheduled run and the cached
report. Keep the previous schedules disabled for rollback rather than deleting
them. Use `{CUR_SKILL_DIR}/scripts/codex_reset_status.lua` inside skill documents;
DATA paths in a router are resolved when installing on that particular device.

Device CLI commands are limited to 512 bytes. Transfers must use bounded chunks,
verify downloaded bytes and rename a staged file only after verification. Remove
any temporary maintenance Lua helper after deployment.
