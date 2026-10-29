---
{
  "name": "codex_reset_status",
  "description": "查询 Codex 额度重置、Tibo 重置预告、进行中、确认完成和重置卡动态。读取每十分钟更新的 AIHOT 监测缓存；不预测个人额度。",
  "metadata": {
    "cap_groups": ["cap_lua", "cap_http_request", "cap_im_qq"],
    "manage_mode": "runtime"
  }
}
---

# Codex Reset Status

Run `{CUR_SKILL_DIR}/scripts/codex_reset_status.lua` synchronously with
`{"mode":"report","recent":3}` to read the most recent successful monitor
snapshot. `recent` can be 0–8. `mode=check` also reads the cache without making
requests, sending messages, or changing state. Relay any `⚠️` line as written.

The scheduler alone owns `mode=notify` and supplies its QQ `chat_id`. Do not
manually run notify/bootstrap or create additional writers. The single scheduler
polls `https://aihot.news/api/v1/codex-resets/recent` every ten minutes, following
AIHOT's upstream cadence. The first successful poll silently establishes a
baseline. Subsequent polls notify individual new/changed source posts, event
changes, explicit withdrawals, and possible regrouping. No local AI is used.

Treat each post's stage independently of its event's current status. A pre-reset
post remains an announcement even after the event has been confirmed. Progress
posts with `action=null` must still be detected. A passed estimate is not a
confirmation; a removed calendar record may have been merged, not withdrawn.

Quote source timestamps with their timezone. `checkedAt` is the complete
verification watermark and can lag newer collection; `monitor.lastCollectedAt`
is collection time. Neither is an event execution time. A confirmation source
timestamp is not a precise execution time or proof of the user's account quota.
Use source-qualified `displayLabel` and audience fields, not the legacy
"全员重置" label when eligibility is unspecified. Link AIHOT and the source post.

State lives at the DATA root's `state/codex_reset_monitor_v2.json`. An outbox is
saved before sending, successful sends are checkpointed individually, and failed
sends remain queued. Do not delete state to silence a failure. The old v1 state
is preserved but no longer used. This deployment updates DATA skills only.
