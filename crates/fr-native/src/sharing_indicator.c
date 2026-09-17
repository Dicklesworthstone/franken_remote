/* XCB-only representation boundary for the revocation-only local indicator.
 * Owns its connection/window/font/GC, never authority or Rust pointers. No
 * callbacks, grabs, ambient DISPLAY, user strings, input injection or Xlib
 * global error handlers. See X11 protocol events and EWMH window properties. */
#include <xcb/xcb.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

enum { WIDTH = 480, HEIGHT = 148 };
struct fr_indicator {
    xcb_connection_t *c;
    xcb_window_t window;
    xcb_gcontext_t gc;
    xcb_font_t font;
    xcb_atom_t protocols, close;
    uint8_t escape, enter, space;
};
/* One returned event accounts for one consumed XCB event, including ignored
 * events. Rust imposes the turn limit and checks authority between events. */
enum { IGNORE, REDRAW, MAPPED, HIDDEN, LOST, STOP };
static int checked(struct fr_indicator *h, xcb_void_cookie_t cookie) {
    xcb_generic_error_t *e = xcb_request_check(h->c, cookie);
    int ok = !e && !xcb_connection_has_error(h->c);
    free(e); return ok;
}
static xcb_atom_t atom(struct fr_indicator *h, const char *name) {
    xcb_generic_error_t *e = NULL;
    xcb_intern_atom_reply_t *r = xcb_intern_atom_reply(h->c,
        xcb_intern_atom(h->c, 0, (uint16_t)strlen(name), name), &e);
    xcb_atom_t result = r && !e ? r->atom : XCB_NONE;
    free(r); free(e); return result;
}
static int property(struct fr_indicator *h, xcb_atom_t name, xcb_atom_t type,
                    uint8_t format, uint32_t count, const void *data) {
    return name && checked(h, xcb_change_property_checked(h->c,
        XCB_PROP_MODE_REPLACE, h->window, name, type, format, count, data));
}
void fr_indicator_close(struct fr_indicator *h) {
    if (!h) return;
    /* Disconnect destroys only this connection's resources. No foreign window
     * operation or clipboard selection change is part of UI cleanup. */
    if (h->c) xcb_disconnect(h->c);
    free(h);
}
static int keys(struct fr_indicator *h) {
    const xcb_setup_t *s = xcb_get_setup(h->c);
    unsigned count = (unsigned)s->max_keycode - s->min_keycode + 1;
    if (!count || count > 248) return 0;
    xcb_generic_error_t *e = NULL;
    xcb_get_keyboard_mapping_reply_t *r = xcb_get_keyboard_mapping_reply(h->c,
        xcb_get_keyboard_mapping(h->c, s->min_keycode, (uint8_t)count), &e);
    if (!r || e || !r->keysyms_per_keycode ||
        xcb_get_keyboard_mapping_keysyms_length(r) != (int)(count * r->keysyms_per_keycode)) {
        free(r); free(e); return 0;
    }
    h->escape = h->enter = h->space = 0;
    xcb_keysym_t *symbols = xcb_get_keyboard_mapping_keysyms(r);
    for (unsigned k = 0; k < count; ++k) {
        /* Local UI accelerators use the unshifted symbol, not a remote physical
         * key identity. MappingNotify refreshes this revocation-only key map. */
        uint32_t symbol = symbols[k * r->keysyms_per_keycode];
        uint8_t code = (uint8_t)(s->min_keycode + k);
        if (symbol == 0xff1b) h->escape = code;
        if (symbol == 0xff0d) h->enter = code;
        if (symbol == 0x20) h->space = code;
    }
    free(r); free(e);
    return h->escape && h->enter && h->space;
}
struct fr_indicator *fr_indicator_open(const char *display, uint32_t *window) {
    if (!display || !window) return NULL;
    struct fr_indicator *h = calloc(1, sizeof(*h));
    if (!h) return NULL;
    int screen = 0;
    h->c = xcb_connect(display, &screen);
    if (!h->c || xcb_connection_has_error(h->c)) goto fail;
    xcb_screen_iterator_t it = xcb_setup_roots_iterator(xcb_get_setup(h->c));
    while (screen-- > 0 && it.rem) xcb_screen_next(&it);
    if (!it.rem || it.data->width_in_pixels < WIDTH + 4 ||
        it.data->height_in_pixels < HEIGHT + 4) goto fail;
    h->window = xcb_generate_id(h->c);
    uint32_t values[] = { it.data->white_pixel,
        XCB_EVENT_MASK_EXPOSURE | XCB_EVENT_MASK_STRUCTURE_NOTIFY |
        XCB_EVENT_MASK_VISIBILITY_CHANGE | XCB_EVENT_MASK_BUTTON_PRESS |
        XCB_EVENT_MASK_KEY_PRESS };
    if (!checked(h, xcb_create_window_checked(h->c, XCB_COPY_FROM_PARENT,
        h->window, it.data->root, 0, 0, WIDTH, HEIGHT, 2,
        XCB_WINDOW_CLASS_INPUT_OUTPUT, it.data->root_visual,
        XCB_CW_BACK_PIXEL | XCB_CW_EVENT_MASK, values))) goto fail;
    h->font = xcb_generate_id(h->c);
    if (!checked(h, xcb_open_font_checked(h->c, h->font, 4, "6x13"))) goto fail;
    h->gc = xcb_generate_id(h->c);
    uint32_t gc[] = { it.data->black_pixel, it.data->white_pixel, h->font, 0 };
    if (!checked(h, xcb_create_gc_checked(h->c, h->gc, h->window,
        XCB_GC_FOREGROUND | XCB_GC_BACKGROUND | XCB_GC_FONT |
        XCB_GC_GRAPHICS_EXPOSURES, gc))) goto fail;
    h->protocols = atom(h, "WM_PROTOCOLS");
    h->close = atom(h, "WM_DELETE_WINDOW");
    xcb_atom_t utf8 = atom(h, "UTF8_STRING");
    xcb_atom_t above = atom(h, "_NET_WM_STATE_ABOVE");
    const char title[] = "FrankenRemote - Stop sharing";
    const char class[] = "franken-remote\0FrankenRemote\0";
    uint32_t size[18] = {0};
    size[0] = (1u << 4) | (1u << 5); /* ICCCM PMinSize | PMaxSize */
    size[5] = size[7] = WIDTH; size[6] = size[8] = HEIGHT;
    if (!h->close || !utf8 || !above ||
        !property(h, h->protocols, XCB_ATOM_ATOM, 32, 1, &h->close) ||
        !property(h, XCB_ATOM_WM_NAME, XCB_ATOM_STRING, 8, sizeof(title)-1, title) ||
        !property(h, atom(h, "_NET_WM_NAME"), utf8, 8, sizeof(title)-1, title) ||
        !property(h, XCB_ATOM_WM_CLASS, XCB_ATOM_STRING, 8, sizeof(class)-1, class) ||
        !property(h, XCB_ATOM_WM_NORMAL_HINTS, XCB_ATOM_WM_SIZE_HINTS, 32, 18, size) ||
        !property(h, atom(h, "_NET_WM_STATE"), XCB_ATOM_ATOM, 32, 1, &above) ||
        !keys(h) || !checked(h, xcb_map_window_checked(h->c, h->window))) goto fail;
    *window = h->window; return h;
fail:
    fr_indicator_close(h); return NULL;
}
int fr_indicator_draw(struct fr_indicator *h) {
    if (!h || xcb_connection_has_error(h->c)) return 0;
    xcb_clear_area(h->c, 0, h->window, 0, 0, WIDTH, HEIGHT);
    const char *lines[] = {"FrankenRemote sharing is authorized",
        "Closing or hiding this window stops sharing.",
        "STOP SHARING", "Esc / Enter / Space: stop sharing"};
    const int16_t x[] = {16, 16, 204, 16}, y[] = {27, 53, 103, 138};
    xcb_rectangle_t border = {16, 73, 448, 46};
    xcb_poly_rectangle(h->c, h->window, h->gc, 1, &border);
    for (unsigned i = 0; i < 4; ++i)
        xcb_image_text_8(h->c, (uint8_t)strlen(lines[i]), h->window, h->gc,
                         x[i], y[i], lines[i]);
    return xcb_flush(h->c) > 0;
}
int fr_indicator_next(struct fr_indicator *h, uint32_t *kind) {
    if (!h || !kind || xcb_connection_has_error(h->c)) return -1;
    xcb_generic_event_t *e = xcb_poll_for_event(h->c);
    if (!e) return xcb_connection_has_error(h->c) ? -1 : 0;
    *kind = IGNORE;
    uint8_t type = e->response_type & 0x7f;
    int synthetic = (e->response_type & 0x80) != 0;
    if (!type) { free(e); return -1; }
    switch (type) {
    case XCB_EXPOSE:
        if (((xcb_expose_event_t *)e)->window == h->window) *kind = REDRAW;
        break;
    case XCB_MAP_NOTIFY:
        if (!synthetic && ((xcb_map_notify_event_t *)e)->window == h->window) *kind = MAPPED;
        break;
    case XCB_UNMAP_NOTIFY:
        if (((xcb_unmap_notify_event_t *)e)->window == h->window) *kind = HIDDEN;
        break;
    case XCB_DESTROY_NOTIFY:
        if (((xcb_destroy_notify_event_t *)e)->window == h->window) *kind = LOST;
        break;
    case XCB_VISIBILITY_NOTIFY: {
        xcb_visibility_notify_event_t *v = (xcb_visibility_notify_event_t *)e;
        if (v->window == h->window && v->state != XCB_VISIBILITY_UNOBSCURED) *kind = HIDDEN;
        break;
    }
    case XCB_CONFIGURE_NOTIFY: {
        xcb_configure_notify_event_t *v = (xcb_configure_notify_event_t *)e;
        if (v->window == h->window && (v->width != WIDTH || v->height != HEIGHT)) *kind = HIDDEN;
        break;
    }
    case XCB_BUTTON_PRESS: {
        xcb_button_press_event_t *v = (xcb_button_press_event_t *)e;
        if (v->event == h->window && v->detail == 1 && v->same_screen &&
            v->event_x >= 16 && v->event_x < 464 &&
            v->event_y >= 73 && v->event_y < 119) *kind = STOP;
        break;
    }
    case XCB_KEY_PRESS: {
        xcb_key_press_event_t *v = (xcb_key_press_event_t *)e;
        if (v->event == h->window && (v->detail == h->escape ||
            v->detail == h->enter || v->detail == h->space)) *kind = STOP;
        break;
    }
    case XCB_CLIENT_MESSAGE: {
        xcb_client_message_event_t *v = (xcb_client_message_event_t *)e;
        if (v->window == h->window && v->format == 32 &&
            v->type == h->protocols && v->data.data32[0] == h->close) *kind = STOP;
        break;
    }
    case XCB_MAPPING_NOTIFY:
        if (((xcb_mapping_notify_event_t *)e)->request != XCB_MAPPING_POINTER) {
            if (!keys(h)) { free(e); return -1; }
        }
        break;
    default: break;
    }
    /* Synthetic stop requests may only REMOVE authority. They never establish
     * native map evidence, approve a session, enable clipboard or grant input. */
    free(e); return 1;
}
