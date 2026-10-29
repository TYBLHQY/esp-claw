#include "cap_im_qq_internal.h"
#include "cJSON.h"
#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static unsigned calls;
static esp_err_t transport_error;
static const char *reply = "{\"data\":{\"url\":\"https://qun.qq.com/qunpro/robot/qunshare?data=hello\"}}";
static char request_body[512];

const char *esp_err_to_name(esp_err_t err) { (void)err; return "TEST_ERROR"; }

esp_err_t cap_im_qq_api_request(esp_http_client_method_t method, const char *path,
                              const char *body, char **response)
{
    assert(method == HTTP_METHOD_POST);
    assert(strcmp(path, "/v2/generate_url_link") == 0);
    assert(strlen(body) < sizeof(request_body));
    strcpy(request_body, body);
    ++calls;
    *response = reply ? strdup(reply) : NULL;
    return transport_error;
}

int main(void)
{
    char output[2048];
    cJSON *json;
    assert(cap_im_qq_generate_share_link_execute("{}", NULL, output, sizeof(output)) == ESP_OK);
    assert(calls == 1 && strcmp(request_body, "{}") == 0);
    json = cJSON_Parse(output);
    assert(strcmp(cJSON_GetObjectItem(json, "url")->valuestring,
                  "https://qun.qq.com/qunpro/robot/qunshare?data=hello") == 0);
    cJSON_Delete(json);

    assert(cap_im_qq_generate_share_link_execute("{\"callback_data\":\"source_\\\"test\\n\"}", NULL,
                                                output, sizeof(output)) == ESP_OK);
    json = cJSON_Parse(request_body);
    assert(strcmp(cJSON_GetObjectItem(json, "callback_data")->valuestring, "source_\"test\n") == 0);
    cJSON_Delete(json);
    unsigned before = calls;
    const char *invalid[] = {"broken", "[]", "null", "{\"callback_data\":42}",
        "{\"callback_data\":null}", "{\"callback_data\":\"123456789012345678901234567890123\"}",
        "{\"callback_data\":\"\xc0\xaf\"}"};
    for (unsigned i = 0; i < sizeof(invalid) / sizeof(invalid[0]); ++i)
        assert(cap_im_qq_generate_share_link_execute(invalid[i], NULL, output, sizeof(output)) == ESP_ERR_INVALID_ARG);
    assert(calls == before);
    assert(cap_im_qq_generate_share_link_execute("{\"callback_data\":\"12345678901234567890123456789012\"}",
                                                NULL, output, sizeof(output)) == ESP_OK);
    char multibyte[256] = "{\"callback_data\":\"";
    for (unsigned i = 0; i < 32; ++i) strcat(multibyte, "中");
    strcat(multibyte, "\"}");
    assert(cap_im_qq_generate_share_link_execute(multibyte, NULL, output, sizeof(output)) == ESP_OK);
    multibyte[strlen(multibyte) - 2] = '\0';
    strcat(multibyte, "中\"}");
    assert(cap_im_qq_generate_share_link_execute(multibyte, NULL, output, sizeof(output)) == ESP_ERR_INVALID_ARG);

    reply = "{\"url\":\"https://qun.qq.com/legacy\"}";
    assert(cap_im_qq_generate_share_link_execute("{}", NULL, output, sizeof(output)) == ESP_OK);
    assert(strstr(output, "legacy"));
    assert(cap_im_qq_generate_share_link_execute("{}", NULL, output, 10) == ESP_ERR_INVALID_SIZE);
    const char *bad_reply[] = {"broken", "{}", "{\"data\":{\"url\":\"javascript:alert(1)\"}}",
        "{\"data\":{\"url\":\"https://\"}}", "{\"data\":{\"url\":42}}"};
    for (unsigned i = 0; i < sizeof(bad_reply) / sizeof(bad_reply[0]); ++i) {
        reply = bad_reply[i];
        assert(cap_im_qq_generate_share_link_execute("{}", NULL, output, sizeof(output)) == ESP_ERR_INVALID_RESPONSE);
    }
    transport_error = ESP_FAIL;
    reply = "{\"code\":10044,\"message\":\"header problem\"}";
    assert(cap_im_qq_generate_share_link_execute("{}", NULL, output, sizeof(output)) == ESP_FAIL);
    assert(strstr(output, "10044"));
    reply = NULL;
    assert(cap_im_qq_generate_share_link_execute("{}", NULL, output, sizeof(output)) == ESP_FAIL);
    assert(cap_im_qq_generate_share_link_execute("{}", NULL, NULL, 0) == ESP_ERR_INVALID_ARG);
    puts("PASS: official data.url, legacy url, callback escaping/UTF-8/32-char limits, errors, full URL output");
    return 0;
}
