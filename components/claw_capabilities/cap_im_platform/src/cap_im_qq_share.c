/* SPDX-License-Identifier: Apache-2.0 */
#include "cap_im_qq_internal.h"

#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "cJSON.h"

#define QQ_SHARE_CALLBACK_MAX_CHARS 32
#define QQ_SHARE_API_PATH "/v2/generate_url_link"

/* The documented limit is characters, not UTF-8 bytes. Also reject malformed
 * UTF-8 rather than sending a truncated or ambiguously encoded callback. */
static bool share_callback_valid(const char *text)
{
    const unsigned char *p = (const unsigned char *)text;
    unsigned count = 0;

    while (*p) {
        unsigned width;
        if (++count > QQ_SHARE_CALLBACK_MAX_CHARS) return false;
        if (*p < 0x80) width = 1;
        else if (*p >= 0xc2 && *p <= 0xdf) width = 2;
        else if (*p >= 0xe0 && *p <= 0xef) width = 3;
        else if (*p >= 0xf0 && *p <= 0xf4) width = 4;
        else return false;
        for (unsigned i = 1; i < width; ++i) {
            if (p[i] < 0x80 || p[i] > 0xbf) return false;
        }
        if ((*p == 0xe0 && p[1] < 0xa0) || (*p == 0xed && p[1] > 0x9f) ||
            (*p == 0xf0 && p[1] < 0x90) || (*p == 0xf4 && p[1] > 0x8f)) return false;
        p += width;
    }
    return true;
}

esp_err_t cap_im_qq_generate_share_link_execute(const char *input_json,
                                              const claw_cap_call_context_t *ctx,
                                              char *output,
                                              size_t output_size)
{
    cJSON *input = NULL, *body = NULL, *response = NULL, *result = NULL;
    cJSON *callback, *data, *url;
    char *body_json = NULL, *response_json = NULL, *result_json = NULL;
    esp_err_t err = ESP_ERR_NO_MEM;
    (void)ctx;

    if (!output || output_size == 0) return ESP_ERR_INVALID_ARG;
    output[0] = '\0';
    input = cJSON_Parse(input_json ? input_json : "{}");
    if (!cJSON_IsObject(input)) {
        snprintf(output, output_size, "Error: input must be a JSON object");
        err = ESP_ERR_INVALID_ARG;
        goto cleanup;
    }
    callback = cJSON_GetObjectItemCaseSensitive(input, "callback_data");
    if (callback && (!cJSON_IsString(callback) || !share_callback_valid(callback->valuestring))) {
        snprintf(output, output_size, "Error: callback_data must be valid UTF-8, at most 32 characters");
        err = ESP_ERR_INVALID_ARG;
        goto cleanup;
    }
    body = cJSON_CreateObject();
    if (!body || (callback && !cJSON_AddStringToObject(body, "callback_data", callback->valuestring))) goto cleanup;
    body_json = cJSON_PrintUnformatted(body);
    if (!body_json) goto cleanup;
    err = cap_im_qq_api_request(HTTP_METHOD_POST, QQ_SHARE_API_PATH, body_json, &response_json);
    if (err != ESP_OK) {
        snprintf(output, output_size, "Error: QQ share link request failed (%s): %.512s",
                 esp_err_to_name(err), response_json ? response_json : "no response body");
        goto cleanup;
    }
    response = cJSON_Parse(response_json ? response_json : "");
    data = cJSON_GetObjectItemCaseSensitive(response, "data");
    url = cJSON_GetObjectItemCaseSensitive(data, "url");
    /* Current official schema is data.url. Earlier responses used root.url. */
    if (!cJSON_IsString(url)) url = cJSON_GetObjectItemCaseSensitive(response, "url");
    if (!cJSON_IsString(url) || !url->valuestring ||
            strncmp(url->valuestring, "https://", 8) != 0 || !url->valuestring[8]) {
        snprintf(output, output_size, "Error: QQ returned no valid HTTPS share URL: %.512s",
                 response_json ? response_json : "empty response");
        err = ESP_ERR_INVALID_RESPONSE;
        goto cleanup;
    }
    result = cJSON_CreateObject();
    if (!result || !cJSON_AddStringToObject(result, "url", url->valuestring)) {
        err = ESP_ERR_NO_MEM;
        goto cleanup;
    }
    result_json = cJSON_PrintUnformatted(result);
    if (!result_json) {
        err = ESP_ERR_NO_MEM;
        goto cleanup;
    }
    if (strlen(result_json) >= output_size) {
        snprintf(output, output_size, "Error: output buffer too small for the complete share URL");
        err = ESP_ERR_INVALID_SIZE;
        goto cleanup;
    }
    memcpy(output, result_json, strlen(result_json) + 1);

cleanup:
    if (err == ESP_ERR_NO_MEM) snprintf(output, output_size, "Error: out of memory generating QQ share link");
    cJSON_Delete(input);
    cJSON_Delete(body);
    cJSON_Delete(response);
    cJSON_Delete(result);
    free(body_json);
    free(response_json);
    free(result_json);
    return err;
}
