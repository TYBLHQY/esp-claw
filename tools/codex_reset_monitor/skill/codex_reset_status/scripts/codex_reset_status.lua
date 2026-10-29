-- One scheduler-owned writer. Reports/checks only read its last good snapshot.
local storage = require("storage")
local json = require("json")
local capability = require("capability")
local events = require("reset_events")

local API = "https://aihot.news/api/v1/codex-resets/recent"
local INTERVAL = 600 -- AIHOT's collection and documented client polling cadence.
local BACKOFF = 3600 -- Headers are not exposed by this firmware's HTTP capability.
local MAX_BODY = 512 * 1024
local MAX_PENDING = 256
local STATE_SCHEMA = 2
local args = type(args) == "table" and args or {}
local mode = args.mode or "report"
local root = storage.get_root_dir()
local dir = storage.join_path(root, "state")
local state_path = storage.join_path(dir, "codex_reset_monitor_v2.json")
local cache_path = storage.join_path(root, "cache", "codex_reset_recent_v2.json")
local channel = args.channel or "qq"
local chat_id = args.chat_id
-- ESP Lua uses single-precision lua_Number; system.time() and JSON numeric
-- epochs lose minute-level precision. os.time() returns a Lua integer, and
-- persisted epochs are decimal strings so a 600 s deadline stays exactly 600 s.
local now = os.time()

local function decode_file(path)
    return json.decode(storage.read_file(path))
end

local function load_state()
    local state
    if not storage.exists(state_path) then
        if storage.exists(state_path .. ".previous") then
            state = decode_file(state_path .. ".previous")
        else
            return nil
        end
    else
        state = decode_file(state_path)
    end
    -- Corrupt state is an error, never permission to silently reset the baseline.
    assert(state.schema == STATE_SCHEMA and type(state.snapshot) == "table"
        and type(state.pending) == "table", "invalid monitor state")
    return state
end

local function save_state(state)
    storage.mkdir(dir)
    local tmp, previous = state_path .. ".tmp", state_path .. ".previous"
    storage.write_file(tmp, json.encode(state))
    -- Keep a readable rollback copy even on FAT implementations that cannot
    -- rename over an existing file. Each state write has exactly one owner.
    if storage.exists(state_path) then
        storage.write_file(previous, storage.read_file(state_path))
        storage.remove(state_path)
    end
    storage.rename(tmp, state_path)
end

local function call(name, payload)
    local ok, out, err = capability.call(name, payload, {
        source_cap = "codex_reset_status", channel = channel, chat_id = chat_id,
    })
    if not ok then return nil, tostring(err or out or "capability failed") end
    return tostring(out or "")
end

local function send(message)
    assert(channel == "qq" and type(chat_id) == "string" and chat_id ~= "", "QQ destination required")
    return call("qq_send_message", {chat_id = chat_id, message = message})
end

local function drain(state)
    -- Save after each success, so the successful prefix is not replayed when a
    -- later message fails. A crash between QQ acceptance and checkpointing can
    -- still duplicate that one message (at-least-once delivery).
    while #state.pending > 0 do
        local item = state.pending[1]
        local out, err = send(item.message)
        if not out then
            state.delivery_error = err
            save_state(state)
            print("[codex_reset_v2] QQ failed; pending=" .. #state.pending .. " error=" .. err)
            return false
        end
        table.remove(state.pending, 1)
        state.delivery_error = nil
        state.last_sent_at = tostring(now)
        save_state(state)
        print("[codex_reset_v2] QQ accepted key=" .. item.key .. " result=" .. out)
    end
    return true
end

local function fetch()
    storage.mkdir(storage.join_path(root, "cache"))
    local out, err = call("http_request", {
        url = API, method = "GET", headers = { ["User-Agent"] = "aihot-api/2.0.0" },
        timeout_ms = 25000, save_path = cache_path, max_file_bytes = MAX_BODY,
    })
    if not out then return nil, err end
    local code = tonumber(out:match("^HTTP%s+(%d+)"))
    if code ~= 200 or out:find("truncated", 1, true) then
        if storage.exists(cache_path) then storage.remove(cache_path) end
        return nil, "HTTP " .. tostring(code) .. (out:find("truncated", 1, true) and " truncated" or ""), code
    end
    local ok, payload = pcall(decode_file, cache_path)
    if storage.exists(cache_path) then storage.remove(cache_path) end
    if not ok then return nil, "JSON incomplete or invalid: " .. tostring(payload) end
    local valid, snapshot = pcall(events.snapshot, payload)
    if not valid then return nil, "invalid snapshot: " .. tostring(snapshot) end
    return {snapshot = snapshot, checked_at = payload.checkedAt, monitor = payload.monitor}
end

local function report(state)
    if not state then print("⚠️ 监测尚未建立基线，需先完成部署初始化"); return end
    print("Codex 重置监测：每 10 分钟检查，预告／进展／完成／修订／撤回分别通知")
    local fetched = state.last_fetch_at and os.date("!%Y-%m-%d %H:%M:%S", tonumber(state.last_fetch_at) + 8 * 3600)
    print("本机最近成功获取：" .. (fetched or "未知") .. " 北京时间")
    print("来源完整核验水位：" .. events.time_label(state.checked_at))
    local monitor = state.monitor or {}
    print("来源采集时间：" .. events.time_label(monitor.lastCollectedAt))
    print("来源监测状态：" .. tostring(monitor.status or "unknown"))
    print("待发送：" .. #state.pending)
    local posts = {}
    for _, p in pairs(state.snapshot.posts) do
        if p.notify then posts[#posts + 1] = p end
    end
    table.sort(posts, function(a, b) return a.at > b.at end)
    local limit = math.min(tonumber(args.recent) or 3, 8)
    for i = 1, math.min(#posts, limit) do
        local p = posts[i]
        print(p.stage .. "｜" .. events.time_label(p.at) .. "｜" .. p.text .. "\n" .. p.url)
    end
end

local function main()
    assert(mode == "notify" or mode == "bootstrap" or mode == "report" or mode == "check", "invalid mode")
    local state = load_state()
    if mode == "report" or mode == "check" then report(state); return end
    assert(now > 1735689600, "device clock not synchronized")
    assert(channel == "qq" and type(chat_id) == "string" and chat_id ~= "", "QQ destination required")
    if state then
        assert(state.destination == channel .. ":" .. chat_id, "destination differs from baseline")
        if mode == "bootstrap" then error("baseline already exists; refusing to discard pending notifications") end
        -- Delivery retry is independent of fetch cadence, including early runs.
        local delivered = drain(state)
        if now < (tonumber(state.next_fetch_at) or 0) then
            print("[codex_reset_v2] poll deferred until=" .. state.next_fetch_at)
            assert(delivered, "QQ delivery still pending")
            return
        end
    else
        state = {
            schema = STATE_SCHEMA, destination = channel .. ":" .. chat_id,
            snapshot = {events = {}, posts = {}, historyFrom = ""}, pending = {},
        }
    end
    -- Checkpoint before networking so another trigger/reboot cannot cause an
    -- immediate extra upstream request. Only this scheduler owns notify mode.
    state.next_fetch_at = tostring(now + INTERVAL)
    save_state(state)
    local result, err, status = fetch()
    if not result then
        state.fetch_error = err
        state.failure_count = (state.failure_count or 0) + 1
        -- The current capability exposes body/status, not ETag or Retry-After.
        -- Never retry immediately; rate limits and service failures wait 1 h.
        if status == 429 or status == 503 then state.next_fetch_at = tostring(now + BACKOFF) end
        if state.failure_count >= 3 and now - (tonumber(state.last_failure_notice) or 0) >= 43200 then
            if send("⚠️ Codex 重置监测连续获取失败，稍后自动重试。\n" .. tostring(err)
                .. "\n数据来源：AIHOT https://aihot.news/codex-reset") then
                state.last_failure_notice = tostring(now)
            end
        end
        save_state(state)
        error(err)
    end
    local changes = state.initialized and events.diff(state.snapshot, result.snapshot) or {}
    assert(#state.pending + #changes <= MAX_PENDING, "notification backlog full; snapshot retained")
    for _, change in ipairs(changes) do
        state.pending[#state.pending + 1] = {
            key = change.key, message = events.message(change), revision = change.revision,
        }
    end
    state.snapshot = result.snapshot
    state.checked_at, state.monitor = result.checked_at, result.monitor
    state.last_fetch_at, state.fetch_error, state.failure_count = tostring(now), nil, 0
    local first = not state.initialized
    state.initialized = true
    save_state(state) -- Commit the outbox before making any outbound send.
    print("[codex_reset_v2] fetch ok baseline=" .. tostring(first) .. " changes=" .. #changes
        .. " pending=" .. #state.pending .. " checkedAt=" .. result.checked_at)
    assert(drain(state), "QQ delivery still pending")
end

local ok, err = pcall(main)
if not ok then
    print("⚠️ Codex 重置监测：" .. tostring(err))
    error(err, 0)
end
