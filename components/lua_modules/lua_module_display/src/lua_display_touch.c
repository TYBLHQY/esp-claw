/*
 * SPDX-FileCopyrightText: 2026 Espressif Systems (Shanghai) CO LTD
 *
 * SPDX-License-Identifier: Apache-2.0
 */
#include "lua_display_touch.h"

#include "esp_err.h"
#include "lauxlib.h"

static int lua_display_touch_read(lua_State *L)
{
    display_service_session_handle_t *session = lua_touserdata(L, lua_upvalueindex(1));
    display_service_touch_snapshot_t snapshot = {0};

    if (lua_gettop(L) != 0) {
        return luaL_error(L, "display.touch.read expects no arguments");
    }
    if (session == NULL || *session == NULL) {
        return luaL_error(L, "display touch requires display.init()");
    }

    esp_err_t err = display_service_session_get_touch_snapshot(*session, &snapshot);
    if (err != ESP_OK) {
        return luaL_error(L, "display touch read failed: %s", esp_err_to_name(err));
    }

    lua_createtable(L, snapshot.count, 0);
    for (uint8_t i = 0; i < snapshot.count; i++) {
        lua_createtable(L, 0, 3);
        lua_pushinteger(L, snapshot.points[i].id);
        lua_setfield(L, -2, "id");
        lua_pushinteger(L, snapshot.points[i].x);
        lua_setfield(L, -2, "x");
        lua_pushinteger(L, snapshot.points[i].y);
        lua_setfield(L, -2, "y");
        lua_rawseti(L, -2, i + 1);
    }
    return 1;
}

void lua_display_touch_register_lua(lua_State *L, display_service_session_handle_t *session)
{
    lua_newtable(L);
    lua_pushlightuserdata(L, session);
    lua_pushcclosure(L, lua_display_touch_read, 1);
    lua_setfield(L, -2, "read");
    lua_setfield(L, -2, "touch");
}
