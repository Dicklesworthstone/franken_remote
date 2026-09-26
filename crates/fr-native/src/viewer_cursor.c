/* The LOCAL pointer image over the client's own viewer window (plan 11.4).
 * Owns ONE XCB connection, separate from the renderer's window connection and
 * from input capture. It selects only Enter/Leave on that window (no grabs,
 * no input, no injection) and sets the window's cursor attribute to an ARGB
 * RENDER cursor built from an already validated, premultiplied image. Every
 * change is a checked request, so success means the X server processed it.
 * No callbacks, Xlib globals, Rust pointers or peer strings. */
#include <xcb/xcb.h>
#include <xcb/render.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

struct fr_viewer_cursor {
    xcb_connection_t *c;
    xcb_window_t window;
    uint16_t width, height;
    xcb_render_pictformat_t argb;
    uint8_t byte_order;
    xcb_cursor_t current; /* our cursor now defined on the window, or NONE */
};
#define FR_CURSOR_MAX_SIDE 256u
#define FR_CURSOR_BAND_BYTES (64u * 1024u)

static int checked(struct fr_viewer_cursor *h, xcb_void_cookie_t cookie) {
    xcb_generic_error_t *e = xcb_request_check(h->c, cookie);
    int ok = !e && !xcb_connection_has_error(h->c);
    free(e); return ok;
}
static int argb32(struct fr_viewer_cursor *h) {
    xcb_generic_error_t *e = NULL;
    xcb_render_query_version_reply_t *v = xcb_render_query_version_reply(h->c,
        xcb_render_query_version(h->c, 0, 11), &e);
    /* RenderCreateCursor exists from RENDER 0.5. */
    int ok = v && !e && v->major_version == 0 && v->minor_version >= 5;
    free(v); free(e); if (!ok) return 0;
    xcb_render_query_pict_formats_reply_t *f = xcb_render_query_pict_formats_reply(h->c,
        xcb_render_query_pict_formats(h->c), &e);
    if (!f || e) { free(f); free(e); return 0; }
    xcb_render_pictforminfo_iterator_t it = xcb_render_query_pict_formats_formats_iterator(f);
    for (; it.rem; xcb_render_pictforminfo_next(&it)) {
        const xcb_render_pictforminfo_t *p = it.data;
        const xcb_render_directformat_t *d = &p->direct;
        if (p->type == XCB_RENDER_PICT_TYPE_DIRECT && p->depth == 32 &&
            d->alpha_shift == 24 && d->alpha_mask == 0xff &&
            d->red_shift == 16 && d->red_mask == 0xff &&
            d->green_shift == 8 && d->green_mask == 0xff &&
            d->blue_shift == 0 && d->blue_mask == 0xff) {
            h->argb = p->id;
            break;
        }
    }
    free(f);
    return h->argb != 0;
}
static int inside(const xcb_query_pointer_reply_t *p, uint16_t width, uint16_t height) {
    return p && p->same_screen && p->win_x >= 0 && p->win_y >= 0 &&
        p->win_x < width && p->win_y < height;
}
void fr_viewer_cursor_close(struct fr_viewer_cursor *h) {
    if (!h) return;
    if (h->c && !xcb_connection_has_error(h->c) && h->current) {
        /* Restore the platform's own pointer before releasing ours. */
        uint32_t none = XCB_CURSOR_NONE;
        checked(h, xcb_change_window_attributes_checked(h->c, h->window, XCB_CW_CURSOR, &none));
        xcb_free_cursor(h->c, h->current);
        xcb_flush(h->c);
    }
    if (h->c) xcb_disconnect(h->c);
    free(h);
}
struct fr_viewer_cursor *fr_viewer_cursor_open(const char *display, uint32_t window,
                                             uint32_t width, uint32_t height, int *in) {
    if (!display || !window || !in || !width || !height || width > 32767 || height > 32767)
        return NULL;
    struct fr_viewer_cursor *h = calloc(1, sizeof(*h));
    if (!h) return NULL;
    h->window = window; h->width = (uint16_t)width; h->height = (uint16_t)height;
    h->c = xcb_connect(display, NULL);
    if (!h->c || xcb_connection_has_error(h->c)) goto fail;
    h->byte_order = xcb_get_setup(h->c)->image_byte_order;
    xcb_get_geometry_reply_t *g = xcb_get_geometry_reply(h->c, xcb_get_geometry(h->c, window), NULL);
    int ok = g && g->root != window && g->width == width && g->height == height;
    free(g); if (!ok || !argb32(h)) goto fail;
    /* Crossing only, on OUR connection's mask for this window. */
    const uint32_t mask = XCB_EVENT_MASK_ENTER_WINDOW | XCB_EVENT_MASK_LEAVE_WINDOW;
    if (!checked(h, xcb_change_window_attributes_checked(h->c, window, XCB_CW_EVENT_MASK, &mask)))
        goto fail;
    xcb_query_pointer_reply_t *p = xcb_query_pointer_reply(h->c, xcb_query_pointer(h->c, window), NULL);
    if (!p) goto fail;
    *in = inside(p, h->width, h->height);
    free(p);
    return h;
fail:
    fr_viewer_cursor_close(h); return NULL;
}
/* -1: connection failure. 0: nothing pending. 1: one event consumed. */
int fr_viewer_cursor_poll(struct fr_viewer_cursor *h, int *in) {
    if (!h || !in || xcb_connection_has_error(h->c)) return -1;
    xcb_generic_event_t *e = xcb_poll_for_event(h->c);
    if (!e) return xcb_connection_has_error(h->c) ? -1 : 0;
    uint8_t kind = e->response_type & 0x7f;
    if (kind == XCB_ENTER_NOTIFY || kind == XCB_LEAVE_NOTIFY) {
        xcb_enter_notify_event_t *v = (void *)e; /* same layout for Leave */
        if (v->event == h->window && v->detail != XCB_NOTIFY_DETAIL_INFERIOR)
            *in = kind == XCB_ENTER_NOTIFY;
    }
    free(e); return 1;
}
static void put32(uint8_t *out, uint32_t v, uint8_t order) {
    if (order == XCB_IMAGE_ORDER_LSB_FIRST) {
        out[0] = (uint8_t)v; out[1] = (uint8_t)(v >> 8);
        out[2] = (uint8_t)(v >> 16); out[3] = (uint8_t)(v >> 24);
    } else {
        out[0] = (uint8_t)(v >> 24); out[1] = (uint8_t)(v >> 16);
        out[2] = (uint8_t)(v >> 8); out[3] = (uint8_t)v;
    }
}
static xcb_cursor_t render(struct fr_viewer_cursor *h, uint16_t w, uint16_t hh,
                           uint16_t hx, uint16_t hy, const uint32_t *argb) {
    xcb_pixmap_t pixmap = xcb_generate_id(h->c);
    if (!checked(h, xcb_create_pixmap_checked(h->c, 32, pixmap, h->window, w, hh))) return 0;
    xcb_gcontext_t gc = xcb_generate_id(h->c);
    xcb_render_picture_t picture = 0;
    xcb_cursor_t cursor = 0;
    uint8_t *band = NULL;
    if (!checked(h, xcb_create_gc_checked(h->c, gc, pixmap, 0, NULL))) { gc = 0; goto done; }
    /* Bounded bands: never rely on BIG-REQUESTS for a 256x256 image. */
    uint32_t stride = (uint32_t)w * 4u;
    uint32_t rows = FR_CURSOR_BAND_BYTES / stride;
    if (!rows) goto done;
    band = malloc((size_t)stride * rows);
    if (!band) goto done;
    for (uint32_t y = 0; y < hh; y += rows) {
        uint32_t n = hh - y < rows ? hh - y : rows;
        for (uint32_t i = 0; i < n * w; i++) put32(band + (size_t)i * 4, argb[(size_t)y * w + i], h->byte_order);
        if (!checked(h, xcb_put_image_checked(h->c, XCB_IMAGE_FORMAT_Z_PIXMAP, pixmap, gc,
                                              w, (uint16_t)n, 0, (int16_t)y, 0, 32,
                                              stride * n, band))) goto done;
    }
    picture = xcb_generate_id(h->c);
    if (!checked(h, xcb_render_create_picture_checked(h->c, picture, pixmap, h->argb, 0, NULL))) {
        picture = 0; goto done;
    }
    cursor = xcb_generate_id(h->c);
    if (!checked(h, xcb_render_create_cursor_checked(h->c, cursor, picture, hx, hy))) cursor = 0;
done:
    free(band);
    if (picture) xcb_render_free_picture(h->c, picture);
    if (gc) xcb_free_gc(h->c, gc);
    xcb_free_pixmap(h->c, pixmap);
    return cursor;
}
/* kind 0: the platform's own pointer; 1: fully transparent; 2: this image.
 * `argb` is premultiplied 0xAARRGGBB, exactly width*height, already bounded
 * by the Rust owner. Returns 1 only after the server applied the change. */
int fr_viewer_cursor_apply(struct fr_viewer_cursor *h, uint32_t kind, uint32_t width,
                           uint32_t height, uint32_t hot_x, uint32_t hot_y,
                           const uint32_t *argb) {
    static const uint32_t transparent = 0;
    if (!h || xcb_connection_has_error(h->c) || kind > 2) return 0;
    xcb_cursor_t cursor = XCB_CURSOR_NONE;
    if (kind == 1) {
        cursor = render(h, 1, 1, 0, 0, &transparent);
        if (!cursor) return 0;
    } else if (kind == 2) {
        if (!argb || !width || !height || width > FR_CURSOR_MAX_SIDE || height > FR_CURSOR_MAX_SIDE ||
            hot_x >= width || hot_y >= height) return 0;
        cursor = render(h, (uint16_t)width, (uint16_t)height, (uint16_t)hot_x, (uint16_t)hot_y, argb);
        if (!cursor) return 0;
    }
    if (!checked(h, xcb_change_window_attributes_checked(h->c, h->window, XCB_CW_CURSOR, &cursor))) {
        if (cursor) xcb_free_cursor(h->c, cursor);
        xcb_flush(h->c);
        return 0;
    }
    /* The window keeps its own reference; ours to the old image is released. */
    if (h->current) xcb_free_cursor(h->c, h->current);
    h->current = cursor;
    return xcb_flush(h->c) > 0;
}
