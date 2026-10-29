-- Pure snapshot comparison: no network, clock, storage, or model calls.
local M = {}
local ACTION = {announce = "预告", confirm = "确认完成", amend = "修订", withdraw = "撤回"}
local STATUS = {
    announced = "预告待确认", in_progress = "进行中",
    confirmed = "已确认", expired_unconfirmed = "预计窗口已过，尚未确认",
    likely_completed = "站点推测可能完成，尚未获来源确认",
}

local function str(v) return type(v) == "string" and v or "" end
local function object(v) return type(v) == "table" and v or {} end

-- Stable and collision-free encoding; map iteration order never affects alerts.
function M.signature(v)
    if type(v) ~= "table" then
        local s = tostring(v)
        return type(v) .. #s .. ":" .. s
    end
    local keys, out = {}, {}
    for k in pairs(v) do keys[#keys + 1] = k end
    table.sort(keys, function(a, b) return tostring(a) < tostring(b) end)
    for _, k in ipairs(keys) do
        out[#out + 1] = M.signature(k) .. M.signature(v[k])
    end
    return "{" .. table.concat(out) .. "}"
end

local function event_view(e)
    local p = object(e.presentation)
    return {
        id = e.id, status = str(e.status), phase = str(p.status),
        label = str(e.displayLabel) ~= "" and e.displayLabel or "额度重置／发卡",
        title = str(e.title), scope = str(p.audienceZh or p.scopeLabel),
        products = str(p.productsZh), basis = str(e.confirmationBasis),
        schedule = object(e.schedule), estimate = object(e.estimate),
        updatedAt = str(e.updatedAt), createdAt = str(e.createdAt),
        confirmedAt = str(e.confirmedAt), occurredOn = str(e.occurredOn),
    }
end

local function event_signature(e)
    local v = {}
    for k, x in pairs(e) do
        -- Collection watermarks and metadata-only corrections are not alerts.
        if k ~= "updatedAt" and k ~= "createdAt" then v[k] = x end
    end
    return M.signature(v)
end

local function source_view(p)
    assert(type(p.id) == "string" and p.id ~= "", "source post without id")
    return {
        id = p.id, at = str(p.publishedAt), stage = str(p.stage),
        text = str(p.fullText or p.text or p.fullOriginalText or p.originalText),
        original = str(p.fullOriginalText or p.originalText), url = str(p.url),
        action = str(p.action), notify = true, eventIds = {},
    }
end

function M.snapshot(payload)
    assert(type(payload) == "table" and payload.schemaVersion == 1, "unsupported schema")
    assert(type(payload.events) == "table", "missing events array")
    assert(type(payload.checkedAt) == "string", "missing verification watermark")
    assert(type(payload.historyFrom) == "string", "missing history window")
    local snap = {events = {}, posts = {}, historyFrom = payload.historyFrom}
    for _, e in ipairs(payload.events) do
        assert(type(e.id) == "string" and e.id ~= "", "event without id")
        snap.events[e.id] = event_view(e)
        for _, p in ipairs(object(e.posts)) do
            local v = snap.posts[p.id] or source_view(p)
            v.eventIds[e.id] = true
            snap.posts[p.id] = v
        end
    end
    for _, p in ipairs(object(payload.activities)) do
        local previous = snap.posts[p.id]
        local v = source_view(p)
        v.eventIds = previous and previous.eventIds or {}
        for _, id in ipairs(object(p.eventIds)) do v.eventIds[id] = true end
        v.stage = ACTION[v.action] or (previous and previous.stage) or "进展"
        if v.stage == "" then v.stage = "进展" end
        v.notify = p.kind == "event_update" or p.statusChanged == true
        snap.posts[p.id] = v
    end
    return snap
end

local function event_for(post, current, previous)
    local ids = {}
    for id in pairs(post.eventIds) do ids[#ids + 1] = id end
    table.sort(ids)
    for _, id in ipairs(ids) do
        local e = current.events[id] or previous.events[id]
        if e then return e end
    end
    return {}
end

function M.diff(previous, current)
    local changes, covered = {}, {}
    for id, p in pairs(current.posts) do
        local old = previous.posts[id]
        if p.notify and (not old or M.signature(old) ~= M.signature(p)) then
            changes[#changes + 1] = {
                key = "post:" .. id, revision = M.signature(p), post = p,
                event = event_for(p, current, previous), corrected = old ~= nil,
            }
            for event_id in pairs(p.eventIds) do covered[event_id] = true end
        end
    end
    for id, e in pairs(current.events) do
        local old = previous.events[id]
        if not covered[id] and (not old or event_signature(e) ~= event_signature(old)) then
            changes[#changes + 1] = {
                key = "event:" .. id, revision = event_signature(e), event = e,
                phase = STATUS[e.phase] or STATUS[e.status] or "状态更新",
                at = e.updatedAt, corrected = old ~= nil,
            }
        end
    end
    for id, e in pairs(previous.events) do
        -- A rolling-window expiry is not a withdrawal. Disappearance inside the
        -- retained window can also be a merge: never assert a cancellation.
        local age = e.updatedAt ~= "" and e.updatedAt or e.createdAt
        if not current.events[id] and not covered[id] and age >= current.historyFrom then
            changes[#changes + 1] = {
                key = "removed:" .. id, revision = "absent", event = e,
                phase = "记录已移除或合并（不等同于撤回）", at = age,
            }
        end
    end
    table.sort(changes, function(a, b)
        local at, bt = a.post and a.post.at or a.at, b.post and b.post.at or b.at
        if at == bt then return a.key < b.key end
        return at < bt
    end)
    return changes
end

local function clip(s, n)
    if #s <= n then return s end
    local cut = n
    while cut > 0 and s:byte(cut + 1) >= 128 and s:byte(cut + 1) < 192 do cut = cut - 1 end
    return s:sub(1, cut) .. "…"
end

function M.time_label(s)
    if not s or s == "" then return "未知" end
    return s:gsub("T", " "):gsub("%.%d+", ""):gsub("%+08:00$", " 北京时间")
end

function M.message(change)
    local p, e = change.post, change.event or {}
    local stage = p and p.stage or change.phase
    local lines = {"📣 Codex 重置动态｜" .. stage}
    if change.corrected then lines[#lines + 1] = "此条为已收录消息或状态的更新" end
    if e.label then lines[#lines + 1] = "事项：" .. e.label end
    if p then
        lines[#lines + 1] = clip(p.text ~= "" and p.text or p.original, 1100)
        lines[#lines + 1] = "原帖时间：" .. M.time_label(p.at)
    else
        lines[#lines + 1] = e.title or "事件状态更新"
        if e.basis == "receipt_review" then lines[#lines + 1] = "依据：AIHOT 到账核验，非 Tibo 确认帖" end
    end
    -- Source-stage alerts must not inherit the event's later 'confirmed' state.
    if stage ~= "确认完成" and stage ~= "已确认" and stage ~= "撤回" then
        if object(e.schedule).label then lines[#lines + 1] = "原预告：" .. e.schedule.label end
        if object(e.estimate).label then lines[#lines + 1] = "站点推算（非承诺）：" .. e.estimate.label end
    end
    if e.scope and e.scope ~= "" then lines[#lines + 1] = "适用范围：" .. e.scope end
    if p and p.url ~= "" then lines[#lines + 1] = p.url end
    lines[#lines + 1] = "数据来源：AIHOT https://aihot.news/codex-reset"
    return table.concat(lines, "\n")
end

return M
