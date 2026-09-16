/*
 * SPDX-FileCopyrightText: 2026 Espressif Systems (Shanghai) CO LTD
 *
 * SPDX-License-Identifier: Apache-2.0
 */
#include "display_raster.h"

#include <math.h>
#include <stdlib.h>
#include <string.h>

static int64_t max64(int64_t a, int64_t b) { return a > b ? a : b; }
static int64_t min64(int64_t a, int64_t b) { return a < b ? a : b; }

void display_raster_span(display_raster_t *r, int64_t x0, int64_t x1, int64_t y, display_color_t c)
{
    if (!c.a) return;
    y += r->ty;
    if (y < r->y0 || y >= r->y1) return;
    x0 = max64(x0 + r->tx, r->x0);
    x1 = min64(x1 + r->tx, r->x1);
    if (x0 >= x1) return;
    uint8_t *p = r->pixels + (size_t)y * r->stride + (size_t)x0 * r->bpp;
    if (r->bpp == 2) {
        uint16_t opaque = display_color_to_rgb565(c);
        for (int64_t x = x0; x < x1; x++, p += 2) {
            uint16_t v = opaque;
            if (c.a != 255) {
                memcpy(&v, p, sizeof(v));
                v = display_color_blend_rgb565(v, c);
            }
            memcpy(p, &v, sizeof(v));
        }
    } else {
        uint32_t opaque = display_color_to_rgb888(c);
        for (int64_t x = x0; x < x1; x++, p += 3) {
            uint32_t v = c.a == 255 ? opaque : display_color_blend_rgb888(((uint32_t)p[0] << 16) | ((uint32_t)p[1] << 8) | p[2], c);
            p[0] = v >> 16;
            p[1] = v >> 8;
            p[2] = v;
        }
    }
    display_dirty_mark(r->dirty, (int)x0, (int)y, (int)(x1 - x0), 1);
}

void display_raster_pixel(display_raster_t *r, int64_t x, int64_t y, display_color_t c)
{
    display_raster_span(r, x, x + 1, y, c);
}

void display_raster_fill_rect(display_raster_t *r, int x, int y, int w, int h, display_color_t c)
{
    if (w <= 0 || h <= 0 || !c.a) return;
    int64_t bottom = min64((int64_t)y + h, (int64_t)r->y1 - r->ty);
    for (int64_t row = max64(y, (int64_t)r->y0 - r->ty); row < bottom; row++) {
        display_raster_span(r, x, (int64_t)x + w, row, c);
    }
}

void display_raster_stroke_rect(display_raster_t *r, int x, int y, int w, int h, display_color_t c)
{
    if (w <= 0 || h <= 0 || !c.a) return;
    display_raster_span(r, x, (int64_t)x + w, y, c);
    if (h > 1) display_raster_span(r, x, (int64_t)x + w, (int64_t)y + h - 1, c);
    int64_t bottom = min64((int64_t)y + h - 1, (int64_t)r->y1 - r->ty);
    for (int64_t row = max64((int64_t)y + 1, (int64_t)r->y0 - r->ty); row < bottom; row++) {
        display_raster_pixel(r, x, row, c);
        if (w > 1) display_raster_pixel(r, (int64_t)x + w - 1, row, c);
    }
}

static bool clip_edge(double p, double q, double *lo, double *hi)
{
    if (p == 0) return q >= 0;
    double t = q / p;
    if (p < 0) {
        if (t > *hi) return false;
        if (t > *lo) *lo = t;
    } else {
        if (t < *lo) return false;
        if (t < *hi) *hi = t;
    }
    return true;
}

void display_raster_line(display_raster_t *r, int x0, int y0, int x1, int y1, display_color_t c)
{
    if (!c.a || r->x0 >= r->x1 || r->y0 >= r->y1) return;
    /* Clip before stepping so off-screen coordinates cannot create unbounded work. */
    double dx = (double)x1 - x0, dy = (double)y1 - y0, lo = 0, hi = 1;
    if (!clip_edge(-dx, (double)x0 + r->tx - r->x0, &lo, &hi) ||
        !clip_edge(dx, (double)r->x1 - 1 - r->tx - x0, &lo, &hi) ||
        !clip_edge(-dy, (double)y0 + r->ty - r->y0, &lo, &hi) ||
        !clip_edge(dy, (double)r->y1 - 1 - r->ty - y0, &lo, &hi)) return;
    int64_t ax = llround(x0 + lo * dx), ay = llround(y0 + lo * dy);
    int64_t bx = llround(x0 + hi * dx), by = llround(y0 + hi * dy);
    int64_t sx = ax < bx ? 1 : -1, sy = ay < by ? 1 : -1;
    int64_t adx = llabs(bx - ax), ady = -llabs(by - ay), err = adx + ady;
    for (;;) {
        display_raster_pixel(r, ax, ay, c);
        if (ax == bx && ay == by) break;
        int64_t e = err * 2;
        if (e >= ady) { err += ady; ax += sx; }
        if (e <= adx) { err += adx; ay += sy; }
    }
}

static int64_t circle_extent(int radius, int64_t dy)
{
    /* Squared integer radii fit int64_t; double sqrt avoids a radius-sized loop. */
    int64_t n = (int64_t)radius * radius - dy * dy;
    if (n < 0) return -1;
    int64_t x = (int64_t)sqrt((double)n);
    while (x * x > n) x--;
    while ((x + 1) * (x + 1) <= n) x++;
    return x;
}

static void circle(display_raster_t *r, int cx, int cy, int radius, bool fill, display_color_t c)
{
    if (radius < 0 || !c.a) return;
    int64_t bottom = min64((int64_t)cy + radius, (int64_t)r->y1 - r->ty - 1);
    for (int64_t y = max64((int64_t)cy - radius, (int64_t)r->y0 - r->ty); y <= bottom; y++) {
        int64_t outer = circle_extent(radius, y - cy);
        int64_t inner = !fill && radius > 0 ? circle_extent(radius - 1, y - cy) : -1;
        if (inner < 0) display_raster_span(r, (int64_t)cx - outer, (int64_t)cx + outer + 1, y, c);
        else {
            display_raster_span(r, (int64_t)cx - outer, (int64_t)cx - inner, y, c);
            display_raster_span(r, (int64_t)cx + inner + 1, (int64_t)cx + outer + 1, y, c);
        }
    }
}

void display_raster_fill_circle(display_raster_t *r, int cx, int cy, int radius, display_color_t c)
{
    circle(r, cx, cy, radius, true, c);
}

void display_raster_stroke_circle(display_raster_t *r, int cx, int cy, int radius, display_color_t c)
{
    circle(r, cx, cy, radius, false, c);
}

void display_raster_arc(display_raster_t *r, int cx, int cy, int radius, double start, double end, display_color_t c)
{
    if (radius < 0 || !c.a || !isfinite(start) || !isfinite(end)) return;
    double sweep = end - start;
    if (fabs(sweep) >= 360) { circle(r, cx, cy, radius, false, c); return; }
    sweep = fmod(sweep + 360, 360);
    if (sweep == 0) return;
    const double radians = 0.017453292519943295;
    start = fmod(start, 360) * radians;
    double finish = start + sweep * radians;
    double sx = cos(start), sy = sin(start), ex = cos(finish), ey = sin(finish);
    int64_t bottom = min64((int64_t)cy + radius, (int64_t)r->y1 - r->ty - 1);
    for (int64_t y = max64((int64_t)cy - radius, (int64_t)r->y0 - r->ty); y <= bottom; y++) {
        int64_t outer = circle_extent(radius, y - cy);
        int64_t inner = radius > 0 ? circle_extent(radius - 1, y - cy) : -1;
        int64_t left = max64((int64_t)cx - outer, (int64_t)r->x0 - r->tx);
        int64_t right = min64((int64_t)cx + outer, (int64_t)r->x1 - r->tx - 1);
        for (int64_t x = left; x <= right; x++) {
            if (inner >= 0 && x >= (int64_t)cx - inner && x <= (int64_t)cx + inner) {
                x = (int64_t)cx + inner;
                continue;
            }
            double dx = x - cx, dy = y - cy;
            bool after = sx * dy - sy * dx >= -1e-9;
            bool before = dx * ey - dy * ex >= -1e-9;
            if (sweep <= 180 ? (after && before) : (after || before)) display_raster_pixel(r, x, y, c);
        }
    }
}

static int64_t round_inset(int w, int h, int radius, int64_t row)
{
    radius = (int)min64(radius, min64((w - 1) / 2, (h - 1) / 2));
    if (radius <= 0) return 0;
    int64_t dy = row < radius ? radius - row : row - (h - radius - 1);
    return dy > 0 ? radius - circle_extent(radius, dy) : 0;
}

static void round_rect(display_raster_t *r, int x, int y, int w, int h, int radius, bool fill, display_color_t c)
{
    if (w <= 0 || h <= 0 || radius < 0 || !c.a) return;
    radius = (int)min64(radius, min64((w - 1) / 2, (h - 1) / 2));
    int64_t bottom = min64((int64_t)y + h, (int64_t)r->y1 - r->ty);
    for (int64_t row = max64(y, (int64_t)r->y0 - r->ty); row < bottom; row++) {
        int64_t inset = round_inset(w, h, radius, row - y);
        int64_t left = (int64_t)x + inset, right = (int64_t)x + w - inset;
        if (fill || row == y || row == (int64_t)y + h - 1 || w <= 2 || h <= 2) {
            display_raster_span(r, left, right, row, c);
        } else {
            int64_t inner = 1 + round_inset(w - 2, h - 2, radius > 0 ? radius - 1 : 0, row - y - 1);
            display_raster_span(r, left, (int64_t)x + inner, row, c);
            display_raster_span(r, (int64_t)x + w - inner, right, row, c);
        }
    }
}

void display_raster_fill_round_rect(display_raster_t *r, int x, int y, int w, int h, int radius, display_color_t c)
{
    round_rect(r, x, y, w, h, radius, true, c);
}

void display_raster_stroke_round_rect(display_raster_t *r, int x, int y, int w, int h, int radius, display_color_t c)
{
    round_rect(r, x, y, w, h, radius, false, c);
}

void display_raster_fill_triangle(display_raster_t *r, int x0, int y0, int x1, int y1, int x2, int y2, display_color_t c)
{
    if (!c.a) return;
    int xs[3] = {x0, x1, x2}, ys[3] = {y0, y1, y2};
    int64_t top = max64(min64(y0, min64(y1, y2)), (int64_t)r->y0 - r->ty);
    int64_t bottom = min64(max64(y0, max64(y1, y2)), (int64_t)r->y1 - r->ty - 1);
    for (int64_t y = top; y <= bottom; y++) {
        double left = INFINITY, right = -INFINITY;
        for (int i = 0; i < 3; i++) {
            int j = (i + 1) % 3;
            if (y < min64(ys[i], ys[j]) || y > max64(ys[i], ys[j])) continue;
            if (ys[i] == ys[j]) {
                left = fmin(left, min64(xs[i], xs[j]));
                right = fmax(right, max64(xs[i], xs[j]));
            } else {
                double x = xs[i] + (double)(y - ys[i]) * ((double)xs[j] - xs[i]) / ((double)ys[j] - ys[i]);
                left = fmin(left, x);
                right = fmax(right, x);
            }
        }
        if (left <= right) display_raster_span(r, (int64_t)ceil(left), (int64_t)floor(right) + 1, y, c);
    }
}
