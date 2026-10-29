local source = debug.getinfo(1, "S").source:sub(2)
local dir = source:match("^(.*)/") .. "/../skill/codex_reset_status/scripts/"
package.path = dir .. "?.lua;" .. package.path
local core = require("reset_events")
local function copy(v)
    if type(v) ~= "table" then return v end
    local r = {}; for k, x in pairs(v) do r[k] = copy(x) end; return r
end
local function post(id, stage, text, at)
    return {id = id, stage = stage, text = text, originalText = text,
        publishedAt = at or "2026-10-15T12:55:51+08:00", url = "https://x.com/i/status/" .. id}
end
local function payload(posts, status)
    return {schemaVersion = 1, checkedAt = "2026-10-15T15:00:00+08:00",
        historyFrom = "2026-10-08T00:00:00+08:00", activities = {}, events = {{
        id = "reset-A", status = status or "announced", type = "direct_reset",
        displayLabel = "重置（形式未明确）", title = "重置",
        updatedAt = "2026-10-15T12:55:51+08:00", createdAt = "2026-10-15T12:55:51+08:00",
        schedule = {label = "两小时内"}, presentation = {status = status or "announced"}, posts = posts,
    }}}
end
local empty = {events = {}, posts = {}, historyFrom = "2026-10-08T00:00:00+08:00"}
local announce = post("p1", "预告", "Landing within two hours")
local initial = payload({announce})
local diff = core.diff(empty, core.snapshot(initial))
assert(#diff == 1 and diff[1].post.stage == "预告", "announcement must alert independently")

local progress = post("p2", "进展", "Landing within the next 2 hours", "2026-10-15T12:56:07+08:00")
local complete = post("p3", "确认完成", "Done and dusted", "2026-10-15T14:28:13+08:00")
local finished = payload({complete, progress, announce}, "confirmed")
finished.activities = {{id = "p2", kind = "event_update", statusChanged = true,
    publishedAt = progress.publishedAt, originalText = progress.text,
    text = progress.text, eventIds = {"reset-A"}, url = progress.url}}
diff = core.diff(empty, core.snapshot(finished))
assert(#diff == 3 and diff[1].post.stage == "预告" and diff[2].post.stage == "进展"
    and diff[3].post.stage == "确认完成", "fast reset must preserve all stages in time order")
assert(not core.message(diff[3]):find("两小时内", 1, true), "completion must not show an upcoming estimate")
assert(not core.message(diff[1]):find("已确认", 1, true), "announcement must not inherit current confirmed status")
assert(core.message(diff[1]):find("形式未明确", 1, true), "do not claim global eligibility")

local latest = core.snapshot(finished)
local changed_clock = copy(finished)
changed_clock.checkedAt = "2026-10-15T16:00:00+08:00"
changed_clock.events[1].updatedAt = "2026-10-15T16:00:00+08:00"
assert(#core.diff(latest, core.snapshot(changed_clock)) == 0, "watermark-only changes stay silent")
local related = copy(finished)
related.activities[#related.activities + 1] = {id = "chat", kind = "related", statusChanged = false,
    publishedAt = progress.publishedAt, text = "hello", originalText = "hello", eventIds = {}, url = "x"}
assert(#core.diff(latest, core.snapshot(related)) == 0, "unrelated interactions stay silent")

local withdraw = copy(finished)
withdraw.events = {}
withdraw.activities = {{id = "p4", action = "withdraw", kind = "event_update", statusChanged = true,
    publishedAt = "2026-10-15T16:00:00+08:00", text = "withdrawn", eventIds = {"reset-A"}, url = "x"}}
diff = core.diff(latest, core.snapshot(withdraw))
assert(#diff == 1 and diff[1].post.stage == "撤回", "explicit withdrawal must alert exactly once")
local removed = copy(finished); removed.events = {}; removed.activities = {}
diff = core.diff(latest, core.snapshot(removed))
assert(#diff == 1 and diff[1].phase:find("不等同于撤回", 1, true), "merge must not be called a withdrawal")
removed.historyFrom = "2026-10-16T00:00:00+08:00"
assert(#core.diff(latest, core.snapshot(removed)) == 0, "rolling-window expiry is silent")
local corrected = copy(finished); corrected.events[1].posts[1].text = "Correction"
diff = core.diff(latest, core.snapshot(corrected))
assert(#diff == 1 and diff[1].corrected, "same-ID source corrections must alert")
local amended = copy(finished)
amended.activities = {{id = "p5", action = "amend", kind = "event_update", statusChanged = true,
    publishedAt = "2026-10-15T16:00:00+08:00", text = "new time", eventIds = {"reset-A"}, url = "x"}}
assert(core.diff(latest, core.snapshot(amended))[1].post.stage == "修订")
local window_passed = copy(initial)
window_passed.events[1].presentation.status = "likely_completed"
diff = core.diff(core.snapshot(initial), core.snapshot(window_passed))
assert(#diff == 1 and diff[1].phase:find("尚未获来源确认", 1, true), "inferred completion is not source confirmation")
assert(not pcall(core.snapshot, {schemaVersion = 99, events = {}}), "reject schema drift")
print("PASS: announcement, rapid stages, nullable action, revisions, withdrawals, expiry, freshness")

-- Exercise the actual scheduler wrapper with a deterministic simulated device.
-- The JSON codec here snapshots tables so storage behaves like a real restart.
local files, encoded, serial, now = {}, {}, 0, 1800000000
local current, requests, sends, failures, http_status, malformed = initial, 0, {}, {}, 200, false
local max_bytes, targets = nil, {}
os.time = function() return now end
package.preload.storage = function() return {
    get_root_dir = function() return "/data" end,
    join_path = function(...) return table.concat({...}, "/") end,
    mkdir = function() return true end,
    exists = function(p) return files[p] ~= nil end,
    read_file = function(p) assert(files[p], "not found"); return files[p] end,
    write_file = function(p, v) files[p] = v; return true end,
    remove = function(p) files[p] = nil; return true end,
    rename = function(a, b) assert(files[a]); files[b], files[a] = files[a], nil; return true end,
} end
package.preload.json = function() return {
    encode = function(v) serial = serial + 1; local key = "json:" .. serial; encoded[key] = copy(v); return key end,
    decode = function(v)
        if v == "payload" then return copy(current) end
        assert(encoded[v], "invalid JSON"); return copy(encoded[v])
    end,
} end
package.preload.system = function() return {time = function() return now end} end
package.preload.capability = function() return {call = function(name, p)
    if name == "http_request" then
        requests = requests + 1; max_bytes = p.max_file_bytes
        assert(p.url:match("/recent$") and p.headers["User-Agent"] == "aihot-api/2.0.0")
        files[p.save_path] = malformed and "broken" or "payload"
        return true, "HTTP " .. http_status .. "\nSaved response body"
    end
    assert(name == "qq_send_message")
    targets[#targets + 1] = p.message
    if failures[p.message] then return false, nil, "offline" end
    sends[#sends + 1] = p.message
    return true, "sent"
end} end
local state_path = "/data/state/codex_reset_monitor_v2.json"
local function state() return copy(encoded[files[state_path]]) end
local function run(mode)
    _G.args = {mode = mode or "notify", channel = "qq", chat_id = "test"}
    return pcall(dofile, dir .. "codex_reset_status.lua")
end
assert(run("bootstrap")); assert(#sends == 0 and requests == 1, "baseline must not replay history")
assert(state().next_fetch_at == tostring(now + 600) and state().last_fetch_at == tostring(now),
    "persist epochs as decimal strings to avoid ESP float32 rounding")
assert(max_bytes >= 65536, "must fetch complete JSON to file, not inline prefix")
assert(run()); assert(requests == 1, "early trigger must not bypass upstream cadence")
now = now + 600; current = finished
assert(run()); assert(requests == 2 and #sends == 2, "new progress and confirmation must both notify")
assert(state().initialized and #state().pending == 0)
now = now + 600
assert(run()); assert(#sends == 2, "identical payload after restart must stay silent")
local before = files[state_path]
assert(run("report") and run("check")); assert(files[state_path] == before and requests == 3,
    "manual report/check must not fetch, send, or mutate")

now = now + 600; current = amended
local expected = core.message(core.diff(core.snapshot(finished), core.snapshot(amended))[1])
failures[expected] = true
assert(not run()); assert(#state().pending == 1, "failed delivery must remain persisted")
local request_count = requests
failures[expected] = nil
assert(run()); assert(#state().pending == 0 and requests == request_count,
    "delivery retry must work without a new fetch")
assert(#sends == 3, "successful messages must not be replayed with failed ones")

now = now + 600; http_status = 429
local snapshot_before = core.signature(state().snapshot)
assert(not run()); assert(state().next_fetch_at == tostring(now + 3600), "rate limit must back off")
assert(core.signature(state().snapshot) == snapshot_before, "HTTP failure must preserve comparison baseline")
now = now + 600; assert(run()); assert(requests == request_count + 1, "backoff must survive restart")
now = now + 3000; http_status = 200; malformed = true
assert(not run()); assert(core.signature(state().snapshot) == snapshot_before, "invalid JSON must retain baseline")
assert(not run("bootstrap"), "reinitialization must not discard state")
files[state_path] = "corrupt"
assert(not run(), "corrupt state must not silently rebaseline")
print("PASS: persisted baseline, ten-minute polling, complete payload, outbox/retry, 429, malformed JSON, read-only reports")
