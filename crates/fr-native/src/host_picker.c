/* Pre-session, locally owned machine chooser. No network callbacks, grabs,
 * session handles or input injection. Rows are borrowed immutable display
 * data until close; only their original ordinal can leave this boundary. */
#define _POSIX_C_SOURCE 200809L
#include <xcb/xcb.h>
#include <xcb/xcbext.h>
#include <X11/extensions/XKBproto.h>
#include <sys/uio.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

struct fr_host_row { char name[255], stable_id[129]; uint32_t path; };
enum { KEY_ESCAPE, KEY_ENTER, KEY_UP, KEY_DOWN, KEY_LEFT, KEY_RIGHT,
       KEY_PAGE_UP, KEY_PAGE_DOWN, KEY_HOME, KEY_END, KEY_COUNT };
struct fr_host_picker {
    xcb_connection_t *c;
    xcb_window_t window;
    xcb_atom_t protocols, close, clock;
    xcb_font_t font;
    xcb_gcontext_t gc;
    const struct fr_host_row *rows;
    uint32_t count, selected, first, page_rows;
    uint16_t width, height, columns, line, ascent, header, row_height;
    uint8_t keycodes[KEY_COUNT], held[32], xkb;
    uint8_t mapped, focused, ready, dirty, pressed, pending, candidate, rearm;
    uint32_t press_at, press_row, baseline, sequence, barrier_kind, candidate_age;
    uint64_t barrier_mono, candidate_mono, revision;
};

static int checked(struct fr_host_picker *h, xcb_void_cookie_t cookie) {
    xcb_generic_error_t *error = xcb_request_check(h->c, cookie);
    int ok = !error && !xcb_connection_has_error(h->c);
    free(error); return ok;
}
static xcb_atom_t atom(struct fr_host_picker *h, const char *name) {
    xcb_generic_error_t *error = NULL;
    xcb_intern_atom_reply_t *reply = xcb_intern_atom_reply(h->c,
        xcb_intern_atom(h->c, 0, (uint16_t)strlen(name), name), &error);
    xcb_atom_t value = reply && !error ? reply->atom : XCB_NONE;
    free(reply); free(error); return value;
}
static int property(struct fr_host_picker *h, xcb_atom_t name, xcb_atom_t type,
                    uint8_t format, uint32_t count, const void *data) {
    return name && checked(h, xcb_change_property_checked(h->c,
        XCB_PROP_MODE_REPLACE, h->window, name, type, format, count, data));
}
static uint64_t monotonic_ms(void) {
    struct timespec value;
    if (clock_gettime(CLOCK_MONOTONIC, &value) || value.tv_sec < 0 ||
        (uint64_t)value.tv_sec > (UINT64_MAX - 999) / 1000) return 0;
    return (uint64_t)value.tv_sec * 1000 + (uint64_t)value.tv_nsec / 1000000 + 1;
}

/* Detectable repeat preserves the first physical down timestamp. Selecting
 * XKB map/name events also invalidates unchanged-valued map replacements. */
static xcb_extension_t xkb_extension = { "XKEYBOARD", 0 };
static uint32_t xkb_request(struct fr_host_picker *h, uint8_t opcode,
                            int is_void, void *body, size_t size) {
    struct iovec parts[4] = {{0}};
    parts[2].iov_base = body; parts[2].iov_len = size;
    parts[3].iov_len = (-size) & 3;
    xcb_protocol_request_t request = {2, &xkb_extension, opcode, (uint8_t)is_void};
    return xcb_send_request(h->c, XCB_REQUEST_CHECKED, parts + 2, &request);
}
static void *xkb_reply(struct fr_host_picker *h, uint32_t sequence) {
    xcb_generic_error_t *error = NULL;
    void *reply = xcb_wait_for_reply(h->c, sequence, &error);
    if (error || xcb_connection_has_error(h->c)) { free(reply); reply = NULL; }
    free(error); return reply;
}
static int keyboard(struct fr_host_picker *h) {
    static const uint32_t symbols[KEY_COUNT] = {
        0xff1b, 0xff0d, 0xff52, 0xff54, 0xff51, 0xff53,
        0xff55, 0xff56, 0xff50, 0xff57
    };
    const xcb_query_extension_reply_t *extension = xcb_get_extension_data(h->c, &xkb_extension);
    if (!extension || !extension->present) return 0;
    h->xkb = extension->first_event;
    xkbUseExtensionReq use = {0}; use.wantedMajor = 1;
    xkbUseExtensionReply *version = xkb_reply(h,
        xkb_request(h, X_kbUseExtension, 0, &use, sizeof(use)));
    int ok = version && version->supported && !version->length;
    free(version); if (!ok) return 0;
    xkbSelectEventsReq select = {0}; select.deviceSpec = XkbUseCoreKbd;
    select.affectWhich = select.selectAll = XkbNewKeyboardNotifyMask | XkbMapNotifyMask | XkbNamesNotifyMask;
    select.affectMap = select.map = XkbAllMapComponentsMask;
    xcb_void_cookie_t cookie = { xkb_request(h, X_kbSelectEvents, 1, &select, sizeof(select)) };
    if (!checked(h, cookie)) return 0;
    xkbPerClientFlagsReq flags = {0}; flags.deviceSpec = XkbUseCoreKbd;
    flags.change = flags.value = XkbPCF_DetectableAutoRepeatMask;
    xkbPerClientFlagsReply *repeat = xkb_reply(h,
        xkb_request(h, X_kbPerClientFlags, 0, &flags, sizeof(flags)));
    ok = repeat && !repeat->length && (repeat->supported & XkbPCF_DetectableAutoRepeatMask) &&
        (repeat->value & XkbPCF_DetectableAutoRepeatMask);
    free(repeat); if (!ok) return 0;
    const xcb_setup_t *setup = xcb_get_setup(h->c);
    unsigned count = (unsigned)setup->max_keycode - setup->min_keycode + 1;
    if (!count || count > 248) return 0;
    xcb_generic_error_t *error = NULL;
    xcb_get_keyboard_mapping_reply_t *map = xcb_get_keyboard_mapping_reply(h->c,
        xcb_get_keyboard_mapping(h->c, setup->min_keycode, (uint8_t)count), &error);
    if (!map || error || !map->keysyms_per_keycode || map->keysyms_per_keycode > 32 ||
        xcb_get_keyboard_mapping_keysyms_length(map) != (int)(count * map->keysyms_per_keycode)) {
        free(map); free(error); return 0;
    }
    const xcb_keysym_t *keys = xcb_get_keyboard_mapping_keysyms(map);
    for (unsigned i = 0; i < count; ++i) {
        for (unsigned key = 0; key < KEY_COUNT; ++key) {
            if (!h->keycodes[key] && keys[i * map->keysyms_per_keycode] == symbols[key])
                h->keycodes[key] = (uint8_t)(setup->min_keycode + i);
        }
    }
    free(map); free(error);
    for (unsigned key = 0; key < KEY_COUNT; ++key) if (!h->keycodes[key]) return 0;
    return 1;
}

static void invalidate(struct fr_host_picker *h) {
    ++h->revision;
    h->pressed = h->pending = h->candidate = 0;
    if (h->barrier_kind == 2) h->barrier_kind = 0;
}
static int held(const struct fr_host_picker *h) {
    for (unsigned i = 0; i < sizeof(h->held); ++i) if (h->held[i]) return 1;
    return 0;
}
static int row_at(const struct fr_host_picker *h, int16_t x, int16_t y, uint32_t *row) {
    if (x < 12 || x >= (int)h->width - 12 || y < (int)h->header) return 0;
    unsigned offset = ((unsigned)y - h->header) / h->row_height;
    if (offset >= h->page_rows || h->first + offset >= h->count) return 0;
    *row = h->first + offset; return 1;
}
/* Sample actual focus, every keyboard key, and all pointer buttons. This is a
 * final observation, not a server grab or an atomic claim about future input. */
static int released(struct fr_host_picker *h, int pointer_row) {
    xcb_generic_error_t *error = NULL;
    xcb_get_input_focus_reply_t *focus = xcb_get_input_focus_reply(h->c,
        xcb_get_input_focus(h->c), &error);
    int ok = focus && !error, focused = ok && focus->focus == h->window;
    free(focus); free(error); if (!ok) return -1;
    error = NULL;
    xcb_query_keymap_reply_t *keys = xcb_query_keymap_reply(h->c, xcb_query_keymap(h->c), &error);
    ok = keys && !error;
    int down = 0;
    if (ok) for (unsigned i = 0; i < sizeof(keys->keys); ++i) down |= keys->keys[i];
    free(keys); free(error); if (!ok) return -1;
    error = NULL;
    xcb_query_pointer_reply_t *pointer = xcb_query_pointer_reply(h->c,
        xcb_query_pointer(h->c, h->window), &error);
    ok = pointer && !error;
    uint32_t row = 0;
    int inside = ok && pointer->same_screen && (!pointer_row ||
        (row_at(h, pointer->win_x, pointer->win_y, &row) && row == h->press_row));
    if (ok) down |= pointer->mask & (XCB_BUTTON_MASK_1 | XCB_BUTTON_MASK_2 |
        XCB_BUTTON_MASK_3 | XCB_BUTTON_MASK_4 | XCB_BUTTON_MASK_5);
    free(pointer); free(error);
    return !ok || xcb_connection_has_error(h->c) ? -1 : focused && inside && !down;
}
static int barrier(struct fr_host_picker *h, uint32_t kind) {
    if (h->sequence) {
        if (kind == 1) h->rearm = 1;
        return 1;
    }
    h->barrier_mono = monotonic_ms();
    if (!h->barrier_mono) return 0;
    xcb_void_cookie_t request = xcb_change_property_checked(h->c, XCB_PROP_MODE_REPLACE,
        h->window, h->clock, XCB_ATOM_CARDINAL, 32, 1, &kind);
    h->sequence = request.sequence; h->barrier_kind = kind;
    return h->sequence && xcb_flush(h->c) > 0;
}
static int text(struct fr_host_picker *h, const char *value, int y) {
    size_t length = strlen(value), offset = 0;
    while (offset < length) {
        size_t count = length - offset;
        if (count > h->columns) count = h->columns;
        xcb_image_text_8(h->c, (uint8_t)count, h->window, h->gc, 12,
            (int16_t)(y + h->ascent), value + offset);
        y += h->line; offset += count;
    }
    return y;
}
static void colors(struct fr_host_picker *h, uint32_t foreground, uint32_t background) {
    uint32_t values[] = { foreground, background };
    xcb_change_gc(h->c, h->gc, XCB_GC_FOREGROUND | XCB_GC_BACKGROUND, values);
}
static int draw(struct fr_host_picker *h) {
    colors(h, 0x171717, 0xf5f5f5);
    xcb_clear_area(h->c, 0, h->window, 0, 0, h->width, h->height);
    int y = text(h, "Choose a machine", 8);
    text(h, "Identity is checked again before connecting.", y);
    for (uint32_t slot = 0; slot < h->page_rows && h->first + slot < h->count; ++slot) {
        uint32_t index = h->first + slot;
        uint32_t background = index == h->selected ? 0x164875 : 0xf5f5f5;
        uint32_t foreground = index == h->selected ? 0xffffff : 0x171717;
        uint32_t fill = background;
        xcb_change_gc(h->c, h->gc, XCB_GC_FOREGROUND, &fill);
        xcb_rectangle_t rectangle = { 8, (int16_t)(h->header + slot * h->row_height),
            (uint16_t)(h->width - 16), h->row_height };
        xcb_poly_fill_rectangle(h->c, h->window, h->gc, 1, &rectangle);
        colors(h, foreground, background);
        char value[272];
        snprintf(value, sizeof(value), "Machine: %s", h->rows[index].name);
        y = text(h, value, rectangle.y + 4);
        snprintf(value, sizeof(value), "Node ID: %s", h->rows[index].stable_id);
        y = text(h, value, y);
        const char *path = h->rows[index].path == 1 ? "Path hint: direct" :
            h->rows[index].path == 2 ? "Path hint: relayed" : "Path hint: unknown";
        text(h, path, y);
    }
    colors(h, 0x171717, 0xf5f5f5);
    char page[80];
    uint32_t last = h->first + h->page_rows;
    if (last > h->count) last = h->count;
    snprintf(page, sizeof(page), "Hosts %u-%u of %u", h->first + 1, last, h->count);
    y = (int)h->height - 4 * h->line - 8;
    y = text(h, page, y);
    y = text(h, "Arrows: move  Page Up/Down: page  Home/End", y);
    text(h, "Enter or click: choose  Esc: cancel", y);
    /* Local accessibility/geometry metadata. It is not read for selection. */
    uint32_t layout[] = { h->width, h->height, h->header, h->row_height,
        h->first, h->page_rows, h->selected, h->count };
    if (!property(h, atom(h, "_FR_HOST_PICKER_LAYOUT"), XCB_ATOM_CARDINAL, 32, 8, layout)) return 0;
    h->dirty = 0;
    return xcb_flush(h->c) > 0;
}

void fr_host_picker_close(struct fr_host_picker *h) {
    if (!h) return;
    if (h->c) xcb_disconnect(h->c);
    free(h);
}
struct fr_host_picker *fr_host_picker_open(const char *display,
        const struct fr_host_row *rows, uint32_t count, uint32_t *window) {
    if (!display || !rows || !count || count > 1024 || !window) return NULL;
    *window = XCB_NONE;
    for (uint32_t i = 0; i < count; ++i) {
        if (!rows[i].name[0] || !memchr(rows[i].name, 0, sizeof(rows[i].name)) ||
            !rows[i].stable_id[0] || !memchr(rows[i].stable_id, 0, sizeof(rows[i].stable_id)) || rows[i].path > 2)
            return NULL;
    }
    struct fr_host_picker *h = calloc(1, sizeof(*h));
    if (!h) return NULL;
    h->rows = rows; h->count = count;
    int screen = 0;
    h->c = xcb_connect(display, &screen);
    if (!h->c || xcb_connection_has_error(h->c)) goto fail;
    xcb_screen_iterator_t it = xcb_setup_roots_iterator(xcb_get_setup(h->c));
    while (screen-- > 0 && it.rem) xcb_screen_next(&it);
    if (!it.rem || it.data->root_depth != 24 || it.data->width_in_pixels < 352 ||
        it.data->height_in_pixels < 360) goto fail;
    h->width = it.data->width_in_pixels > 992 ? 960 : (uint16_t)(it.data->width_in_pixels - 32);
    h->height = it.data->height_in_pixels > 784 ? 720 : (uint16_t)(it.data->height_in_pixels - 64);
    h->font = xcb_generate_id(h->c);
    if (!checked(h, xcb_open_font_checked(h->c, h->font, 5, "fixed"))) goto fail;
    xcb_generic_error_t *error = NULL;
    xcb_query_font_reply_t *font = xcb_query_font_reply(h->c, xcb_query_font(h->c, h->font), &error);
    int valid = font && !error && font->min_bounds.character_width > 0 &&
        font->min_bounds.character_width == font->max_bounds.character_width &&
        font->max_bounds.character_width <= 32 && font->font_ascent > 0 &&
        font->font_ascent + font->font_descent > 0 && font->font_ascent + font->font_descent <= 32;
    if (valid) {
        h->columns = (uint16_t)((h->width - 24) / font->max_bounds.character_width);
        if (h->columns > 240) h->columns = 240;
        h->ascent = (uint16_t)font->font_ascent;
        h->line = (uint16_t)(font->font_ascent + font->font_descent + 3);
    }
    free(font); free(error); if (!valid || h->columns < 40) goto fail;
    h->header = (uint16_t)(4 * h->line + 12);
    h->row_height = (uint16_t)((((263u + h->columns - 1) / h->columns) +
        ((137u + h->columns - 1) / h->columns) + 2) * h->line + 8);
    if (h->height < h->header + 4 * h->line + 12 + h->row_height) goto fail;
    h->page_rows = (h->height - h->header - 4 * h->line - 12) / h->row_height;
    h->window = xcb_generate_id(h->c);
    uint32_t values[] = { 0xf5f5f5, XCB_EVENT_MASK_EXPOSURE | XCB_EVENT_MASK_STRUCTURE_NOTIFY |
        XCB_EVENT_MASK_FOCUS_CHANGE | XCB_EVENT_MASK_KEY_PRESS | XCB_EVENT_MASK_KEY_RELEASE |
        XCB_EVENT_MASK_BUTTON_PRESS | XCB_EVENT_MASK_BUTTON_RELEASE | XCB_EVENT_MASK_POINTER_MOTION |
        XCB_EVENT_MASK_PROPERTY_CHANGE };
    if (!checked(h, xcb_create_window_checked(h->c, XCB_COPY_FROM_PARENT, h->window,
        it.data->root, 16, 16, h->width, h->height, 0, XCB_WINDOW_CLASS_INPUT_OUTPUT,
        it.data->root_visual, XCB_CW_BACK_PIXEL | XCB_CW_EVENT_MASK, values))) goto fail;
    h->protocols = atom(h, "WM_PROTOCOLS"); h->close = atom(h, "WM_DELETE_WINDOW");
    h->clock = atom(h, "_FR_HOST_PICKER_CLOCK");
    const char title[] = "FrankenRemote - Choose a machine";
    const char class_name[] = "fr\0FrankenRemote\0";
    uint32_t size_hints[18] = {0}, input_hints[9] = {1, 1};
    size_hints[0] = (1u << 4) | (1u << 5);
    size_hints[5] = size_hints[7] = h->width;
    size_hints[6] = size_hints[8] = h->height;
    if (!h->protocols || !h->close || !h->clock ||
        !property(h, h->protocols, XCB_ATOM_ATOM, 32, 1, &h->close) ||
        !property(h, XCB_ATOM_WM_NAME, XCB_ATOM_STRING, 8, sizeof(title) - 1, title) ||
        !property(h, XCB_ATOM_WM_CLASS, XCB_ATOM_STRING, 8, sizeof(class_name) - 1, class_name) ||
        !property(h, XCB_ATOM_WM_NORMAL_HINTS, XCB_ATOM_WM_SIZE_HINTS, 32, 18, size_hints) ||
        !property(h, XCB_ATOM_WM_HINTS, XCB_ATOM_WM_HINTS, 32, 9, input_hints) || !keyboard(h)) goto fail;
    h->gc = xcb_generate_id(h->c);
    uint32_t gc[] = {0x171717, 0xf5f5f5, h->font};
    if (!checked(h, xcb_create_gc_checked(h->c, h->gc, h->window,
        XCB_GC_FOREGROUND | XCB_GC_BACKGROUND | XCB_GC_FONT, gc)) ||
        !checked(h, xcb_map_window_checked(h->c, h->window)) || xcb_flush(h->c) <= 0) goto fail;
    h->dirty = 1; *window = h->window; return h;
fail:
    fr_host_picker_close(h); return NULL;
}

static void navigate(struct fr_host_picker *h, unsigned key) {
    if (key == KEY_HOME) h->selected = 0;
    else if (key == KEY_END) h->selected = h->count - 1;
    else if (key == KEY_UP && h->selected) --h->selected;
    else if (key == KEY_DOWN && h->selected + 1 < h->count) ++h->selected;
    else if (key == KEY_PAGE_UP || key == KEY_LEFT)
        h->selected = h->selected > h->page_rows ? h->selected - h->page_rows : 0;
    else if (key == KEY_PAGE_DOWN || key == KEY_RIGHT)
        h->selected = h->selected + h->page_rows < h->count ? h->selected + h->page_rows : h->count - 1;
    h->first = (h->selected / h->page_rows) * h->page_rows; h->dirty = 1;
}
static int event(struct fr_host_picker *h, xcb_generic_event_t *event) {
    uint8_t type = event->response_type & 0x7f;
    int synthetic = (event->response_type & 0x80) != 0;
    if (!type) return -1;
    if (type == XCB_MAPPING_NOTIFY || type == h->xkb) { invalidate(h); return 4; }
    if (type == XCB_CLIENT_MESSAGE) {
        const xcb_client_message_event_t *close = (const void *)event;
        if (close->window == h->window && close->type == h->protocols &&
            close->format == 32 && close->data.data32[0] == h->close) { invalidate(h); return 3; }
        return 0;
    }
    if (type == XCB_DESTROY_NOTIFY || type == XCB_UNMAP_NOTIFY) { invalidate(h); return 3; }
    if (type == XCB_CONFIGURE_NOTIFY) {
        const xcb_configure_notify_event_t *configure = (const void *)event;
        if (configure->window == h->window && (configure->width != h->width || configure->height != h->height))
            return 4;
    }
    if (type == XCB_FOCUS_OUT) { invalidate(h); h->ready = h->focused = h->rearm = 0; return 0; }
    if (type == XCB_FOCUS_IN && !synthetic) {
        invalidate(h); h->ready = 0; h->focused = 1;
        return barrier(h, 1) ? 0 : -1;
    }
    if (type == XCB_MAP_NOTIFY && !synthetic) { h->mapped = 1; h->dirty = 1; }
    if (type == XCB_EXPOSE) h->dirty = 1;
    if (type == XCB_PROPERTY_NOTIFY) {
        const xcb_property_notify_event_t *clock = (const void *)event;
        if (synthetic || clock->window != h->window || clock->atom != h->clock ||
            clock->state != XCB_PROPERTY_NEW_VALUE || !h->sequence || event->full_sequence != h->sequence) return 0;
        uint32_t kind = h->barrier_kind;
        h->sequence = h->barrier_kind = 0;
        if (h->rearm) {
            /* A focus/held-state reset must not consume the old clock as its
             * baseline or resurrect that clock's discarded positive intent. */
            h->rearm = h->ready = 0;
            invalidate(h);
            return !h->focused || barrier(h, 1) ? 0 : -1;
        }
        if (kind == 1 && h->focused) {
            int all_up = released(h, 0);
            if (all_up < 0) return -1;
            h->baseline = clock->time; h->ready = (uint8_t)all_up;
            if (all_up) memset(h->held, 0, sizeof(h->held));
        } else if (kind == 2 && h->pending) {
            h->candidate_age = clock->time - h->press_at;
            h->candidate_mono = h->barrier_mono;
            h->candidate = h->pending; h->pending = 0;
        }
        return 0;
    }
    if (type == XCB_KEY_PRESS || type == XCB_KEY_RELEASE) {
        const xcb_key_press_event_t *key = (const void *)event;
        if (key->event != h->window) return 0;
        if (synthetic || !key->same_screen || key->detail < 8) { invalidate(h); return 0; }
        unsigned slot = KEY_COUNT;
        for (unsigned i = 0; i < KEY_COUNT; ++i) if (h->keycodes[i] == key->detail) { slot = i; break; }
        unsigned byte = key->detail / 8, mask = 1u << (key->detail % 8);
        int was_down = !!(h->held[byte] & mask), before_held = held(h), down = type == XCB_KEY_PRESS;
        if (down) h->held[byte] |= (uint8_t)mask; else h->held[byte] &= (uint8_t)~mask;
        if (slot == KEY_ESCAPE && down) { invalidate(h); return 3; }
        if (down && was_down && h->pressed == 1 && slot == KEY_ENTER) return 0;
        if (!down && was_down && h->pressed == 1 && slot == KEY_ENTER && !held(h)) {
            h->pressed = 0; h->pending = 1;
            return barrier(h, 2) ? 0 : -1;
        }
        invalidate(h);
        if (!h->ready) return !down && h->focused ? (barrier(h, 1) ? 0 : -1) : 0;
        uint32_t since = key->time - h->baseline;
        if (down && !was_down && !before_held && since && since < (1u << 31)) {
            if (slot == KEY_ENTER) {
                h->pressed = 1; h->press_at = key->time; h->press_row = h->selected;
            } else if (slot < KEY_COUNT) navigate(h, slot);
        }
        return 0;
    }
    if (type == XCB_BUTTON_PRESS || type == XCB_BUTTON_RELEASE) {
        const xcb_button_press_event_t *button = (const void *)event;
        if (button->event != h->window) return 0;
        if (synthetic || !button->same_screen) { invalidate(h); return 0; }
        uint32_t row = 0;
        int inside = row_at(h, button->event_x, button->event_y, &row), down = type == XCB_BUTTON_PRESS;
        if (!down && button->detail == 1 && h->pressed == 2 && inside && row == h->press_row) {
            h->pressed = 0; h->pending = 2;
            return barrier(h, 2) ? 0 : -1;
        }
        invalidate(h);
        if (!h->ready) return !down && h->focused ? (barrier(h, 1) ? 0 : -1) : 0;
        uint32_t since = button->time - h->baseline;
        if (down && !held(h) && since && since < (1u << 31)) {
            if (button->detail == 1 && inside) {
                h->selected = h->press_row = row; h->press_at = button->time;
                h->pressed = 2; h->dirty = 1;
            } else if (button->detail == 4 || button->detail == 5)
                navigate(h, button->detail == 4 ? KEY_PAGE_UP : KEY_PAGE_DOWN);
        }
        return 0;
    }
    if (type == XCB_MOTION_NOTIFY && h->pressed == 2) {
        const xcb_motion_notify_event_t *motion = (const void *)event;
        uint32_t row = 0;
        if (synthetic || !row_at(h, motion->event_x, motion->event_y, &row) || row != h->press_row)
            invalidate(h);
    }
    return 0;
}

/* One bounded batch, including events queued by final focus/key/button
 * queries. A later event can invalidate an earlier candidate in this batch. */
int fr_host_picker_step(struct fr_host_picker *h, uint32_t *ordinal, uint32_t *age_ms) {
    if (!h || !ordinal || !age_ms || xcb_connection_has_error(h->c)) return -1;
    unsigned budget = 128;
    uint64_t verified_revision = 0;
    int verified = 0;
    for (unsigned phase = 0; phase < 2; ++phase) {
        xcb_generic_event_t *next;
        while ((next = xcb_poll_for_event(h->c))) {
            int result = event(h, next); free(next);
            if (result) return result;
            if (!--budget) return -2;
        }
        if (xcb_connection_has_error(h->c)) return -1;
        if (!h->candidate) break;
        if (!phase) {
            int all_up = released(h, h->candidate == 2);
            if (all_up < 0) return -1;
            if (!all_up) invalidate(h);
            else { verified_revision = h->revision; verified = 1; }
        }
    }
    if (h->candidate && verified && verified_revision == h->revision) {
        uint64_t now = monotonic_ms();
        if (!now || now < h->candidate_mono) return -1;
        uint64_t elapsed = now - h->candidate_mono;
        uint64_t age = (uint64_t)h->candidate_age + elapsed + 1;
        h->candidate = 0;
        if (age < 250) {
            *ordinal = h->press_row; *age_ms = (uint32_t)age; return 2;
        }
    }
    if (h->dirty && h->mapped && !draw(h)) return -1;
    return h->mapped ? 1 : 0;
}
