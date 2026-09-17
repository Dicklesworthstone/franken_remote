/* Client-owned XCB drawable. The decoder borrows only the chosen X resource;
 * it never selects the input target or controls this connection's lifetime.
 * No callbacks, Xlib globals, grabs, native input injection or peer strings. */
#include <xcb/xcb.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

struct fr_viewer_window {
    xcb_connection_t *c;
    xcb_window_t window;
    xcb_atom_t protocols, close;
    uint16_t width, height;
};
enum { IGNORE, MAPPED, HIDDEN, LOST, RESIZED, STOP };
static int checked(struct fr_viewer_window *h, xcb_void_cookie_t cookie) {
    xcb_generic_error_t *e = xcb_request_check(h->c, cookie);
    int ok = !e && !xcb_connection_has_error(h->c);
    free(e); return ok;
}
static xcb_atom_t atom(struct fr_viewer_window *h, const char *name) {
    xcb_generic_error_t *e = NULL;
    xcb_intern_atom_reply_t *r = xcb_intern_atom_reply(h->c,
        xcb_intern_atom(h->c, 0, (uint16_t)strlen(name), name), &e);
    xcb_atom_t result = r && !e ? r->atom : XCB_NONE;
    free(r); free(e); return result;
}
static int property(struct fr_viewer_window *h, xcb_atom_t name, xcb_atom_t type,
                    uint8_t format, uint32_t count, const void *data) {
    return name && checked(h, xcb_change_property_checked(h->c,
        XCB_PROP_MODE_REPLACE, h->window, name, type, format, count, data));
}
void fr_viewer_window_close(struct fr_viewer_window *h) {
    if (!h) return;
    /* Destroy only our resources. The Rust owner fences the original session
     * first. Disconnect cannot collect or prove another worker's cleanup. */
    if (h->c) xcb_disconnect(h->c);
    free(h);
}
struct fr_viewer_window *fr_viewer_window_open(const char *display,
        uint32_t width, uint32_t height, uint32_t *window) {
    if (!display || !window || width < 16 || height < 16 ||
        width > UINT16_MAX || height > UINT16_MAX || (width & 1) || (height & 1))
        return NULL;
    *window = XCB_NONE;
    struct fr_viewer_window *h = calloc(1, sizeof(*h));
    if (!h) return NULL;
    h->width = (uint16_t)width; h->height = (uint16_t)height;
    int screen = 0;
    h->c = xcb_connect(display, &screen);
    if (!h->c || xcb_connection_has_error(h->c)) goto fail;
    xcb_screen_iterator_t it = xcb_setup_roots_iterator(xcb_get_setup(h->c));
    while (screen-- > 0 && it.rem) xcb_screen_next(&it);
    if (!it.rem || it.data->root_depth != 24 ||
        it.data->width_in_pixels < width || it.data->height_in_pixels < height) goto fail;
    h->window = xcb_generate_id(h->c);
    uint32_t values[] = { it.data->black_pixel, XCB_EVENT_MASK_STRUCTURE_NOTIFY };
    if (!checked(h, xcb_create_window_checked(h->c, XCB_COPY_FROM_PARENT,
        h->window, it.data->root, 0, 0, h->width, h->height, 0,
        XCB_WINDOW_CLASS_INPUT_OUTPUT, it.data->root_visual,
        XCB_CW_BACK_PIXEL | XCB_CW_EVENT_MASK, values))) goto fail;
    h->protocols = atom(h, "WM_PROTOCOLS");
    h->close = atom(h, "WM_DELETE_WINDOW");
    xcb_atom_t utf8 = atom(h, "UTF8_STRING");
    const char title[] = "FrankenRemote";
    const char class[] = "franken-remote\0FrankenRemote\0";
    /* The first qualified layout is exactly native pixels. A resize is terminal,
     * not an invitation to scale pixels independently of input coordinates. */
    uint32_t size[18] = {0};
    size[0] = (1u << 4) | (1u << 5); /* ICCCM PMinSize | PMaxSize */
    size[5] = size[7] = width; size[6] = size[8] = height;
    if (!h->close || !utf8 ||
        !property(h, h->protocols, XCB_ATOM_ATOM, 32, 1, &h->close) ||
        !property(h, XCB_ATOM_WM_NAME, XCB_ATOM_STRING, 8, sizeof(title)-1, title) ||
        !property(h, atom(h, "_NET_WM_NAME"), utf8, 8, sizeof(title)-1, title) ||
        !property(h, XCB_ATOM_WM_CLASS, XCB_ATOM_STRING, 8, sizeof(class)-1, class) ||
        !property(h, XCB_ATOM_WM_NORMAL_HINTS, XCB_ATOM_WM_SIZE_HINTS, 32, 18, size) ||
        !checked(h, xcb_map_window_checked(h->c, h->window))) goto fail;
    *window = h->window; return h;
fail:
    fr_viewer_window_close(h); return NULL;
}
int fr_viewer_window_next(struct fr_viewer_window *h, uint32_t *kind) {
    if (!h || !kind || xcb_connection_has_error(h->c)) return -1;
    xcb_generic_event_t *e = xcb_poll_for_event(h->c);
    if (!e) return xcb_connection_has_error(h->c) ? -1 : 0;
    *kind = IGNORE;
    uint8_t type = e->response_type & 0x7f;
    int synthetic = (e->response_type & 0x80) != 0;
    if (!type) { free(e); return -1; }
    switch (type) {
    case XCB_MAP_NOTIFY:
        if (!synthetic && ((xcb_map_notify_event_t *)e)->window == h->window)
            *kind = MAPPED;
        break;
    case XCB_UNMAP_NOTIFY:
        if (((xcb_unmap_notify_event_t *)e)->window == h->window) *kind = HIDDEN;
        break;
    case XCB_DESTROY_NOTIFY:
        if (((xcb_destroy_notify_event_t *)e)->window == h->window) *kind = LOST;
        break;
    case XCB_CONFIGURE_NOTIFY: {
        xcb_configure_notify_event_t *v = (xcb_configure_notify_event_t *)e;
        if (v->window == h->window && (v->width != h->width || v->height != h->height))
            *kind = RESIZED;
        break;
    }
    case XCB_CLIENT_MESSAGE: {
        xcb_client_message_event_t *v = (xcb_client_message_event_t *)e;
        if (v->window == h->window && v->format == 32 &&
            v->type == h->protocols && v->data.data32[0] == h->close) *kind = STOP;
        break;
    }
    default: break;
    }
    /* Synthetic requests can only close/refuse. They cannot prove mapping or
     * visibility, raise the window, grant input, or select a replacement target.
     * Each consumed event is returned, even ignored ones: Rust bounds work. */
    free(e); return 1;
}

/* A finite display-choice surface on the SAME XCB shell boundary. Catalog
 * identity and session lifetime stay in Rust. C sees only bounded geometry and
 * returns a row ordinal, never a display handle or an authorization decision. */
#include <stdio.h>
#include <inttypes.h>
enum { PICK_WIDTH = 560, PICK_TOP = 64, PICK_ROW = 36, PICK_MAX = 8 };
struct fr_picker_row { int32_t x, y; uint32_t width, height; };
struct fr_picker {
    struct fr_viewer_window *base;
    xcb_font_t font;
    xcb_gcontext_t gc;
    struct fr_picker_row rows[PICK_MAX];
    uint32_t count;
    int highlighted, pressed;
    uint8_t escape, enter, up, down, digits[PICK_MAX], armed_key;
};
static int picker_keys(struct fr_picker *p) {
    xcb_connection_t *c = p->base->c;
    const xcb_setup_t *s = xcb_get_setup(c);
    unsigned count = (unsigned)s->max_keycode - s->min_keycode + 1;
    if (!count || count > 248) return 0;
    xcb_generic_error_t *e = NULL;
    xcb_get_keyboard_mapping_reply_t *r = xcb_get_keyboard_mapping_reply(c,
        xcb_get_keyboard_mapping(c, s->min_keycode, (uint8_t)count), &e);
    if (!r || e || !r->keysyms_per_keycode ||
        xcb_get_keyboard_mapping_keysyms_length(r) != (int)(count * r->keysyms_per_keycode)) {
        free(r); free(e); return 0;
    }
    p->escape = p->enter = p->up = p->down = p->armed_key = 0;
    memset(p->digits, 0, sizeof(p->digits)); p->pressed = -1;
    xcb_keysym_t *symbols = xcb_get_keyboard_mapping_keysyms(r);
    for (unsigned k = 0; k < count; ++k) {
        uint32_t symbol = symbols[k * r->keysyms_per_keycode];
        uint8_t code = (uint8_t)(s->min_keycode + k);
        if (symbol == 0xff1b) p->escape = code;
        if (symbol == 0xff0d) p->enter = code;
        if (symbol == 0xff52) p->up = code;
        if (symbol == 0xff54) p->down = code;
        if (symbol >= 0x31 && symbol < 0x31 + PICK_MAX) p->digits[symbol - 0x31] = code;
    }
    free(r); free(e); return p->escape && p->enter && p->up && p->down;
}
static int picker_text(struct fr_picker *p, int16_t x, int16_t y, const char *text) {
    size_t n = strlen(text);
    return n < 256 && checked(p->base, xcb_image_text_8_checked(p->base->c,
        (uint8_t)n, p->base->window, p->gc, x, y, text));
}
static int picker_draw(struct fr_picker *p) {
    struct fr_viewer_window *h = p->base;
    if (!checked(h, xcb_clear_area_checked(h->c, 0, h->window, 0, 0, 0, 0)) ||
        !picker_text(p, 16, 24, "FrankenRemote - choose a remote display") ||
        !picker_text(p, 16, 44, "Approved session. This choice does not grant input control.")) return 0;
    for (uint32_t i = 0; i < p->count; ++i) {
        char line[96];
        int n = snprintf(line, sizeof(line), "%c %u.  %" PRIu32 " x %" PRIu32 " pixels",
            p->highlighted == (int)i ? '>' : ' ', i + 1, p->rows[i].width, p->rows[i].height);
        if (n < 0 || (size_t)n >= sizeof(line) ||
            !picker_text(p, 16, (int16_t)(PICK_TOP + i * PICK_ROW + 12), line)) return 0;
        n = snprintf(line, sizeof(line), "       Desktop origin (%" PRId32 ", %" PRId32 ")",
            p->rows[i].x, p->rows[i].y);
        if (n < 0 || (size_t)n >= sizeof(line) ||
            !picker_text(p, 16, (int16_t)(PICK_TOP + i * PICK_ROW + 26), line)) return 0;
    }
    return picker_text(p, 16, (int16_t)(PICK_TOP + p->count * PICK_ROW + 20),
        "Click, or Up/Down then Enter; number keys select. Esc cancels.");
}
void fr_viewer_picker_close(struct fr_picker *p) {
    if (!p) return;
    fr_viewer_window_close(p->base);
    free(p);
}
struct fr_picker *fr_viewer_picker_open(const char *display,
        const struct fr_picker_row *rows, uint32_t count, uint32_t *window) {
    if (!rows || !window || !count || count > PICK_MAX) return NULL;
    struct fr_picker *p = calloc(1, sizeof(*p));
    if (!p) return NULL;
    p->count = count; p->highlighted = p->pressed = -1;
    memcpy(p->rows, rows, count * sizeof(*rows));
    p->base = fr_viewer_window_open(display, PICK_WIDTH, 96 + count * PICK_ROW, window);
    if (!p->base) goto fail;
    struct fr_viewer_window *h = p->base;
    uint32_t values[] = {0xffffff,
        XCB_EVENT_MASK_STRUCTURE_NOTIFY | XCB_EVENT_MASK_EXPOSURE |
        XCB_EVENT_MASK_BUTTON_PRESS | XCB_EVENT_MASK_BUTTON_RELEASE |
        XCB_EVENT_MASK_KEY_PRESS | XCB_EVENT_MASK_KEY_RELEASE | XCB_EVENT_MASK_FOCUS_CHANGE};
    if (!checked(h, xcb_change_window_attributes_checked(h->c, h->window,
        XCB_CW_BACK_PIXEL | XCB_CW_EVENT_MASK, values))) goto fail;
    p->font = xcb_generate_id(h->c);
    if (!checked(h, xcb_open_font_checked(h->c, p->font, 5, "fixed"))) goto fail;
    p->gc = xcb_generate_id(h->c);
    uint32_t gc[] = {0, 0xffffff, p->font};
    if (!checked(h, xcb_create_gc_checked(h->c, p->gc, h->window,
        XCB_GC_FOREGROUND | XCB_GC_BACKGROUND | XCB_GC_FONT, gc)) ||
        !picker_keys(p) || !picker_draw(p)) goto fail;
    return p;
fail:
    fr_viewer_picker_close(p); return NULL;
}
static int picker_row(struct fr_picker *p, int16_t x, int16_t y) {
    if (x < 12 || x >= PICK_WIDTH - 12 || y < PICK_TOP) return -1;
    int row = (y - PICK_TOP) / PICK_ROW;
    return row < (int)p->count ? row : -1;
}
/* kind: 0 ignored, 1 mapped, 2 cancelled/lost/changed, 3 selected row.
 * Every consumed event is reported, including ignored/synthetic events, so the
 * Rust caller bounds native work and drains a batch before accepting a choice. */
int fr_viewer_picker_next(struct fr_picker *p, uint32_t *kind, uint32_t *row) {
    if (!p || !kind || !row || xcb_connection_has_error(p->base->c)) return -1;
    struct fr_viewer_window *h = p->base;
    xcb_generic_event_t *e = xcb_poll_for_event(h->c);
    if (!e) return xcb_connection_has_error(h->c) ? -1 : 0;
    *kind = *row = 0;
    uint8_t type = e->response_type & 0x7f;
    int synthetic = (e->response_type & 0x80) != 0, redraw = 0;
    if (!type) { free(e); return -1; }
    switch (type) {
    case XCB_MAP_NOTIFY:
        if (!synthetic && ((xcb_map_notify_event_t *)e)->window == h->window) *kind = 1;
        break;
    case XCB_UNMAP_NOTIFY: case XCB_DESTROY_NOTIFY:
        if (((xcb_unmap_notify_event_t *)e)->window == h->window) *kind = 2;
        break;
    case XCB_CONFIGURE_NOTIFY: {
        xcb_configure_notify_event_t *v = (xcb_configure_notify_event_t *)e;
        if (v->window == h->window && (v->width != h->width || v->height != h->height)) *kind = 2;
        break;
    }
    case XCB_CLIENT_MESSAGE: {
        xcb_client_message_event_t *v = (xcb_client_message_event_t *)e;
        if (v->window == h->window && v->format == 32 && v->type == h->protocols &&
            v->data.data32[0] == h->close) *kind = 2;
        break;
    }
    case XCB_EXPOSE: redraw = !synthetic; break;
    case XCB_FOCUS_OUT: p->pressed = -1; p->armed_key = 0; break;
    case XCB_MAPPING_NOTIFY:
        p->highlighted = -1;
        if (!picker_keys(p)) { free(e); return -1; }
        redraw = 1; break;
    case XCB_BUTTON_PRESS: case XCB_BUTTON_RELEASE: {
        xcb_button_press_event_t *v = (xcb_button_press_event_t *)e;
        if (synthetic || v->event != h->window || v->detail != 1 || !v->same_screen) break;
        int r = picker_row(p, v->event_x, v->event_y);
        if (type == XCB_BUTTON_PRESS) {
            p->pressed = r; p->highlighted = r; redraw = 1;
        } else {
            if (r >= 0 && r == p->pressed) { *kind = 3; *row = (uint32_t)r; }
            p->pressed = -1;
        }
        break;
    }
    case XCB_KEY_PRESS: case XCB_KEY_RELEASE: {
        xcb_key_press_event_t *v = (xcb_key_press_event_t *)e;
        if (synthetic || v->event != h->window || !v->same_screen) break;
        if (type == XCB_KEY_RELEASE) {
            if (p->armed_key && v->detail == p->armed_key && p->highlighted >= 0) {
                *kind = 3; *row = (uint32_t)p->highlighted;
            }
            p->armed_key = 0; break;
        }
        if (v->detail == p->escape) { *kind = 2; break; }
        if (v->detail == p->up || v->detail == p->down) {
            p->armed_key = 0;
            if (p->highlighted < 0) p->highlighted = 0;
            else if (v->detail == p->down) p->highlighted = (p->highlighted + 1) % (int)p->count;
            else p->highlighted = (p->highlighted + (int)p->count - 1) % (int)p->count;
            redraw = 1;
        } else if (v->detail == p->enter && p->highlighted >= 0) p->armed_key = v->detail;
        else for (uint32_t i = 0; i < p->count; ++i) if (p->digits[i] == v->detail) {
            p->highlighted = (int)i; p->armed_key = v->detail; redraw = 1; break;
        }
        break;
    }
    default: break;
    }
    free(e);
    if (redraw && !picker_draw(p)) return -1;
    return 1;
}
