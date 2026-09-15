/*
 * SPDX-FileCopyrightText: 2026 Espressif Systems (Shanghai) CO LTD
 *
 * SPDX-License-Identifier: Apache-2.0
 */
#pragma once

#include "esp_err.h"
#include "display_raster.h"

typedef struct display_font_t *display_font_handle_t;

typedef struct {
    display_font_handle_t font;
    /* ASCII height 8..64; width is aspect-preserving, rounded to nearest. DFN1 uses native dimensions. */
    int font_size;
    display_color_t color;
} display_text_options_t;

/* DFN1: LE width:u16, height:u16, count:u32; sorted codepoint:u32 + row-aligned MSB-first 1bpp glyphs. */
esp_err_t display_font_create(const char *path, display_font_handle_t *ret_font);
void display_font_delete(display_font_handle_t font);
esp_err_t display_text_measure(const char *text, size_t length, const display_text_options_t *options, int *width, int *height);
esp_err_t display_text_draw(display_raster_t *r, int x, int y, const char *text, size_t length, const display_text_options_t *options);
