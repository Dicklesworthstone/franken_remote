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
    uint32_t parent, parent_width, parent_height, direction, root;
    int mapped;
    /* Clipboard readiness needs server-authored unobscured evidence, not just
     * a successful map request. 0 is unknown, 1 unobscured, 2 obscured. */
    int visibility;
};
struct fr_upload_rect { int32_t x, y, width, height; };
/* Use the actual local X hierarchy, including a WM's reparenting frame. A
 * bounded walk cannot mistake a peer-supplied coordinate for the viewer. */
static int upload_rectangle(struct fr_upload *p, uint32_t window,
        uint32_t width, uint32_t height, struct fr_upload_rect *out) {
    xcb_connection_t *c = p->base->c;
    xcb_generic_error_t *e = NULL;
    xcb_get_geometry_reply_t *g = xcb_get_geometry_reply(c, xcb_get_geometry(c, window), &e);
    int valid = g && !e && g->root == p->root && g->width == width && g->height == height;
    free(g); free(e); e = NULL;
    if (!valid) return 0;
    xcb_get_window_attributes_reply_t *a = xcb_get_window_attributes_reply(c,
        xcb_get_window_attributes(c, window), &e);
    valid = a && !e && a->map_state == XCB_MAP_STATE_VIEWABLE;
    free(a); free(e); e = NULL;
    if (!valid) return 0;
    uint32_t outer = window;
    int rooted = 0;
    for (unsigned depth = 0; depth < 8; ++depth) {
        xcb_query_tree_reply_t *q = xcb_query_tree_reply(c, xcb_query_tree(c, outer), &e);
        valid = q && !e && q->root == p->root && q->parent && q->parent != outer;
        uint32_t parent = valid ? q->parent : 0;
        free(q); free(e); e = NULL;
        if (!valid) return 0;
        if (parent == p->root) { rooted = 1; break; }
        outer = parent;
    }
    if (!rooted) return 0;
    g = xcb_get_geometry_reply(c, xcb_get_geometry(c, outer), &e);
    valid = g && !e && g->root == p->root;
    if (valid) {
        out->width = (int32_t)g->width + 2 * (int32_t)g->border_width;
        out->height = (int32_t)g->height + 2 * (int32_t)g->border_width;
    }
    int32_t border = valid ? g->border_width : 0;
    free(g); free(e); e = NULL;
    if (!valid) return 0;
    xcb_translate_coordinates_reply_t *t = xcb_translate_coordinates_reply(c,
        xcb_translate_coordinates(c, outer, p->root, 0, 0), &e);
    valid = t && !e && t->same_screen;
    if (valid) { out->x = (int32_t)t->dst_x - border; out->y = (int32_t)t->dst_y - border; }
    free(t); free(e);
    return valid && !xcb_connection_has_error(c);
}
static int upload_separate(const struct fr_upload_rect *a, const struct fr_upload_rect *b) {
    return a->x + a->width <= b->x || b->x + b->width <= a->x ||
        a->y + a->height <= b->y || b->y + b->height <= a->y;
}
static int upload_on_screen(const struct fr_upload_rect *r, int32_t width, int32_t height) {
    return r->x >= 0 && r->y >= 0 && r->x <= INT16_MAX && r->y <= INT16_MAX &&
        r->x + r->width <= width && r->y + r->height <= height;
}
static int32_t upload_clamp(int32_t n, int32_t max) { return n < 0 ? 0 : n > max ? max : n; }
static int upload_position(struct fr_upload *p, int32_t width, int32_t height,
        int16_t *x, int16_t *y) {
    struct fr_upload_rect parent;
    if (!upload_rectangle(p, p->parent, p->parent_width, p->parent_height, &parent)) return 0;
    int32_t panel_width = p->base->width, panel_height = p->base->height;
    int32_t px = upload_clamp(parent.x, width - panel_width);
    int32_t py = upload_clamp(parent.y, height - panel_height);
    for (int32_t gap = 24; gap >= 0; gap -= 24) {
        struct fr_upload_rect choices[] = {
            { parent.x + parent.width + gap, py, panel_width, panel_height },
            { parent.x - panel_width - gap, py, panel_width, panel_height },
            { px, parent.y + parent.height + gap, panel_width, panel_height },
            { px, parent.y - panel_height - gap, panel_width, panel_height },
        };
        for (unsigned i = 0; i < sizeof(choices) / sizeof(choices[0]); ++i) {
            if (upload_on_screen(&choices[i], width, height) && upload_separate(&choices[i], &parent)) {
                *x = (int16_t)choices[i].x; *y = (int16_t)choices[i].y; return 1;
            }
        }
    }
    return 0;
}
/* Recheck mapped geometry before accepting local commands and on each refresh.
 * A WM may ignore position hints or move either ancestor later. Hide the
 * optional panel; never move/resize the viewer or relax its visibility fence. */
static int upload_layout_valid(struct fr_upload *p) {
    struct fr_upload_rect parent, panel;
    if (!upload_rectangle(p, p->parent, p->parent_width, p->parent_height, &parent) ||
        !upload_rectangle(p, p->base->window, p->base->width, p->base->height, &panel) ||
        !upload_separate(&parent, &panel)) return 0;
    xcb_generic_error_t *e = NULL;
    xcb_get_geometry_reply_t *g = xcb_get_geometry_reply(p->base->c,
        xcb_get_geometry(p->base->c, p->root), &e);
    int valid = g && !e && g->root == p->root &&
        upload_on_screen(&parent, g->width, g->height) &&
        upload_on_screen(&panel, g->width, g->height);
    free(g); free(e);
    return valid && !xcb_connection_has_error(p->base->c);
}
int fr_upload_validate(struct fr_upload *p) {
    if (!p || xcb_connection_has_error(p->base->c)) return -1;
    if (p->direction == 3 && p->visibility == 2) return 0;
    return !p->mapped || upload_layout_valid(p);
}
int fr_upload_mapped(struct fr_upload *p) {
    return p && p->mapped && (p->direction != 3 || p->visibility == 1) &&
        !xcb_connection_has_error(p->base->c);
}
void fr_upload_close(struct fr_upload *p) {
    if (!p) return;
    fr_viewer_window_close(p->base);
    free(p);
}
void fr_upload_disarm(struct fr_upload *p) { if (p) p->pressed = 0; }
struct fr_upload *fr_upload_open(const char *display, uint32_t parent,
        uint32_t width, uint32_t height, uint32_t direction) {
    if (!display || !parent || !width || !height || direction > 3) return NULL;
    struct fr_upload *p = calloc(1, sizeof(*p));
    if (!p) return NULL;
    p->base = calloc(1, sizeof(*p->base));
    if (!p->base) goto fail;
    struct fr_viewer_window *h = p->base;
    h->width = direction == 3 ? FR_CLIPBOARD_WIDTH : UP_WIDTH;
    h->height = direction == 3 ? FR_CLIPBOARD_HEIGHT : UP_HEIGHT;
    int screen = 0;
    h->c = xcb_connect(display, &screen);
    if (!h->c || xcb_connection_has_error(h->c)) goto fail;
    xcb_screen_iterator_t it = xcb_setup_roots_iterator(xcb_get_setup(h->c));
    while (screen-- > 0 && it.rem) xcb_screen_next(&it);
    if (!it.rem || it.data->root_depth != 24 ||
        it.data->width_in_pixels < h->width || it.data->height_in_pixels < h->height) goto fail;
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
    p->direction = direction; p->root = it.data->root;
    int16_t x, y;
    if (!upload_position(p, it.data->width_in_pixels, it.data->height_in_pixels, &x, &y)) goto fail;
    uint32_t parent_events = XCB_EVENT_MASK_STRUCTURE_NOTIFY;
    if (!checked(h, xcb_change_window_attributes_checked(h->c, parent,
        XCB_CW_EVENT_MASK, &parent_events))) goto fail;
    h->window = xcb_generate_id(h->c);
    uint32_t values[] = { 0xffffff, XCB_EVENT_MASK_STRUCTURE_NOTIFY | XCB_EVENT_MASK_EXPOSURE |
        XCB_EVENT_MASK_BUTTON_PRESS | XCB_EVENT_MASK_BUTTON_RELEASE | XCB_EVENT_MASK_LEAVE_WINDOW |
        XCB_EVENT_MASK_FOCUS_CHANGE | (direction == 3 ? XCB_EVENT_MASK_VISIBILITY_CHANGE : 0) };
    if (!checked(h, xcb_create_window_checked(h->c, XCB_COPY_FROM_PARENT,
        h->window, it.data->root, x, y, h->width, h->height, 0,
        XCB_WINDOW_CLASS_INPUT_OUTPUT, it.data->root_visual,
        XCB_CW_BACK_PIXEL | XCB_CW_EVENT_MASK, values))) goto fail;
    h->protocols = atom(h, "WM_PROTOCOLS"); h->close = atom(h, "WM_DELETE_WINDOW");
    xcb_atom_t utf8 = atom(h, "UTF8_STRING"), utility = atom(h, "_NET_WM_WINDOW_TYPE_UTILITY");
    /* Set InputHint=false BEFORE map. No WM_TAKE_FOCUS protocol, SetInputFocus,
     * grabs, raise, or root/global input subscription exists on this boundary. */
    uint32_t hints[9] = {1, 0, 0, 0, 0, 0, 0, 0, 0};
    uint32_t size[18] = {0};
    size[0] = (1u << 2) | (1u << 4) | (1u << 5); /* PPosition | PMinSize | PMaxSize */
    size[1] = (uint32_t)x; size[2] = (uint32_t)y;
    size[5] = size[7] = h->width; size[6] = size[8] = h->height;
    const char *title = direction == 3 ? "FrankenRemote - shared clipboard" :
        direction == 2 ? "FrankenRemote - remote audio" :
        direction == 1 ? "FrankenRemote - download progress" : "FrankenRemote - upload progress";
    const char upload_class[] = "franken-remote-upload\0FrankenRemote\0";
    const char download_class[] = "franken-remote-download\0FrankenRemote\0";
    const char audio_class[] = "franken-remote-audio\0FrankenRemote\0";
    const char clipboard_class[] = "franken-remote-clipboard\0FrankenRemote\0";
    const char *class = direction == 3 ? clipboard_class :
        direction == 2 ? audio_class : direction == 1 ? download_class : upload_class;
    size_t class_len = direction == 3 ? sizeof(clipboard_class)-1 : direction == 2 ? sizeof(audio_class)-1 :
        direction == 1 ? sizeof(download_class)-1 : sizeof(upload_class)-1;
    xcb_font_t font = xcb_generate_id(h->c);
    if (!h->close || !utf8 || !utility ||
        !property(h, h->protocols, XCB_ATOM_ATOM, 32, 1, &h->close) ||
        !property(h, XCB_ATOM_WM_HINTS, XCB_ATOM_WM_HINTS, 32, 9, hints) ||
        !property(h, XCB_ATOM_WM_TRANSIENT_FOR, XCB_ATOM_WINDOW, 32, 1, &parent) ||
        !property(h, atom(h, "_NET_WM_WINDOW_TYPE"), XCB_ATOM_ATOM, 32, 1, &utility) ||
        !property(h, XCB_ATOM_WM_NAME, XCB_ATOM_STRING, 8, strlen(title), title) ||
        !property(h, atom(h, "_NET_WM_NAME"), utf8, 8, strlen(title), title) ||
        !property(h, XCB_ATOM_WM_CLASS, XCB_ATOM_STRING, 8, class_len, class) ||
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
    if (!p || !f || f->progress < -1 || f->progress > 1000 ||
        (f->actions & ~(p->direction == 3 ? 64u : p->direction == 2 ? 56u : p->direction == 1 ? 4u : 7u))) return 0;
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
    if (p->direction == 3) {
        /* A compact local strip, never an overlay on the remote drawable.
         * Keep the actual state and off gesture visible; no transfer progress,
         * private text or unrelated actions are rendered in this surface. */
        const char heading[] = "Shared clipboard - this connection";
        const char footer[] = "Closing or hiding this panel also turns text sharing off.";
        return upload_text(p, 16, 23, heading, sizeof(heading)-1) &&
            upload_text(p, 16, 45, (const char *)f->lines[0], sizes[0]) &&
            upload_box(p, 456, 8, 168, 32, 0) &&
            upload_text(p, 468, 29, "Turn off", 8) &&
            ((p->actions & 64u) || upload_text(p, 570, 29, "(off)", 5)) &&
            upload_text(p, 16, 67, (const char *)f->lines[3], sizes[3]) &&
            upload_text(p, 16, 87, footer, sizeof(footer)-1);
    }
    const char *heading = p->direction == 3 ? "Shared clipboard - this connection only" :
        p->direction == 2 ? "Remote audio - local volume controls" :
        p->direction == 1 ? "Download progress - original destination only" :
        "Upload progress - original selection only";
    if (!upload_text(p, 16, 23, heading, strlen(heading))) return 0;
    for (unsigned row = 0; row < UP_ROWS; ++row) {
        if (!upload_text(p, 16, (int16_t)(46 + row * 20), (const char *)f->lines[row], sizes[row])) return 0;
    }
    if (!upload_box(p, 16, 200, 606, 16, 0)) return 0;
    if (f->progress > 0 && !upload_box(p, 18, 202, (uint16_t)(602 * (uint32_t)f->progress / 1000), 12, 1)) return 0;
    const char *labels[] = { p->direction == 2 ? "Quieter" : "Pause",
        p->direction == 2 ? "Louder" : "Resume",
        p->direction == 3 ? "Turn off" : p->direction == 2 ? "Mute / unmute" :
        p->direction == 1 ? "Cancel download" : "Cancel upload" };
    for (unsigned i = p->direction == 1 || p->direction == 3 ? 2u : 0u; i < 3; ++i) {
        int16_t x = (int16_t)(16 + i * 204);
        if (!upload_box(p, x, 234, 188, 32, 0) ||
            !upload_text(p, (int16_t)(x + 10), 254, labels[i], strlen(labels[i]))) return 0;
        uint32_t action = p->direction == 3 ? 64u : (1u << (i + (p->direction == 2 ? 3u : 0u)));
        if (!(p->actions & action) &&
            !upload_text(p, (int16_t)(x + 110), 254, "(off)", 5)) return 0;
    }
    const char *footer = p->direction == 3 ? "Closing this panel also turns text sharing off." :
        p->direction == 2 ? "No microphone, host-volume or system-volume changes." :
        "Buttons request changes. Closing this panel only hides progress.";
    return upload_text(p, 16, 290, footer, strlen(footer));
}
static uint32_t upload_hit(const struct fr_upload *p, int16_t x, int16_t y) {
    if (p->direction == 3)
        return x >= 456 && x < 624 && y >= 8 && y < 40 ? 4u : 0u;
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
        if (!synthetic && ((xcb_map_notify_event_t *)e)->window == h->window) {
            p->mapped = 1;
            *kind = upload_layout_valid(p) ? 1 : 2;
        }
        break;
    case XCB_VISIBILITY_NOTIFY: {
        xcb_visibility_notify_event_t *v = (xcb_visibility_notify_event_t *)e;
        if (p->direction == 3 && !synthetic && v->window == h->window) {
            p->visibility = v->state == XCB_VISIBILITY_UNOBSCURED ? 1 : 2;
            *kind = p->visibility == 1 ? 1 : 2;
        }
        break;
    }
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
        if ((v->window == h->window && (v->width != h->width || v->height != h->height)) ||
            (v->window == p->parent && (v->width != p->parent_width || v->height != p->parent_height))) *kind = 2;
        else if (p->mapped && (v->window == h->window || v->window == p->parent) &&
            !upload_layout_valid(p)) *kind = 2;
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
        uint32_t hit = upload_hit(p, v->event_x, v->event_y);
        hit = (p->direction == 3 ? (hit == 4u ? 64u : 0u) :
            hit << (p->direction == 2 ? 3u : 0u)) & p->actions;
        if (synthetic || v->event != h->window || v->detail != 1 || (v->state & 0xff)) { p->pressed = 0; break; }
        if (!p->mapped || (p->direction == 3 && p->visibility != 1) ||
            !upload_layout_valid(p)) { p->pressed = 0; *kind = 2; break; }
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
