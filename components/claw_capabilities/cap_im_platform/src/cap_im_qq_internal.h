/* SPDX-License-Identifier: Apache-2.0 */
#pragma once

#include "claw_cap.h"
#include "esp_http_client.h"

/* Private QQ transport. When supplied, *out_response is allocated for the
 * caller to free, including HTTP error responses. Authentication stays inside
 * the QQ module; tool handlers must never expose its token or secret. */
esp_err_t cap_im_qq_api_request(esp_http_client_method_t method,
                              const char *path,
                              const char *body_json,
                              char **out_response);

esp_err_t cap_im_qq_generate_share_link_execute(const char *input_json,
                                              const claw_cap_call_context_t *ctx,
                                              char *output,
                                              size_t output_size);
