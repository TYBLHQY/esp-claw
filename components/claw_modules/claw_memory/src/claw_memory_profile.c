/*
 * SPDX-FileCopyrightText: 2026 Espressif Systems (Shanghai) CO LTD
 *
 * SPDX-License-Identifier: Apache-2.0
 */
#include "claw_memory_internal.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static esp_err_t claw_memory_profile_dup_nonempty(const char *path, char **out_text)
{
    char *text = NULL;
    esp_err_t err;

    if (!path || !out_text) {
        return ESP_ERR_INVALID_ARG;
    }

    *out_text = NULL;
    err = read_file_dup(path, &text);
    if (err != ESP_OK) {
        return err;
    }

    trim_whitespace(text);
    if (!text[0]) {
        free(text);
        return ESP_ERR_NOT_FOUND;
    }

    *out_text = text;
    return ESP_OK;
}

static esp_err_t claw_memory_profile_migrate_name(const char *path)
{
    const char *old_name = "ESP-Claw";
    const char *new_name = "CopilotLee";
    char *text = NULL;
    char *cursor;
    size_t old_len = strlen(old_name);
    size_t new_len = strlen(new_name);
    size_t count = 0;
    size_t text_len;
    size_t new_size;
    char *updated = NULL;
    char *out;
    esp_err_t err;

    err = read_file_dup(path, &text);
    if (err != ESP_OK) {
        return err == ESP_ERR_NOT_FOUND ? ESP_OK : err;
    }
    text_len = strlen(text);
    for (cursor = text; (cursor = strstr(cursor, old_name)) != NULL; cursor += old_len) {
        count++;
    }
    if (count == 0) {
        free(text);
        return ESP_OK;
    }
    if (count > (SIZE_MAX - text_len) / (new_len > old_len ? new_len - old_len : 1)) {
        free(text);
        return ESP_ERR_INVALID_SIZE;
    }
    new_size = text_len + count * (new_len - old_len) + 1;
    updated = malloc(new_size);
    if (!updated) {
        free(text);
        return ESP_ERR_NO_MEM;
    }
    out = updated;
    cursor = text;
    while (*cursor) {
        char *match = strstr(cursor, old_name);
        size_t chunk = match ? (size_t)(match - cursor) : strlen(cursor);

        memcpy(out, cursor, chunk);
        out += chunk;
        if (!match) {
            break;
        }
        memcpy(out, new_name, new_len);
        out += new_len;
        cursor = match + old_len;
    }
    *out = '\0';
    err = write_file_text(path, updated);
    free(updated);
    free(text);
    return err;
}

esp_err_t claw_memory_profile_init_defaults(void)
{
    static const char *const default_soul =
        "# Soul\n\n"
        "Describe who the agent is, what it values, and how it prefers to help.\n";
    static const char *const default_identity =
        "# Identity Card\n\n"
        "- Name:\n"
        "- Role:\n"
        "- Personality:\n"
        "- Strengths:\n";
    static const char *const default_user =
        "# User\n\n"
        "Write down who the current user is, their preferences, and any standing guidance.\n";

    if (!s_memory.memory_root_dir[0]) {
        return ESP_ERR_INVALID_STATE;
    }

    if (claw_memory_join_path(s_memory.soul_path,
                              sizeof(s_memory.soul_path),
                              s_memory.memory_root_dir,
                              CLAW_MEMORY_SOUL_FILE) != ESP_OK ||
        claw_memory_join_path(s_memory.identity_path,
                              sizeof(s_memory.identity_path),
                              s_memory.memory_root_dir,
                              CLAW_MEMORY_IDENTITY_FILE) != ESP_OK ||
        claw_memory_join_path(s_memory.user_path,
                              sizeof(s_memory.user_path),
                              s_memory.memory_root_dir,
                              CLAW_MEMORY_USER_FILE) != ESP_OK) {
        return ESP_ERR_INVALID_SIZE;
    }

    if (ensure_file_with_default(s_memory.soul_path, default_soul) != ESP_OK ||
            ensure_file_with_default(s_memory.identity_path, default_identity) != ESP_OK ||
            ensure_file_with_default(s_memory.user_path, default_user) != ESP_OK) {
        return ESP_FAIL;
    }

    /* Preserve user profile files while updating the previous built-in name. */
    if (claw_memory_profile_migrate_name(s_memory.soul_path) != ESP_OK ||
            claw_memory_profile_migrate_name(s_memory.identity_path) != ESP_OK) {
        return ESP_FAIL;
    }

    return ESP_OK;
}

static esp_err_t claw_memory_profile_collect(const claw_core_request_t *request,
                                             claw_core_context_t *out_context,
                                             void *user_ctx)
{
    const char *paths[3] = {
        s_memory.user_path,
        s_memory.soul_path,
        s_memory.identity_path,
    };
    const char *titles[3] = {
        "User Profile",
        "Agent Soul",
        "Agent Identity Card",
    };
    char *parts[3] = {0};
    char *content = NULL;
    size_t total_len = 0;
    size_t off = 0;
    size_t i;

    (void)request;
    (void)user_ctx;

    if (!out_context || !s_memory.initialized) {
        return ESP_ERR_INVALID_ARG;
    }
    memset(out_context, 0, sizeof(*out_context));
    for (i = 0; i < 3; i++) {
        if (claw_memory_profile_dup_nonempty(paths[i], &parts[i]) == ESP_OK) {
            total_len += strlen(titles[i]) + strlen(paths[i]) + strlen(parts[i]) + 12;
        }
    }

    if (total_len == 0) {
        return ESP_ERR_NOT_FOUND;
    }

    content = calloc(1, total_len + 1);
    if (!content) {
        for (i = 0; i < 3; i++) {
            free(parts[i]);
        }
        return ESP_ERR_NO_MEM;
    }

    for (i = 0; i < 3; i++) {
        if (!parts[i]) {
            continue;
        }
        off += snprintf(content + off,
                        total_len + 1 - off,
                        "%s (%s):\n%s\n\n",
                        titles[i],
                        paths[i],
                        parts[i]);
    }

    for (i = 0; i < 3; i++) {
        free(parts[i]);
    }

    out_context->kind = CLAW_CORE_CONTEXT_KIND_SYSTEM_PROMPT;
    out_context->content = content;
    return ESP_OK;
}

const claw_core_context_provider_t claw_memory_profile_provider = {
    .name = "Editable Profile Memory",
    .collect = claw_memory_profile_collect,
    .user_ctx = NULL,
};
