/* XCB-only representation boundary for the revocation-only local indicator.
 * Owns its connection/window/font/GC, never authority or Rust pointers. No
 * callbacks, grabs, ambient DISPLAY, user strings, input injection or Xlib
 * global error handlers. See X11 protocol events and EWMH window properties.
 * The remote-CONTROL indicator (mode 3) additionally attributes every click
 * and key to its XInput2 source device and ignores XTEST slave devices: the
 * controlling peer injects through XTest and must not operate this surface. */
#include <xcb/xcb.h>
#include <xcb/xcbext.h>
#include <sys/uio.h>
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
    uint8_t mode, mapped, allow_pressed;
    uint8_t xi_opcode; /* nonzero only for the XI2-attributed control mode */
};
enum { MODE_SHARING = 0, MODE_CONTROL = 3 };
/* XI2 wire constants (XI2.h / XI2proto.h). Requests are sent raw through
 * xcbext so no libxcb-xinput dependency is added. */
enum { XI_QUERY_VERSION = 47, XI_SELECT_EVENTS = 46, XI_QUERY_DEVICE = 48 };
enum { XI_KEY_PRESS = 2, XI_BUTTON_PRESS = 4, XI_BUTTON_RELEASE = 5 };
enum { XI_ALL_MASTER_DEVICES = 1 };
static xcb_extension_t xinput = { "XInputExtension", 0 };
/* One returned event accounts for one consumed XCB event, including ignored
 * events. Rust imposes the turn limit and checks authority between events. */
enum { IGNORE, REDRAW, MAPPED, HIDDEN, LOST, STOP, ALLOW };
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
/* One raw XI2 request with a reply. Returns the malloc'd reply or NULL. */
static uint8_t *xi_request(struct fr_indicator *h, uint8_t opcode, void *body, size_t len) {
    struct iovec parts[3];
    parts[2].iov_base = body;
    parts[2].iov_len = len;
    xcb_protocol_request_t request = { 1, &xinput, opcode, 0 };
    unsigned int seq = xcb_send_request(h->c, XCB_REQUEST_CHECKED, parts + 2, &request);
    if (!seq) return NULL;
    xcb_generic_error_t *e = NULL;
    uint8_t *reply = xcb_wait_for_reply(h->c, seq, &e);
    if (e) { free(e); free(reply); return NULL; }
    return reply;
}
/* XInput 2.0 with button/key selection on this window for all master devices.
 * Core button/key events are NOT selected in this mode. */
static int xi_setup(struct fr_indicator *h) {
    const xcb_query_extension_reply_t *ext = xcb_get_extension_data(h->c, &xinput);
    if (!ext || !ext->present) return 0;
    struct { uint8_t major, minor; uint16_t length, want_major, want_minor; } version =
        { 0, 0, 0, 2, 0 };
    uint8_t *reply = xi_request(h, XI_QUERY_VERSION, &version, sizeof(version));
    uint16_t major = 0;
    if (reply) memcpy(&major, reply + 8, 2);
    free(reply);
    if (major < 2) return 0;
    struct {
        uint8_t major, minor; uint16_t length; uint32_t window;
        uint16_t num_masks, pad, deviceid, mask_len; uint32_t mask;
    } select = { 0, 0, 0, h->window, 1, 0, XI_ALL_MASTER_DEVICES, 1,
                 (1u << XI_KEY_PRESS) | (1u << XI_BUTTON_PRESS) | (1u << XI_BUTTON_RELEASE) };
    struct iovec parts[3];
    parts[2].iov_base = &select;
    parts[2].iov_len = sizeof(select);
    xcb_protocol_request_t request = { 1, &xinput, XI_SELECT_EVENTS, 1 };
    xcb_void_cookie_t cookie = { xcb_send_request(h->c, XCB_REQUEST_CHECKED, parts + 2, &request) };
    if (!cookie.sequence || !checked(h, cookie)) return 0;
    h->xi_opcode = ext->major_opcode;
    return 1;
}
/* 0 only for a known non-XTEST source device; XTEST slaves and anything that
 * cannot be attributed (-1) are treated as synthetic and ignored. */
static int xi_synthetic(struct fr_indicator *h, uint16_t source) {
    struct { uint8_t major, minor; uint16_t length, deviceid, pad; } query =
        { 0, 0, 0, source, 0 };
    uint8_t *reply = xi_request(h, XI_QUERY_DEVICE, &query, sizeof(query));
    if (!reply) return -1;
    uint32_t words; uint16_t count, id, name_len;
    memcpy(&words, reply + 4, 4);
    memcpy(&count, reply + 8, 2);
    size_t total = 32 + (size_t)words * 4;
    int result = -1;
    if (count >= 1 && total >= 44) {
        memcpy(&id, reply + 32, 2);
        memcpy(&name_len, reply + 40, 2);
        if (id == source && 44 + (size_t)name_len <= total) {
            result = 0;
            for (size_t i = 0; i + 5 <= name_len; ++i)
                if (memcmp(reply + 44 + i, "XTEST", 5) == 0) { result = 1; break; }
        }
    }
    free(reply);
    return result;
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
static struct fr_indicator *open_window(const char *display, uint32_t *window, uint8_t mode) {
    if (!display || !window) return NULL;
    struct fr_indicator *h = calloc(1, sizeof(*h));
    if (!h) return NULL;
    h->mode = mode;
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
        XCB_EVENT_MASK_VISIBILITY_CHANGE };
    if (mode != MODE_CONTROL)
        values[1] |= XCB_EVENT_MASK_BUTTON_PRESS | XCB_EVENT_MASK_KEY_PRESS |
                     XCB_EVENT_MASK_BUTTON_RELEASE;
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
    const char *title = mode == MODE_CONTROL ? "FrankenRemote - Stop remote control"
        : mode ? "FrankenRemote - Local approval" : "FrankenRemote - Stop sharing";
    const char class[] = "franken-remote\0FrankenRemote\0";
    uint32_t size[18] = {0};
    size[0] = (1u << 4) | (1u << 5); /* ICCCM PMinSize | PMaxSize */
    size[5] = size[7] = WIDTH; size[6] = size[8] = HEIGHT;
    if (!h->close || !utf8 || !above ||
        !property(h, h->protocols, XCB_ATOM_ATOM, 32, 1, &h->close) ||
        !property(h, XCB_ATOM_WM_NAME, XCB_ATOM_STRING, 8, (uint32_t)strlen(title), title) ||
        !property(h, atom(h, "_NET_WM_NAME"), utf8, 8, (uint32_t)strlen(title), title) ||
        !property(h, XCB_ATOM_WM_CLASS, XCB_ATOM_STRING, 8, sizeof(class)-1, class) ||
        !property(h, XCB_ATOM_WM_NORMAL_HINTS, XCB_ATOM_WM_SIZE_HINTS, 32, 18, size) ||
        !property(h, atom(h, "_NET_WM_STATE"), XCB_ATOM_ATOM, 32, 1, &above) ||
        !keys(h) || (mode == MODE_CONTROL && !xi_setup(h)) ||
        !checked(h, xcb_map_window_checked(h->c, h->window))) goto fail;
    *window = h->window; return h;
fail:
    fr_indicator_close(h); return NULL;
}
struct fr_indicator *fr_indicator_open(const char *display, uint32_t *window) {
    return open_window(display, window, MODE_SHARING);
}
/* Remote-control indicator for the input executor: fails (NULL) without XI2. */
struct fr_indicator *fr_indicator_open_control(const char *display, uint32_t *window) {
    return open_window(display, window, MODE_CONTROL);
}
/* Role comes from Rust's original one-use Approval, never a remote UI string. */
struct fr_indicator *fr_approval_open(const char *display, uint32_t role, uint32_t *window) {
    if (role > 1) return NULL;
    return open_window(display, window, (uint8_t)(role + 1));
}
int fr_indicator_draw(struct fr_indicator *h) {
    if (!h || xcb_connection_has_error(h->c)) return 0;
    xcb_clear_area(h->c, 0, h->window, 0, 0, WIDTH, HEIGHT);
    if (h->mode && h->mode != MODE_CONTROL) {
        const char *lines[] = {
            h->mode == 1 ? "Verified peer requests desktop viewing" : "Verified peer requests desktop CONTROL",
            "Only this request; no approval of future peers.",
            "DENY", h->mode == 1 ? "ALLOW VIEWING" : "ALLOW CONTROL",
            "Esc / Enter: deny. Click Allow explicitly."};
        const int16_t x[] = {16, 16, 100, 320, 16}, y[] = {27, 53, 103, 103, 138};
        xcb_rectangle_t boxes[] = {{16, 73, 216, 46}, {248, 73, 216, 46}};
        xcb_poly_rectangle(h->c, h->window, h->gc, 2, boxes);
        for (unsigned i = 0; i < 5; ++i)
            xcb_image_text_8(h->c, (uint8_t)strlen(lines[i]), h->window, h->gc,
                             x[i], y[i], lines[i]);
        return xcb_flush(h->c) > 0;
    }
    const char *sharing[] = {"FrankenRemote sharing is authorized",
        "Closing or hiding this window stops sharing.",
        "STOP SHARING", "Esc / Enter / Space: stop sharing"};
    const char *control[] = {"A remote peer is CONTROLLING this desktop",
        "Closing or hiding this window stops control.",
        "STOP CONTROL", "Esc / Enter / Space: stop control"};
    const char **lines = h->mode == MODE_CONTROL ? control : sharing;
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
    if (h->mode == MODE_CONTROL) {
        /* Only XI2 events from a real (non-XTEST) source may stop control.
         * Core button/key events (e.g. SendEvent to the creator) are ignored.
         * WM close (ClientMessage) and hiding still stop: removal only. */
        if (type == XCB_BUTTON_PRESS || type == XCB_BUTTON_RELEASE || type == XCB_KEY_PRESS) {
            free(e); return 1;
        }
        if (type == XCB_GE_GENERIC) {
            const uint8_t *b = (const uint8_t *)e;
            uint32_t words;
            memcpy(&words, b + 4, 4);
            /* Device events carry >= 60 wire bytes; XCB stores wire byte 32+
             * after its 4-byte full_sequence, i.e. at buffer offset 36+. */
            if (b[1] == h->xi_opcode && (size_t)words * 4 + 32 >= 60) {
                uint16_t evtype, source;
                uint32_t detail, event;
                int32_t fx, fy;
                memcpy(&evtype, b + 8, 2);
                memcpy(&detail, b + 16, 4);
                memcpy(&event, b + 24, 4);
                memcpy(&fx, b + 44, 4);
                memcpy(&fy, b + 48, 4);
                memcpy(&source, b + 56, 2);
                int32_t x = fx / 65536, y = fy / 65536;
                int press = evtype == XI_BUTTON_PRESS && detail == 1 &&
                    x >= 16 && x < 464 && y >= 73 && y < 119;
                int key = evtype == XI_KEY_PRESS && (detail == h->escape ||
                    detail == h->enter || detail == h->space);
                if (event == h->window && (press || key) && xi_synthetic(h, source) == 0)
                    *kind = STOP;
            }
            free(e); return 1;
        }
    }
    switch (type) {
    case XCB_EXPOSE:
        if (((xcb_expose_event_t *)e)->window == h->window) *kind = REDRAW;
        break;
    case XCB_MAP_NOTIFY:
        if (!synthetic && ((xcb_map_notify_event_t *)e)->window == h->window) {
            h->mapped = 1; *kind = MAPPED;
        }
        break;
    case XCB_UNMAP_NOTIFY:
        if (((xcb_unmap_notify_event_t *)e)->window == h->window) {
            h->mapped = 0; h->allow_pressed = 0; *kind = HIDDEN;
        }
        break;
    case XCB_DESTROY_NOTIFY:
        if (((xcb_destroy_notify_event_t *)e)->window == h->window) *kind = LOST;
        break;
    case XCB_VISIBILITY_NOTIFY: {
        xcb_visibility_notify_event_t *v = (xcb_visibility_notify_event_t *)e;
        if (v->window == h->window && v->state != XCB_VISIBILITY_UNOBSCURED) {
            h->allow_pressed = 0; *kind = HIDDEN;
        }
        break;
    }
    case XCB_CONFIGURE_NOTIFY: {
        xcb_configure_notify_event_t *v = (xcb_configure_notify_event_t *)e;
        if (v->window == h->window && (v->width != WIDTH || v->height != HEIGHT)) *kind = HIDDEN;
        break;
    }
    case XCB_BUTTON_PRESS: {
        h->allow_pressed = 0;
        xcb_button_press_event_t *v = (xcb_button_press_event_t *)e;
        if (v->event == h->window && v->detail == 1 && v->same_screen &&
            v->event_x >= 16 && v->event_x < 464 &&
            v->event_y >= 73 && v->event_y < 119) {
            if (!h->mode || v->event_x < 232) *kind = STOP;
            else if (!synthetic && h->mapped && v->event_x >= 248)
                h->allow_pressed = 1;
        } else h->allow_pressed = 0;
        break;
    }
    case XCB_BUTTON_RELEASE: {
        xcb_button_release_event_t *v = (xcb_button_release_event_t *)e;
        /* A complete non-SendEvent primary click within Allow, after mapping.
         * XTest is intentionally indistinguishable from the selected user's
         * input: the local X server and same-user processes are trust boundaries. */
        if (h->mode && h->allow_pressed && h->mapped && !synthetic &&
            v->event == h->window && v->detail == 1 && v->same_screen &&
            v->event_x >= 248 && v->event_x < 464 &&
            v->event_y >= 73 && v->event_y < 119) *kind = ALLOW;
        h->allow_pressed = 0;
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
