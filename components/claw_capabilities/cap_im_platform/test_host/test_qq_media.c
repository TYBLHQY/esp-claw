/* Included after the actual firmware media functions by run_qq_media.py. */
#include <assert.h>
#include <stdio.h>

static void check(const char *json, const char *expected)
{
    cJSON *data = cJSON_Parse(json);
    char url[256] = {0}, mime[64] = {0};
    assert(data);
    bool found = cap_im_qq_extract_image_url(data, url, sizeof(url), mime, sizeof(mime));
    assert(found == (expected != NULL));
    if (expected) {
        assert(strcmp(url, expected) == 0);
        assert(strncmp(mime, "image/", 6) == 0);
    }
    cJSON_Delete(data);
}

int main(void)
{
    const char *picture = "https://example.com/image?id=123";
    check("{\"ark_data\":{\"ark_type\":\"picture\",\"fields\":{\"jump_url\":\"https://example.com/page\",\"source_logo\":\"https://example.com/logo.png\",\"preview\":\"https://example.com/image?id=123\"}}}", picture);
    check("{\"msg_elements\":[{\"message_type\":3,\"ark_data\":{\"ark_type\":\"picture\",\"fields\":{\"preview\":\"https://example.com/image?id=123\"}}}]}", picture);
    check("{\"ark_data\":\"{\\\"ark_type\\\":\\\"picture\\\",\\\"fields\\\":{\\\"preview\\\":\\\"https://example.com/image?id=123\\\"}}\"}", picture);
    check("{\"ark_data\":{\"ark_type\":\"miniapp\",\"fields\":{\"preview\":\"https://example.com/image?id=123\",\"jump_url\":\"https://example.com/page\"}}}", NULL);
    check("{\"ark_data\":{\"ark_type\":\"picture\",\"fields\":{\"jump_url\":\"https://example.com/page\",\"source_logo\":\"https://example.com/logo.png\"}}}", NULL);
    check("{\"ark_data\":{\"ark_type\":\"picture\",\"fields\":{\"preview\":42}}}", NULL);
    check("{\"attachments\":[{\"url\":\"https://example.com/image?id=123\",\"content_type\":\"image/png\"}]}", picture);
    check("{\"ark_data\":{\"ark_type\":\"picture\",\"fields\":{\"preview\":\"file:///etc/passwd\"}}}", NULL);
    puts("PASS: picture preview, quoted/nested ARK, legacy attachment, no card-link/logo false positives");
    return 0;
}
