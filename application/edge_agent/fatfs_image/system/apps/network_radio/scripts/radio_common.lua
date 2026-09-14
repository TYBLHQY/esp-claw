-- --------------------------------------------------------------
-- Shared helpers for the network radio skill.
-- --------------------------------------------------------------

local json = require("json")
local storage = require("storage")
local system = require("system")

local M = {}

M.DEFAULT_CODEC_NAME = "audio_dac"
M.DEFAULT_VOLUME = 70
M.MIN_VOLUME = 0
M.MAX_VOLUME = 100
M.CONTROL_DIR = "/ramfs/network_radio"
M.STATUS_PATH = "/ramfs/network_radio/status.json"

M.STATIONS = {
  { title = "崂山921", url = "http://lhttp.qingting.fm/live/20212426/64k.mp3" },
  { title = "长沙101.7城市之声", url = "http://lhttp.qingting.fm/live/4237/64k.mp3" },
  { title = "上海经典947", url = "http://lhttp.qingting.fm/live/267/64k.mp3" },
  { title = "湖北经典音乐广播", url = "http://lhttp.qingting.fm/live/1296/64k.mp3" },
  { title = "成都年代音乐怀旧好声音", url = "http://lhttp.qingting.fm/live/20211686/64k.mp3" },
  { title = "山东经典音乐广播", url = "http://lhttp.qingting.fm/live/20240/64k.mp3" },
  { title = "杭州90.7", url = "http://lhttp.qingting.fm/live/15318146/64k.mp3" },
  { title = "天津TIKI 100.5", url = "http://lhttp.qingting.fm/live/20003/64k.mp3" },
}

function M.ensure_control_dir()
  if storage.exists(M.CONTROL_DIR) then
    return M.CONTROL_DIR
  end
  local ok, err = storage.mkdir(M.CONTROL_DIR)
  if ok == false then
    print("[network_radio] ERROR: failed to create control dir: " .. tostring(err))
    error("failed to create network_radio control dir: " .. tostring(err))
  end
  return M.CONTROL_DIR
end

function M.read_json(path)
  if not storage.exists(path) then
    return nil
  end
  local text = storage.read_file(path)
  if not text or text == "" then
    return nil
  end
  local ok, value = pcall(json.decode, text)
  if not ok then
    print("[network_radio] ERROR: invalid json file=" .. tostring(path) .. " err=" .. tostring(value))
    return nil, value
  end
  return value
end

function M.write_json(path, value)
  M.ensure_control_dir()
  local text = json.encode(value)
  local tmp_path = path .. ".tmp"
  local ok, err = storage.write_file(tmp_path, text)
  if ok == false then
    print("[network_radio] ERROR: failed to write temp json: " .. tostring(err))
    error("failed to write " .. tmp_path .. ": " .. tostring(err))
  end
  pcall(storage.remove, path)
  ok, err = storage.rename(tmp_path, path)
  if ok == false then
    print("[network_radio] ERROR: failed to publish json: " .. tostring(err))
    error("failed to rename " .. tmp_path .. " to " .. path .. ": " .. tostring(err))
  end
end

function M.now_ms()
  return system.millis()
end

function M.clamp_volume(volume)
  local n = tonumber(volume)
  if not n then
    return nil
  end
  n = math.floor(n)
  if n < M.MIN_VOLUME then
    n = M.MIN_VOLUME
  elseif n > M.MAX_VOLUME then
    n = M.MAX_VOLUME
  end
  return n
end

return M
