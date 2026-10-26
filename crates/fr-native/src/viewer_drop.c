/* Included by viewer_window.c. Copy-only XDND on the original local drawable.
 * One bounded URI list, no file reads, no INCR, no move/delete operation.
 * Rust validates local URI paths and the original intent lifetime before
 * acknowledging admission; this is not an acknowledgement of remote delivery. */
enum { FR_DROP_BYTES = 32768, FR_DROP_CLOCK = 10, FR_DROP_WAIT = 11, FR_DROP_DATA = 12 };
struct fr_viewer_drop {
    xcb_atom_t aware, enter, position, status, leave, drop, finished;
    xcb_atom_t selection, uri, types, copy, property, clock;
    xcb_window_t source;
    uint32_t sequence, time, age, length;
    uint8_t enabled, offered, accepted, stage;
    uint8_t bytes[FR_DROP_BYTES];
};
static int drop_message(struct fr_viewer_window *h, xcb_atom_t type,
        uint32_t flags, uint32_t third, uint32_t fourth, uint32_t fifth) {
    struct fr_viewer_drop *d = h->files;
    if (!d || !d->source) return 0;
    xcb_client_message_event_t e = {0};
    e.response_type = XCB_CLIENT_MESSAGE; e.format = 32;
    e.window = d->source; e.type = type;
    e.data.data32[0] = h->window; e.data.data32[1] = flags;
    e.data.data32[2] = third; e.data.data32[3] = fourth; e.data.data32[4] = fifth;
    return checked(h, xcb_send_event_checked(h->c, 0, d->source, 0, (const char *)&e));
}
void fr_viewer_window_drop_cancel(struct fr_viewer_window *h) {
    if (!h || !h->files) return;
    struct fr_viewer_drop *d = h->files;
    if (d->stage) (void)drop_message(h, d->finished, 0, XCB_NONE, 0, 0);
    d->source = d->sequence = d->time = d->length = 0;
    d->offered = d->accepted = d->stage = 0; d->age = UINT32_MAX;
}
static int drop_init(struct fr_viewer_window *h) {
    struct fr_viewer_drop *d = calloc(1, sizeof(*d));
    if (!d) return 0;
    h->files = d;
    d->aware = atom(h, "XdndAware"); d->enter = atom(h, "XdndEnter");
    d->position = atom(h, "XdndPosition"); d->status = atom(h, "XdndStatus");
    d->leave = atom(h, "XdndLeave"); d->drop = atom(h, "XdndDrop");
    d->finished = atom(h, "XdndFinished"); d->selection = atom(h, "XdndSelection");
    d->uri = atom(h, "text/uri-list"); d->types = atom(h, "XdndTypeList");
    d->copy = atom(h, "XdndActionCopy"); d->property = atom(h, "_FR_FILE_DROP");
    d->clock = atom(h, "_FR_FILE_DROP_CLOCK");
    if (!d->aware || !d->enter || !d->position || !d->status || !d->leave ||
        !d->drop || !d->finished || !d->selection || !d->uri || !d->types ||
        !d->copy || !d->property || !d->clock) {
        free(d); h->files = NULL; return 0;
    }
    uint32_t version = 5;
    if (!property(h, d->aware, XCB_ATOM_ATOM, 32, 1, &version)) {
        free(d); h->files = NULL; return 0;
    }
    return 1;
}
int fr_viewer_window_drop_offer(struct fr_viewer_window *h, int enabled) {
    if (!h || xcb_connection_has_error(h->c)) return -1;
    if (!h->files && (!enabled || !drop_init(h))) return enabled ? 0 : 1;
    struct fr_viewer_drop *d = h->files;
    if (!enabled) fr_viewer_window_drop_cancel(h);
    d->enabled = !!enabled;
    /* Keep the independently owned control-request event subscription intact. */
    uint32_t mask = XCB_EVENT_MASK_STRUCTURE_NOTIFY | XCB_EVENT_MASK_FOCUS_CHANGE |
        XCB_EVENT_MASK_PROPERTY_CHANGE;
    if (h->control_key) mask |= XCB_EVENT_MASK_KEY_PRESS | XCB_EVENT_MASK_KEY_RELEASE;
    return checked(h, xcb_change_window_attributes_checked(h->c, h->window,
        XCB_CW_EVENT_MASK, &mask));
}
uint32_t fr_viewer_window_drop_age(struct fr_viewer_window *h) {
    return h && h->files ? h->files->age : UINT32_MAX;
}
uint32_t fr_viewer_window_drop_read(struct fr_viewer_window *h, uint8_t *out, uint32_t capacity) {
    if (!h || !h->files || !out || h->files->stage != 3 ||
        h->files->length > capacity) return 0;
    memcpy(out, h->files->bytes, h->files->length);
    return h->files->length;
}
void fr_viewer_window_drop_complete(struct fr_viewer_window *h, int accepted) {
    if (!h || !h->files || h->files->stage != 3) return;
    struct fr_viewer_drop *d = h->files;
    (void)drop_message(h, d->finished, !!accepted, accepted ? d->copy : XCB_NONE, 0, 0);
    d->stage = 0;
    fr_viewer_window_drop_cancel(h);
}
static int drop_owner(struct fr_viewer_window *h) {
    xcb_generic_error_t *e = NULL;
    xcb_get_selection_owner_reply_t *r = xcb_get_selection_owner_reply(h->c,
        xcb_get_selection_owner(h->c, h->files->selection), &e);
    int same = r && !e && r->owner == h->files->source;
    free(r); free(e); return same;
}
static int drop_has_type(struct fr_viewer_window *h, const xcb_client_message_event_t *e) {
    struct fr_viewer_drop *d = h->files;
    if (!(e->data.data32[1] & 1u)) {
        for (unsigned i = 2; i < 5; ++i) if (e->data.data32[i] == d->uri) return 1;
        return 0;
    }
    xcb_generic_error_t *error = NULL;
    xcb_get_property_reply_t *r = xcb_get_property_reply(h->c,
        xcb_get_property(h->c, 0, d->source, d->types, XCB_ATOM_ATOM, 0, 64), &error);
    int found = 0;
    if (r && !error && r->type == XCB_ATOM_ATOM && r->format == 32 &&
        !r->bytes_after && xcb_get_property_value_length(r) <= 256) {
        const xcb_atom_t *types = xcb_get_property_value(r);
        for (int i = 0; i < xcb_get_property_value_length(r) / 4; ++i)
            if (types[i] == d->uri) found = 1;
    }
    free(r); free(error); return found;
}
static int drop_event(struct fr_viewer_window *h, const xcb_generic_event_t *event, uint32_t *kind) {
    struct fr_viewer_drop *d = h->files;
    if (!d) return 0;
    uint8_t type = event->response_type & 0x7f;
    if (type == XCB_CLIENT_MESSAGE) {
        const xcb_client_message_event_t *e = (const xcb_client_message_event_t *)event;
        if (e->window != h->window || e->format != 32) return 0;
        if (e->type == d->enter) {
            if (d->stage) return 1;
            fr_viewer_window_drop_cancel(h);
            unsigned version = e->data.data32[1] >> 24;
            if (!d->enabled || version < 3 || version > 5 || !e->data.data32[0]) return 1;
            d->source = e->data.data32[0]; d->offered = (uint8_t)drop_has_type(h, e);
            return 1;
        }
        if (e->type != d->position && e->type != d->leave && e->type != d->drop) return 0;
        if (!d->source || e->data.data32[0] != d->source) return 1;
        if (e->type == d->leave) { fr_viewer_window_drop_cancel(h); return 1; }
        if (e->type == d->position) {
            d->accepted = d->enabled && d->offered && !d->stage;
            /* Always request position updates and accept COPY only. */
            (void)drop_message(h, d->status, d->accepted ? 3u : 2u, 0, 0,
                d->accepted ? d->copy : XCB_NONE);
            return 1;
        }
        if (d->stage) return 1;
        d->stage = 1;
        if (!d->enabled || !d->accepted || !e->data.data32[2] || !drop_owner(h)) {
            fr_viewer_window_drop_cancel(h); return 1;
        }
        d->time = e->data.data32[2]; d->age = UINT32_MAX;
        xcb_void_cookie_t clock = xcb_change_property_checked(h->c, XCB_PROP_MODE_REPLACE,
            h->window, d->clock, XCB_ATOM_CARDINAL, 32, 1, &d->time);
        d->sequence = clock.sequence;
        if (!clock.sequence || xcb_flush(h->c) <= 0) fr_viewer_window_drop_cancel(h);
        else *kind = FR_DROP_CLOCK;
        return 1;
    }
    if (type == XCB_PROPERTY_NOTIFY) {
        const xcb_property_notify_event_t *e = (const xcb_property_notify_event_t *)event;
        if (e->window != h->window || e->atom != d->clock) return 0;
        if ((event->response_type & 0x80) || d->stage != 1 ||
            !d->sequence || event->full_sequence != d->sequence ||
            e->state != XCB_PROPERTY_NEW_VALUE) return 1;
        d->sequence = 0;
        uint32_t age = e->time - d->time;
        if (age >= 1000 || !d->enabled || !drop_owner(h)) {
            fr_viewer_window_drop_cancel(h); return 1;
        }
        d->age = age + 1; d->stage = 2;
        if (!checked(h, xcb_convert_selection_checked(h->c, h->window,
            d->selection, d->uri, d->property, d->time))) {
            fr_viewer_window_drop_cancel(h); return 1;
        }
        *kind = FR_DROP_WAIT; return 1;
    }
    if (type == XCB_SELECTION_NOTIFY) {
        const xcb_selection_notify_event_t *e = (const xcb_selection_notify_event_t *)event;
        if (e->requestor != h->window || e->selection != d->selection) return 0;
        if (d->stage != 2 || e->target != d->uri || e->time != d->time) return 1;
        if (!d->enabled || e->property != d->property || !drop_owner(h)) {
            fr_viewer_window_drop_cancel(h); return 1;
        }
        xcb_generic_error_t *error = NULL;
        xcb_get_property_reply_t *r = xcb_get_property_reply(h->c,
            xcb_get_property(h->c, 1, h->window, d->property, XCB_GET_PROPERTY_TYPE_ANY,
                0, FR_DROP_BYTES / 4), &error);
        int length = r ? xcb_get_property_value_length(r) : 0;
        if (r && !error && r->type == d->uri && r->format == 8 &&
            !r->bytes_after && length > 0 && length <= FR_DROP_BYTES) {
            memcpy(d->bytes, xcb_get_property_value(r), (size_t)length);
            d->length = (uint32_t)length; d->stage = 3; *kind = FR_DROP_DATA;
        } else {
            /* INCR and oversize property bodies never enter a second buffer. */
            (void)checked(h, xcb_delete_property_checked(h->c, h->window, d->property));
            fr_viewer_window_drop_cancel(h);
        }
        free(r); free(error); return 1;
    }
    return 0;
}
