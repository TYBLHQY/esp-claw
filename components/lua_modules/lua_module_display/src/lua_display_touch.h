/*
 * SPDX-FileCopyrightText: 2026 Espressif Systems (Shanghai) CO LTD
 *
 * SPDX-License-Identifier: Apache-2.0
 */
#pragma once

#include "display_service.h"
#include "lua.h"

void lua_display_touch_register_lua(lua_State *L, display_service_session_handle_t *session);
