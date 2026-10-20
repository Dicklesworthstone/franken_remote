/* Included by viewer_window.c. Separate, non-focus-taking utility window.
 * The decoder's drawable and coordinates are NEVER changed or drawn over.
 * All native work stays on the panel's own connection/owner thread. */
enum { UP_WIDTH = 640, UP_HEIGHT = 310, UP_ROWS = 8, UP_COLS = 104 };
struct fr_upload_frame {
    uint8_t lines[UP_ROWS][UP_COLS];
    int32_t progress;
    uint32_t actions;
};
struct fr_upload {
    struct fr_viewer_window *base;
    xcb_gcontext_t gc;
    uint32_t actions;
    uint32_t pressed;
    uint32_t parent, parent_width, parent_height;
};
void fr_upload_close(struct fr_upload *p) {
    if (!p) return;
    fr_viewer_window_close(p->base);
    free(p);
}
void fr_upload_disarm(struct fr_upload *p) { if (p) p->pressed = 0; }
struct fr_upload *fr_upload_open(const char *display, uint32_t parent,
        uint32_t width, uint32_t height) {
    if (!display || !parent || !width || !height) return NULL;
    struct fr_upload *p = calloc(1, sizeof(*p));
    if (!p) return NULL;
    p->base = calloc(1, sizeof(*p->base));
    if (!p->base) goto fail;
    struct fr_viewer_window *h = p->base;
    int screen = 0;
    h->c = xcb_connect(display, &screen);
    if (!h->c || xcb_connection_has_error(h->c)) goto fail;
    xcb_screen_iterator_t it = xcb_setup_roots_iterator(xcb_get_setup(h->c));
    while (screen-- > 0 && it.rem) xcb_screen_next(&it);
    if (!it.rem || it.data->root_depth != 24 ||
        it.data->width_in_pixels < UP_WIDTH || it.data->height_in_pixels < UP_HEIGHT) goto fail;
    /* Refuse a stale/mismatched local parent before creating any UI resource. */
    xcb_generic_error_t *e = NULL;
    xcb_get_geometry_reply_t *g = xcb_get_geometry_reply(h->c, xcb_get_geometry(h->c, parent), &e);
    int valid = g && !e && g->root == it.data->root && g->width == width && g->height == height;
    free(g); free(e); e = NULL;
    if (!valid) goto fail;
    xcb_get_window_attributes_reply_t *a = xcb_get_window_attributes_reply(h->c,
        xcb_get_window_attributes(h->c, parent), &e);
    valid = a && !e && a->map_state == XCB_MAP_STATE_VIEWABLE;
    free(a); free(e);
    if (!valid) goto fail;
    p->parent = parent; p->parent_width = width; p->parent_height = height;
    uint32_t parent_events = XCB_EVENT_MASK_STRUCTURE_NOTIFY;
    if (!checked(h, xcb_change_window_attributes_checked(h->c, parent,
        XCB_CW_EVENT_MASK, &parent_events))) goto fail;
    h->width = UP_WIDTH; h->height = UP_HEIGHT;
    h->window = xcb_generate_id(h->c);
    uint32_t values[] = { 0xffffff, XCB_EVENT_MASK_STRUCTURE_NOTIFY | XCB_EVENT_MASK_EXPOSURE |
        XCB_EVENT_MASK_BUTTON_PRESS | XCB_EVENT_MASK_BUTTON_RELEASE | XCB_EVENT_MASK_LEAVE_WINDOW |
        XCB_EVENT_MASK_FOCUS_CHANGE };
    if (!checked(h, xcb_create_window_checked(h->c, XCB_COPY_FROM_PARENT,
        h->window, it.data->root, 0, 0, UP_WIDTH, UP_HEIGHT, 0,
        XCB_WINDOW_CLASS_INPUT_OUTPUT, it.data->root_visual,
        XCB_CW_BACK_PIXEL | XCB_CW_EVENT_MASK, values))) goto fail;
    h->protocols = atom(h, "WM_PROTOCOLS"); h->close = atom(h, "WM_DELETE_WINDOW");
    xcb_atom_t utf8 = atom(h, "UTF8_STRING"), utility = atom(h, "_NET_WM_WINDOW_TYPE_UTILITY");
    /* Set InputHint=false BEFORE map. No WM_TAKE_FOCUS protocol, SetInputFocus,
     * grabs, raise, or root/global input subscription exists on this boundary. */
    uint32_t hints[9] = {1, 0, 0, 0, 0, 0, 0, 0, 0};
    uint32_t size[18] = {0};
    size[0] = (1u << 4) | (1u << 5);
    size[5] = size[7] = UP_WIDTH; size[6] = size[8] = UP_HEIGHT;
    const char title[] = "FrankenRemote - upload progress";
    const char class[] = "franken-remote-upload\0FrankenRemote\0";
    xcb_font_t font = xcb_generate_id(h->c);
    if (!h->close || !utf8 || !utility ||
        !property(h, h->protocols, XCB_ATOM_ATOM, 32, 1, &h->close) ||
        !property(h, XCB_ATOM_WM_HINTS, XCB_ATOM_WM_HINTS, 32, 9, hints) ||
        !property(h, XCB_ATOM_WM_TRANSIENT_FOR, XCB_ATOM_WINDOW, 32, 1, &parent) ||
        !property(h, atom(h, "_NET_WM_WINDOW_TYPE"), XCB_ATOM_ATOM, 32, 1, &utility) ||
        !property(h, XCB_ATOM_WM_NAME, XCB_ATOM_STRING, 8, sizeof(title)-1, title) ||
        !property(h, atom(h, "_NET_WM_NAME"), utf8, 8, sizeof(title)-1, title) ||
        !property(h, XCB_ATOM_WM_CLASS, XCB_ATOM_STRING, 8, sizeof(class)-1, class) ||
        !property(h, XCB_ATOM_WM_NORMAL_HINTS, XCB_ATOM_WM_SIZE_HINTS, 32, 18, size) ||
        !checked(h, xcb_open_font_checked(h->c, font, 5, "fixed"))) goto fail;
    p->gc = xcb_generate_id(h->c);
    uint32_t gc[] = { 0, 0xffffff, font };
    if (!checked(h, xcb_create_gc_checked(h->c, p->gc, h->window,
        XCB_GC_FOREGROUND | XCB_GC_BACKGROUND | XCB_GC_FONT, gc)) ||
        !checked(h, xcb_map_window_checked(h->c, h->window))) goto fail;
    return p;
fail:
    fr_upload_close(p); return NULL;
}
static int upload_text(struct fr_upload *p, int16_t x, int16_t y, const char *text, size_t n) {
    return n < 256 && checked(p->base, xcb_image_text_8_checked(p->base->c,
        (uint8_t)n, p->base->window, p->gc, x, y, text));
}
static int upload_box(struct fr_upload *p, int16_t x, int16_t y, uint16_t w, uint16_t height, int fill) {
    xcb_rectangle_t r = { x, y, w, height };
    struct fr_viewer_window *h = p->base;
    return checked(h, fill ? xcb_poly_fill_rectangle_checked(h->c, h->window, p->gc, 1, &r)
        : xcb_poly_rectangle_checked(h->c, h->window, p->gc, 1, &r));
}
int fr_upload_draw(struct fr_upload *p, const struct fr_upload_frame *f) {
    if (!p || !f || f->progress < -1 || f->progress > 1000 || (f->actions & ~7u)) return 0;
    size_t sizes[UP_ROWS];
    for (unsigned row = 0; row < UP_ROWS; ++row) {
        sizes[row] = 0;
        while (sizes[row] < UP_COLS && f->lines[row][sizes[row]]) {
            uint8_t c = f->lines[row][sizes[row]++];
            if (c < 32 || c > 126) return 0;
        }
        if (sizes[row] == UP_COLS) return 0;
    }
    if (p->actions != f->actions) p->pressed = 0;
    p->actions = f->actions;
    struct fr_viewer_window *h = p->base;
    if (!checked(h, xcb_clear_area_checked(h->c, 0, h->window, 0, 0, 0, 0))) return 0;
    const char heading[] = "Upload progress - original selection only";
    if (!upload_text(p, 16, 23, heading, sizeof(heading)-1)) return 0;
    for (unsigned row = 0; row < UP_ROWS; ++row) {
        if (!upload_text(p, 16, (int16_t)(46 + row * 20), (const char *)f->lines[row], sizes[row])) return 0;
    }
    if (!upload_box(p, 16, 200, 606, 16, 0)) return 0;
    if (f->progress > 0 && !upload_box(p, 18, 202, (uint16_t)(602 * (uint32_t)f->progress / 1000), 12, 1)) return 0;
    const char *labels[] = { "Pause", "Resume", "Cancel upload" };
    for (unsigned i = 0; i < 3; ++i) {
        int16_t x = (int16_t)(16 + i * 204);
        if (!upload_box(p, x, 234, 188, 32, 0) ||
            !upload_text(p, (int16_t)(x + 10), 254, labels[i], strlen(labels[i]))) return 0;
        if (!(p->actions & (1u << i)) && !upload_text(p, (int16_t)(x + 110), 254, "(off)", 5)) return 0;
    }
    const char footer[] = "Buttons request changes. Closing this panel only hides progress.";
    return upload_text(p, 16, 290, footer, sizeof(footer)-1);
}
static uint32_t upload_hit(int16_t x, int16_t y) {
    if (y < 234 || y >= 266) return 0;
    for (unsigned i = 0; i < 3; ++i) {
        int left = 16 + (int)i * 204;
        if (x >= left && x < left + 188) return 1u << i;
    }
    return 0;
}
/* kind: 0 ignored, 1 redraw, 2 panel closed/lost, 3 local command. Every
 * consumed event returns, including synthetic/ignored events, for Rust's bound. */
int fr_upload_next(struct fr_upload *p, uint32_t *kind, uint32_t *action) {
    if (!p || !kind || !action || xcb_connection_has_error(p->base->c)) return -1;
    struct fr_viewer_window *h = p->base;
    xcb_generic_event_t *e = xcb_poll_for_event(h->c);
    if (!e) return xcb_connection_has_error(h->c) ? -1 : 0;
    *kind = *action = 0;
    uint8_t type = e->response_type & 0x7f;
    int synthetic = e->response_type & 0x80;
    if (!type) { free(e); return -1; }
    switch (type) {
    case XCB_EXPOSE:
        if (!synthetic && ((xcb_expose_event_t *)e)->window == h->window) *kind = 1;
        break;
    case XCB_MAP_NOTIFY:
        if (!synthetic && ((xcb_map_notify_event_t *)e)->window == h->window) *kind = 1;
        break;
    case XCB_UNMAP_NOTIFY:
        if (((xcb_unmap_notify_event_t *)e)->window == h->window ||
            ((xcb_unmap_notify_event_t *)e)->window == p->parent) *kind = 2;
        break;
    case XCB_DESTROY_NOTIFY:
        if (((xcb_destroy_notify_event_t *)e)->window == h->window ||
            ((xcb_destroy_notify_event_t *)e)->window == p->parent) *kind = 2;
        break;
    case XCB_CONFIGURE_NOTIFY: {
        xcb_configure_notify_event_t *v = (xcb_configure_notify_event_t *)e;
        if ((v->window == h->window && (v->width != UP_WIDTH || v->height != UP_HEIGHT)) ||
            (v->window == p->parent && (v->width != p->parent_width || v->height != p->parent_height))) *kind = 2;
        break;
    }
    case XCB_CLIENT_MESSAGE: {
        xcb_client_message_event_t *v = (xcb_client_message_event_t *)e;
        if (v->window == h->window && v->type == h->protocols && v->format == 32 && v->data.data32[0] == h->close) *kind = 2;
        break;
    }
    case XCB_LEAVE_NOTIFY: case XCB_FOCUS_OUT: p->pressed = 0; break;
    case XCB_BUTTON_PRESS: case XCB_BUTTON_RELEASE: {
        xcb_button_press_event_t *v = (xcb_button_press_event_t *)e;
        uint32_t hit = upload_hit(v->event_x, v->event_y) & p->actions;
        if (synthetic || v->event != h->window || v->detail != 1 || (v->state & 0xff)) { p->pressed = 0; break; }
        if (type == XCB_BUTTON_PRESS) p->pressed = hit;
        else {
            if (hit && hit == p->pressed) { *kind = 3; *action = hit; }
            p->pressed = 0;
        }
        break;
    }
    default: break;
    }
    free(e); return 1;
}
