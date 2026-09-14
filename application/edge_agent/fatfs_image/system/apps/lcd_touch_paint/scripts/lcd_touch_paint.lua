local bm = require("board_manager")
local display = require("display")
local delay = require("delay")

local function rgb(r, g, b)
    return { r = r, g = g, b = b }
end

local panel_handle, io_handle, width, height, panel_if = bm.get_display_lcd_params("display_lcd")
if not panel_handle then
    print("[lcd_touch_paint] ERROR: get_display_lcd_params(display_lcd) failed: " .. tostring(io_handle))
    return
end

local ok, err = pcall(display.init, panel_handle, io_handle, width, height, panel_if)
if not ok then
    print("[lcd_touch_paint] ERROR: init failed: " .. tostring(err))
    return
end

local screen_created = true

local function cleanup()
    if screen_created then
        pcall(display.end_frame)
        pcall(display.deinit)
        screen_created = false
    end
end

width = display.width
height = display.height

if width <= 0 or height <= 0 then
    print("[lcd_touch_paint] ERROR: invalid display size after init")
    cleanup()
    return
end

local BG_R, BG_G, BG_B = 250, 246, 238
local INK_R, INK_G, INK_B = 24, 74, 126
local ACCENT_R, ACCENT_G, ACCENT_B = 210, 88, 52
local TEXT_R, TEXT_G, TEXT_B = 32, 36, 42
local BG_COLOR = rgb(BG_R, BG_G, BG_B)
local INK_COLOR = rgb(INK_R, INK_G, INK_B)
local ACCENT_COLOR = rgb(ACCENT_R, ACCENT_G, ACCENT_B)
local TEXT_COLOR = rgb(TEXT_R, TEXT_G, TEXT_B)

local CLEAR_W = 74
local CLEAR_H = 32
local CLEAR_X = math.floor((width - CLEAR_W) / 2)
local CLEAR_Y = 8
local EXIT_W = 74
local EXIT_H = 32
local EXIT_X = math.floor((width - EXIT_W) / 2)
local EXIT_Y = CLEAR_Y + CLEAR_H + 6
local BRUSH_R = 4
local POLL_MS = 33

local function draw_ui()
    display.fill_rect(CLEAR_X, CLEAR_Y, CLEAR_W, CLEAR_H, ACCENT_COLOR)
    display.draw_rect(CLEAR_X, CLEAR_Y, CLEAR_W, CLEAR_H, rgb(120, 30, 12))
    display.draw_text_aligned(CLEAR_X, CLEAR_Y, CLEAR_W, CLEAR_H, "CLEAR", {
        color = "white",
        font_size = 16,
        align = "center",
        valign = "middle",
        bg = ACCENT_COLOR,
    })
    display.fill_rect(EXIT_X, EXIT_Y, EXIT_W, EXIT_H, rgb(70, 78, 88))
    display.draw_rect(EXIT_X, EXIT_Y, EXIT_W, EXIT_H, rgb(30, 34, 40))
    display.draw_text_aligned(EXIT_X, EXIT_Y, EXIT_W, EXIT_H, "EXIT", {
        color = "white",
        font_size = 16,
        align = "center",
        valign = "middle",
        bg = rgb(70, 78, 88),
    })
    display.draw_text_aligned(0, 84, width, 24, "LCD Touch Paint", {
        color = TEXT_COLOR,
        font_size = 20,
        align = "center",
        valign = "middle",
        bg = BG_COLOR,
    })
    display.draw_text_aligned(0, 108, width, 18, "draw with finger, tap CLEAR to wipe", {
        color = rgb(90, 96, 104),
        font_size = 12,
        align = "center",
        valign = "middle",
        bg = BG_COLOR,
    })
    display.present()
end

local function inside_clear_button(x, y)
    return x >= CLEAR_X and x < (CLEAR_X + CLEAR_W) and y >= CLEAR_Y and y < (CLEAR_Y + CLEAR_H)
end

local function inside_exit_button(x, y)
    return x >= EXIT_X and x < (EXIT_X + EXIT_W) and y >= EXIT_Y and y < (EXIT_Y + EXIT_H)
end

local function stamp_brush(x, y)
    display.fill_circle(x, y, BRUSH_R, INK_COLOR)
    display.present()
end

display.begin_frame({ clear = true, color = BG_COLOR })
draw_ui()

print("[lcd_touch_paint] ready")
print("[lcd_touch_paint] drag on the LCD to paint, tap CLEAR to wipe, tap EXIT to quit")

local function find_point(points, id)
    for _, point in ipairs(points) do
        if point.id == id then
            return point
        end
    end
end

local run_ok, run_err = xpcall(function()
    local active_id = nil
    local drawing = false
    local last_x, last_y = 0, 0

    while true do
        local points = display.touch.read()
        local point = active_id ~= nil and find_point(points, active_id) or nil

        if active_id == nil and points[1] ~= nil then
            point = points[1]
            active_id = point.id
            if inside_exit_button(point.x, point.y) then
                return
            elseif inside_clear_button(point.x, point.y) then
                display.clear(BG_COLOR)
                draw_ui()
            else
                drawing = true
                last_x, last_y = point.x, point.y
                stamp_brush(point.x, point.y)
            end
        elseif point == nil then
            active_id = nil
            drawing = false
        elseif drawing then
            if inside_clear_button(point.x, point.y) or inside_exit_button(point.x, point.y) then
                drawing = false
            elseif point.x ~= last_x or point.y ~= last_y then
                last_x, last_y = point.x, point.y
                stamp_brush(point.x, point.y)
            end
        end

        delay.delay_ms(POLL_MS)
    end
end, debug.traceback)

cleanup()
if not run_ok then
    print("[lcd_touch_paint] ERROR: " .. tostring(run_err))
end
print("[lcd_touch_paint] done")
