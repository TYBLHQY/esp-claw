#include "http_server_priv.h"

#include <stddef.h>
#include <stdlib.h>
#include <string.h>

#include "esp_crt_bundle.h"
#include "esp_https_ota.h"
#include "esp_log.h"
#include "esp_system.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"

static const char *TAG = "http_maintenance";

#define CONFIG_FIELD(field) { #field, offsetof(app_config_t, field), sizeof(((app_config_t *)0)->field) }
typedef struct { const char *name; size_t offset; size_t size; } maintenance_field_t;
static const maintenance_field_t s_fields[] = {
    CONFIG_FIELD(wifi_ssid), CONFIG_FIELD(wifi_password), CONFIG_FIELD(ap_ssid),
    CONFIG_FIELD(ap_password), CONFIG_FIELD(ap_behavior), CONFIG_FIELD(llm_api_key),
    CONFIG_FIELD(llm_backend_type), CONFIG_FIELD(llm_model), CONFIG_FIELD(llm_base_url),
    CONFIG_FIELD(llm_auth_type), CONFIG_FIELD(llm_timeout_ms), CONFIG_FIELD(llm_max_tokens),
    CONFIG_FIELD(llm_default_image_max_bytes), CONFIG_FIELD(llm_max_tokens_field),
    CONFIG_FIELD(llm_supports_tools), CONFIG_FIELD(llm_supports_vision),
    CONFIG_FIELD(llm_image_remote_url_only), CONFIG_FIELD(qq_app_id),
    CONFIG_FIELD(qq_app_secret), CONFIG_FIELD(qq_msg_type), CONFIG_FIELD(feishu_app_id),
    CONFIG_FIELD(feishu_app_secret), CONFIG_FIELD(tg_bot_token), CONFIG_FIELD(wechat_token),
    CONFIG_FIELD(wechat_base_url), CONFIG_FIELD(wechat_cdn_base_url),
    CONFIG_FIELD(wechat_account_id), CONFIG_FIELD(search_brave_key),
    CONFIG_FIELD(search_tavily_key), CONFIG_FIELD(search_http_allowlist),
    CONFIG_FIELD(enabled_cap_groups), CONFIG_FIELD(llm_visible_cap_groups),
    CONFIG_FIELD(enabled_lua_modules), CONFIG_FIELD(time_timezone),
};

static char *field_ptr(app_config_t *config, const maintenance_field_t *field)
{
    return ((char *)config) + field->offset;
}

static esp_err_t send_ok(httpd_req_t *req, const char *message)
{
    cJSON *root = cJSON_CreateObject();
    if (!root) {
        httpd_resp_send_500(req);
        return ESP_ERR_NO_MEM;
    }
    cJSON_AddBoolToObject(root, "ok", true);
    http_server_json_add_string(root, "message", message);
    return http_server_send_json_response(req, root);
}

static esp_err_t config_backup_handler(httpd_req_t *req)
{
    http_server_ctx_t *ctx = http_server_ctx();
    app_config_t *config = NULL;
    cJSON *root;

    if (!http_server_require_auth(req)) return ESP_OK;
    config = calloc(1, sizeof(*config));
    if (!config) {
        httpd_resp_send_500(req);
        return ESP_ERR_NO_MEM;
    }
    if (ctx->services.load_config(config) != ESP_OK) {
        free(config);
        return httpd_resp_send_err(req, HTTPD_500_INTERNAL_SERVER_ERROR, "Failed to load config");
    }
    root = cJSON_CreateObject();
    if (!root) {
        free(config);
        httpd_resp_send_500(req);
        return ESP_ERR_NO_MEM;
    }
    cJSON_AddStringToObject(root, "format", "esp-claw-config-v1");
    for (size_t i = 0; i < sizeof(s_fields) / sizeof(s_fields[0]); i++) {
        http_server_json_add_string(root, s_fields[i].name, field_ptr(config, &s_fields[i]));
    }
    httpd_resp_set_hdr(req, "Content-Disposition", "attachment; filename=esp-claw-config.json");
    esp_err_t err = http_server_send_json_response(req, root);
    free(config);
    return err;
}

static esp_err_t config_restore_handler(httpd_req_t *req)
{
    http_server_ctx_t *ctx = http_server_ctx();
    app_config_t *config = NULL;
    cJSON *root = NULL;
    cJSON *source;
    size_t applied = 0;
    const char *wifi_error = NULL;

    if (!http_server_require_auth(req)) return ESP_OK;
    config = calloc(1, sizeof(*config));
    if (!config) {
        httpd_resp_send_500(req);
        return ESP_ERR_NO_MEM;
    }
    if (ctx->services.load_config(config) != ESP_OK ||
            http_server_parse_json_body(req, &root) != ESP_OK || !root) {
        cJSON_Delete(root);
        free(config);
        return httpd_resp_send_err(req, HTTPD_400_BAD_REQUEST, "Invalid backup JSON");
    }
    source = cJSON_IsObject(cJSON_GetObjectItemCaseSensitive(root, "config")) ?
             cJSON_GetObjectItemCaseSensitive(root, "config") : root;
    for (size_t i = 0; i < sizeof(s_fields) / sizeof(s_fields[0]); i++) {
        cJSON *item = cJSON_GetObjectItemCaseSensitive(source, s_fields[i].name);
        if (cJSON_IsString(item)) {
            strlcpy(field_ptr(config, &s_fields[i]), item->valuestring, s_fields[i].size);
            applied++;
        }
    }
    cJSON_Delete(root);
    if (!applied || app_config_validate_wifi(config, &wifi_error) != ESP_OK) {
        free(config);
        return httpd_resp_send_err(req, HTTPD_400_BAD_REQUEST,
                                   wifi_error ? wifi_error : "No recognised config fields");
    }
    if (ctx->services.save_config(config) != ESP_OK) {
        free(config);
        return httpd_resp_send_err(req, HTTPD_500_INTERNAL_SERVER_ERROR, "Failed to restore config");
    }
    free(config);
    return send_ok(req, "Configuration restored. Restart the device to apply all changes.");
}

typedef struct { char url[384]; } ota_args_t;

static void ota_task(void *arg)
{
    ota_args_t *args = arg;
    esp_http_client_config_t http_config = {
        .url = args->url,
        .crt_bundle_attach = esp_crt_bundle_attach,
        .timeout_ms = 30000,
    };
    esp_https_ota_config_t ota_config = { .http_config = &http_config };
    esp_err_t err = esp_https_ota(&ota_config);
    ESP_LOGI(TAG, "OTA finished: %s", esp_err_to_name(err));
    if (err == ESP_OK) {
        esp_restart();
    }
    free(args);
    vTaskDelete(NULL);
}

static esp_err_t ota_handler(httpd_req_t *req)
{
    cJSON *root = NULL;
    const char *url;
    ota_args_t *args;

    if (!http_server_require_auth(req)) return ESP_OK;
    if (http_server_parse_json_body(req, &root) != ESP_OK || !root) {
        cJSON_Delete(root);
        return httpd_resp_send_err(req, HTTPD_400_BAD_REQUEST, "Invalid JSON body");
    }
    url = cJSON_GetStringValue(cJSON_GetObjectItemCaseSensitive(root, "url"));
    if (!url || strncmp(url, "https://", 8) != 0 || strlen(url) >= sizeof(args->url)) {
        cJSON_Delete(root);
        return httpd_resp_send_err(req, HTTPD_400_BAD_REQUEST, "OTA URL must be an HTTPS URL");
    }
    args = calloc(1, sizeof(*args));
    if (!args) {
        cJSON_Delete(root);
        httpd_resp_send_500(req);
        return ESP_ERR_NO_MEM;
    }
    strlcpy(args->url, url, sizeof(args->url));
    cJSON_Delete(root);
    if (xTaskCreate(ota_task, "esp_claw_ota", 8192, args, 4, NULL) != pdPASS) {
        free(args);
        return httpd_resp_send_err(req, HTTPD_500_INTERNAL_SERVER_ERROR, "OTA task unavailable");
    }
    return send_ok(req, "OTA started. The device will restart after verification.");
}

esp_err_t http_server_register_maintenance_routes(httpd_handle_t server)
{
    const httpd_uri_t handlers[] = {
        { .uri = "/api/config/backup", .method = HTTP_GET, .handler = config_backup_handler },
        { .uri = "/api/config/restore", .method = HTTP_POST, .handler = config_restore_handler },
        { .uri = "/api/ota", .method = HTTP_POST, .handler = ota_handler },
    };
    for (size_t i = 0; i < sizeof(handlers) / sizeof(handlers[0]); i++) {
        esp_err_t err = httpd_register_uri_handler(server, &handlers[i]);
        if (err != ESP_OK) return err;
    }
    return ESP_OK;
}
