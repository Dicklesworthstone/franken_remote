/* Client-owned XCB drawable. The decoder borrows only the chosen X resource;
 * it never selects the input target or controls this connection's lifetime.
 * No callbacks, Xlib globals, grabs, native input injection or peer strings. */
#include <xcb/xcb.h>
#include <xcb/xcbext.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

/* Shared with the separately owned clipboard surface below. Reserve space
 * before the immutable decoder/input drawable exists, never by resizing it. */
enum { FR_CLIPBOARD_WIDTH = 640, FR_CLIPBOARD_HEIGHT = 96,
       FR_CLIPBOARD_GAP = 24, FR_CLIPBOARD_MARGIN = 24 };

struct fr_viewer_drop;
struct fr_viewer_window {
    struct fr_viewer_drop *files;
    xcb_connection_t *c;
    xcb_window_t window;
    xcb_atom_t protocols, close, net_name, utf8;
    uint16_t width, height;
    uint8_t control_key, control_pressed, control_blocked, control_ready, control_barrier_kind;
    uint16_t control_locks;
    xcb_atom_t control_clock;
    uint32_t control_sequence, control_baseline, control_press_at, control_release_at, control_age;
    uint32_t focus_query;
    uint8_t focus_changed;
};
enum { IGNORE, MAPPED, HIDDEN, LOST, RESIZED, STOP, CONTROL_CHOSEN,
       CONTROL_RESET, CONTROL_REMAP, CONTROL_CLOCK, FOCUS_LEFT = 13 };
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
#include "viewer_drop.c"
void fr_viewer_window_close(struct fr_viewer_window *h) {
    if (!h) return;
    /* Destroy only our resources. The Rust owner fences the original session
     * first. Disconnect cannot collect or prove another worker's cleanup. */
    if (h->c) {
        fr_viewer_window_drop_cancel(h);
        xcb_disconnect(h->c);
    }
    free(h->files);
    free(h);
}
static struct fr_viewer_window *viewer_window_open(const char *display,
        uint32_t width, uint32_t height, int clipboard_space,
        uint32_t *window, uint32_t *actual_width, uint32_t *actual_height) {
    if (!display || !window || !actual_width || !actual_height || width < 16 || height < 16 ||
        width > UINT16_MAX || height > UINT16_MAX || (width & 1) || (height & 1))
        return NULL;
    *window = XCB_NONE; *actual_width = *actual_height = 0;
    struct fr_viewer_window *h = calloc(1, sizeof(*h));
    if (!h) return NULL;
    int screen = 0;
    h->c = xcb_connect(display, &screen);
    if (!h->c || xcb_connection_has_error(h->c)) goto fail;
    xcb_screen_iterator_t it = xcb_setup_roots_iterator(xcb_get_setup(h->c));
    while (screen-- > 0 && it.rem) xcb_screen_next(&it);
    if (!it.rem || it.data->root_depth != 24) goto fail;
    int16_t origin = 0;
    if (clipboard_space && it.data->width_in_pixels >= FR_CLIPBOARD_WIDTH &&
        it.data->height_in_pixels >= FR_CLIPBOARD_HEIGHT + FR_CLIPBOARD_GAP + 2 * FR_CLIPBOARD_MARGIN + 16) {
        uint32_t max_width = ((uint32_t)it.data->width_in_pixels - 2 * FR_CLIPBOARD_MARGIN) & ~1u;
        uint32_t max_height = ((uint32_t)it.data->height_in_pixels -
            FR_CLIPBOARD_HEIGHT - FR_CLIPBOARD_GAP - 2 * FR_CLIPBOARD_MARGIN) & ~1u;
        origin = FR_CLIPBOARD_MARGIN;
        if (width > max_width) width = max_width;
        if (height > max_height) height = max_height;
    }
    /* An unsupported local screen keeps the ordinary viewing policy. The
     * optional clipboard may refuse independently; it never creates overlap. */
    if (it.data->width_in_pixels < width || it.data->height_in_pixels < height) goto fail;
    h->width = (uint16_t)width; h->height = (uint16_t)height;
    h->window = xcb_generate_id(h->c);
    uint32_t values[] = { it.data->black_pixel,
        XCB_EVENT_MASK_STRUCTURE_NOTIFY | XCB_EVENT_MASK_FOCUS_CHANGE };
    if (!checked(h, xcb_create_window_checked(h->c, XCB_COPY_FROM_PARENT,
        h->window, it.data->root, origin, origin, h->width, h->height, 0,
        XCB_WINDOW_CLASS_INPUT_OUTPUT, it.data->root_visual,
        XCB_CW_BACK_PIXEL | XCB_CW_EVENT_MASK, values))) goto fail;
    h->protocols = atom(h, "WM_PROTOCOLS");
    h->close = atom(h, "WM_DELETE_WINDOW");
    h->utf8 = atom(h, "UTF8_STRING");
    h->net_name = atom(h, "_NET_WM_NAME");
    const char title[] = "FrankenRemote";
    const char class[] = "franken-remote\0FrankenRemote\0";
    /* This first layout is immutable. Clipboard layout may have selected a
     * smaller target before creation; Rust must use fitted decoding AND its
     * matching viewport. A later resize is terminal, never an implicit remap. */
    uint32_t size[18] = {0};
    size[0] = (1u << 4) | (1u << 5); /* ICCCM PMinSize | PMaxSize */
    if (origin) {
        size[0] |= 1u << 2; /* PPosition: initial space for local decorations */
        size[1] = size[2] = (uint32_t)origin;
    }
    size[5] = size[7] = width; size[6] = size[8] = height;
    if (!h->close || !h->utf8 || !h->net_name ||
        !property(h, h->protocols, XCB_ATOM_ATOM, 32, 1, &h->close) ||
        !property(h, XCB_ATOM_WM_NAME, XCB_ATOM_STRING, 8, sizeof(title)-1, title) ||
        !property(h, h->net_name, h->utf8, 8, sizeof(title)-1, title) ||
        !property(h, XCB_ATOM_WM_CLASS, XCB_ATOM_STRING, 8, sizeof(class)-1, class) ||
        !property(h, XCB_ATOM_WM_NORMAL_HINTS, XCB_ATOM_WM_SIZE_HINTS, 32, 18, size) ||
        !checked(h, xcb_map_window_checked(h->c, h->window))) goto fail;
    *window = h->window; *actual_width = width; *actual_height = height; return h;
fail:
    fr_viewer_window_close(h); return NULL;
}
struct fr_viewer_window *fr_viewer_window_open(const char *display,
        uint32_t width, uint32_t height, uint32_t *window) {
    uint32_t actual_width, actual_height;
    return viewer_window_open(display, width, height, 0, window, &actual_width, &actual_height);
}
struct fr_viewer_window *fr_viewer_window_open_with_controls(const char *display,
        uint32_t width, uint32_t height, uint32_t *window,
        uint32_t *actual_width, uint32_t *actual_height) {
    return viewer_window_open(display, width, height, 1, window, actual_width, actual_height);
}
/* A server timestamp barrier, following the existing viewer input boundary.
 * Its exact request sequence excludes another local client's property write. */
static int control_barrier(struct fr_viewer_window *h, uint8_t kind) {
    if (h->control_sequence) return 0;
    uint32_t value = kind;
    xcb_void_cookie_t request = xcb_change_property_checked(h->c, XCB_PROP_MODE_REPLACE,
        h->window, h->control_clock, XCB_ATOM_CARDINAL, 32, 1, &value);
    h->control_sequence = request.sequence; h->control_barrier_kind = kind;
    return request.sequence && xcb_flush(h->c) > 0;
}
static int control_held(struct fr_viewer_window *h) {
    xcb_generic_error_t *e = NULL;
    xcb_query_keymap_reply_t *keys = xcb_query_keymap_reply(h->c,
        xcb_query_keymap(h->c), &e);
    int valid = keys && !e, held = 0;
    if (valid) for (unsigned i = 0; i < sizeof(keys->keys); ++i) held |= keys->keys[i];
    free(keys); free(e);
    return !valid || xcb_connection_has_error(h->c) ? -1 : !!held;
}
/* Locks may remain active with no physical key held. Accept only modifier
 * slots whose actual mapping contains exclusively known lock keys. */
static int control_lock_mask(struct fr_viewer_window *h, const xcb_setup_t *s,
        const xcb_get_keyboard_mapping_reply_t *map) {
    xcb_generic_error_t *e = NULL;
    xcb_get_modifier_mapping_reply_t *r = xcb_get_modifier_mapping_reply(h->c,
        xcb_get_modifier_mapping(h->c), &e);
    if (!r || e || r->keycodes_per_modifier > 32 ||
        xcb_get_modifier_mapping_keycodes_length(r) != 8 * r->keycodes_per_modifier) {
        free(r); free(e); return 0;
    }
    xcb_keycode_t *codes = xcb_get_modifier_mapping_keycodes(r);
    xcb_keysym_t *symbols = xcb_get_keyboard_mapping_keysyms(map);
    h->control_locks = 0;
    for (unsigned slot = 0; slot < 8; ++slot) {
        int found = 0, only_locks = 1;
        for (unsigned i = 0; i < r->keycodes_per_modifier; ++i) {
            unsigned code = codes[slot * r->keycodes_per_modifier + i];
            if (!code) continue;
            found = 1;
            if (code < s->min_keycode || code > s->max_keycode) { only_locks = 0; break; }
            uint32_t symbol = symbols[(code - s->min_keycode) * map->keysyms_per_keycode];
            if (symbol != 0xffe5 && symbol != 0xff7f && symbol != 0xff14) only_locks = 0;
        }
        if (found && only_locks) h->control_locks |= (uint16_t)(1u << slot);
    }
    free(r); free(e); return 1;
}
/* Local UI only: no grab, pixel overlay, viewport change or input grant. */
int fr_viewer_window_control_offer(struct fr_viewer_window *h, int enabled) {
    if (!h || xcb_connection_has_error(h->c)) return -1;
    h->control_key = h->control_pressed = h->control_ready = 0;
    h->control_sequence = h->control_barrier_kind = 0;
    h->control_age = UINT32_MAX;
    if (enabled) {
        const xcb_setup_t *s = xcb_get_setup(h->c);
        unsigned count = (unsigned)s->max_keycode - s->min_keycode + 1;
        if (!count || count > 248) return -1;
        xcb_generic_error_t *e = NULL;
        xcb_get_keyboard_mapping_reply_t *r = xcb_get_keyboard_mapping_reply(h->c,
            xcb_get_keyboard_mapping(h->c, s->min_keycode, (uint8_t)count), &e);
        if (!r || e || !r->keysyms_per_keycode || r->keysyms_per_keycode > 32 ||
            xcb_get_keyboard_mapping_keysyms_length(r) != (int)(count * r->keysyms_per_keycode)) {
            free(r); free(e); return -1;
        }
        xcb_keysym_t *symbols = xcb_get_keyboard_mapping_keysyms(r);
        unsigned matches = 0;
        for (unsigned k = 0; k < count; ++k) {
            if (symbols[k * r->keysyms_per_keycode] == 0xffc5) {
                h->control_key = (uint8_t)(s->min_keycode + k); ++matches;
            }
        }
        int locks = control_lock_mask(h, s, r);
        free(r); free(e);
        if (!locks) return -1;
        if (matches != 1) h->control_key = 0;
        int held = control_held(h);
        if (held < 0) return -1;
        h->control_blocked = (uint8_t)held;
        if (!h->control_clock) h->control_clock = atom(h, "_FR_VIEWER_CONTROL_CLOCK");
        if (!h->control_clock) return -1;
    }
    const char *title = !enabled ? "FrankenRemote" : h->control_key ?
        "FrankenRemote - View only - F8 requests control" :
        "FrankenRemote - View only - F8 unavailable";
    uint32_t mask = XCB_EVENT_MASK_STRUCTURE_NOTIFY | XCB_EVENT_MASK_FOCUS_CHANGE;
    if (h->control_key) mask |= XCB_EVENT_MASK_KEY_PRESS | XCB_EVENT_MASK_KEY_RELEASE |
        XCB_EVENT_MASK_PROPERTY_CHANGE;
    if (h->files) mask |= XCB_EVENT_MASK_PROPERTY_CHANGE;
    if (!checked(h, xcb_change_window_attributes_checked(h->c, h->window,
        XCB_CW_EVENT_MASK, &mask)) ||
        !property(h, XCB_ATOM_WM_NAME, XCB_ATOM_STRING, 8, (uint32_t)strlen(title), title) ||
        !property(h, h->net_name, h->utf8, 8, (uint32_t)strlen(title), title)) return -1;
    if (h->control_key && !control_barrier(h, 1)) return -1;
    return !enabled || h->control_key ? 1 : 0;
}
void fr_viewer_window_control_reset(struct fr_viewer_window *h) {
    if (!h) return;
    h->control_pressed = 0; h->control_age = UINT32_MAX;
    if (h->control_barrier_kind == 2) h->control_barrier_kind = 0;
}
uint32_t fr_viewer_window_control_age(struct fr_viewer_window *h) {
    return h ? h->control_age : UINT32_MAX;
}
/* Called after the whole bounded event batch. Core autorepeat still reports a
 * held F8 through QueryKeymap and cannot become a request before real release. */
int fr_viewer_window_control_released(struct fr_viewer_window *h) {
    if (!h || !h->control_key || xcb_connection_has_error(h->c)) return -1;
    if (h->control_age >= 250 || h->control_blocked) return 0;
    xcb_generic_error_t *e = NULL;
    xcb_get_input_focus_reply_t *f = xcb_get_input_focus_reply(h->c,
        xcb_get_input_focus(h->c), &e);
    int valid = f && !e, focused = valid && f->focus == h->window;
    free(f); free(e);
    if (!valid) return -1;
    if (!focused) return 0;
    int held = control_held(h);
    return held < 0 ? -1 : !held;
}
/* Optional automation evidence on the ORIGINAL connection. At most one fixed
 * reply is outstanding, and Rust owns its immutable deadline. Neither call
 * blocks for a reply or changes focus; FocusIn is never positive evidence. */
int fr_viewer_window_focus_begin(struct fr_viewer_window *h) {
    if (!h || h->focus_query || xcb_connection_has_error(h->c)) return -1;
    h->focus_changed = 0;
    h->focus_query = xcb_get_input_focus(h->c).sequence;
    return h->focus_query && xcb_flush(h->c) > 0 ? 1 : -1;
}
/* 0 pending; 1 not focused; 2 focused; -1 malformed/unavailable. */
int fr_viewer_window_focus_poll(struct fr_viewer_window *h) {
    if (!h || !h->focus_query || xcb_connection_has_error(h->c)) return -1;
    void *raw = NULL; xcb_generic_error_t *error = NULL;
    if (!xcb_poll_for_reply(h->c, h->focus_query, &raw, &error))
        return xcb_connection_has_error(h->c) ? -1 : 0;
    xcb_get_input_focus_reply_t *r = raw;
    int valid = r && !error && r->response_type == 1 && !r->length;
    int focused = valid && r->focus == h->window && !h->focus_changed;
    free(raw); free(error); h->focus_query = 0;
    return valid ? (focused ? 2 : 1) : -1;
}
int fr_viewer_window_next(struct fr_viewer_window *h, uint32_t *kind) {
    if (!h || !kind || xcb_connection_has_error(h->c)) return -1;
    xcb_generic_event_t *e = xcb_poll_for_event(h->c);
    if (!e) {
        if (h->control_key && h->control_blocked && !h->control_sequence) {
            int held = control_held(h);
            if (held < 0) return -1;
            if (!held) {
                h->control_blocked = h->control_ready = 0;
                if (!control_barrier(h, 1)) return -1;
            }
        }
        return xcb_connection_has_error(h->c) ? -1 : 0;
    }
    *kind = IGNORE;
    if (drop_event(h, e, kind)) { free(e); return 1; }
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
    case XCB_PROPERTY_NOTIFY: {
        xcb_property_notify_event_t *v = (xcb_property_notify_event_t *)e;
        if (!synthetic && h->control_key && v->window == h->window &&
            v->atom == h->control_clock && v->state == XCB_PROPERTY_NEW_VALUE &&
            h->control_sequence && e->full_sequence == h->control_sequence) {
            uint8_t pending = h->control_barrier_kind;
            h->control_sequence = h->control_barrier_kind = 0;
            if (pending == 1) {
                /* Fence a press racing offer setup too: the pre-arm key query
                 * alone cannot exclude a key held before this server baseline. */
                int held = control_held(h);
                if (held < 0) { free(e); return -1; }
                h->control_baseline = v->time;
                h->control_blocked = (uint8_t)held;
                h->control_ready = !held;
            } else if (pending == 2) {
                uint32_t age = v->time - h->control_release_at;
                uint32_t press_age = v->time - h->control_press_at;
                if (age < 249 && press_age < 249) {
                    h->control_age = age + 1; *kind = CONTROL_CHOSEN;
                }
            }
        }
        break;
    }
    case XCB_KEY_PRESS: case XCB_KEY_RELEASE: {
        xcb_key_press_event_t *v = (xcb_key_press_event_t *)e;
        if (v->event != h->window || !h->control_key) break;
        if (synthetic || !v->same_screen || (v->state & ~h->control_locks)) {
            fr_viewer_window_control_reset(h); *kind = CONTROL_RESET; break;
        }
        if (!h->control_ready || h->control_blocked) break;
        if (type == XCB_KEY_PRESS) {
            fr_viewer_window_control_reset(h);
            uint32_t since = v->time - h->control_baseline;
            if (v->detail == h->control_key && since && since < (1u << 31)) {
                h->control_pressed = 1; h->control_press_at = v->time;
            }
        } else {
            if (h->control_pressed && v->detail == h->control_key &&
                !h->control_sequence && v->time - h->control_press_at < 250) {
                h->control_release_at = v->time;
                if (!control_barrier(h, 2)) { free(e); return -1; }
                *kind = CONTROL_CLOCK;
            }
            h->control_pressed = 0;
        }
        break;
    }
    case XCB_FOCUS_OUT:
        if (((xcb_focus_out_event_t *)e)->event == h->window) {
            h->focus_changed = 1;
            fr_viewer_window_control_reset(h); *kind = FOCUS_LEFT;
        }
        break;
    case XCB_MAPPING_NOTIFY:
        fr_viewer_window_control_reset(h);
        h->control_ready = 0; *kind = CONTROL_REMAP; break;
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

#include "file_progress.c"
