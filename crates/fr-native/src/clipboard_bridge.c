/* XCB representation boundary only. Rust owns text, generations, reader limits,
 * expiry and selection state. No callbacks, retained caller pointers or Xlib
 * global error handlers. Every checked request has a consumed result. */
#include <xcb/xcb.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

struct fr_clip_atoms { uint32_t clipboard, utf8, targets, timestamp, incr, clock; };
struct fr_clip_event { uint32_t kind, window, property, target, time, selection, sequence; };
_Static_assert(sizeof(xcb_selection_notify_event_t) <= 32, "XCB event ABI");
struct fr_clip { xcb_connection_t *c; xcb_window_t window; struct fr_clip_atoms atoms; };

static int checked(struct fr_clip *h, xcb_void_cookie_t cookie) {
    xcb_generic_error_t *e = xcb_request_check(h->c, cookie);
    int result = e ? -2 : (xcb_connection_has_error(h->c) ? -1 : 0);
    free(e);
    return result;
}
static uint32_t atom(xcb_connection_t *c, const char *name) {
    xcb_generic_error_t *error = NULL;
    xcb_intern_atom_reply_t *r = xcb_intern_atom_reply(c,
        xcb_intern_atom(c, 0, (uint16_t)strlen(name), name), &error);
    uint32_t result = r && !error ? r->atom : 0;
    free(r); free(error); return result;
}
void fr_clip_close(struct fr_clip *h) {
    if (!h) return;
    /* Disconnect destroys only our windows; never SetSelectionOwner(None). */
    if (h->c) xcb_disconnect(h->c);
    free(h);
}
struct fr_clip *fr_clip_open(const char *display, struct fr_clip_atoms *atoms,
                             uint32_t *window) {
    struct fr_clip *h = calloc(1, sizeof(*h));
    if (!h) return NULL;
    int screen = 0;
    h->c = xcb_connect(display, &screen);
    if (!h->c || xcb_connection_has_error(h->c)) goto fail;
    xcb_screen_iterator_t it = xcb_setup_roots_iterator(xcb_get_setup(h->c));
    while (screen-- > 0 && it.rem) xcb_screen_next(&it);
    if (!it.rem) goto fail;
    h->window = xcb_generate_id(h->c);
    uint32_t events = XCB_EVENT_MASK_PROPERTY_CHANGE | XCB_EVENT_MASK_STRUCTURE_NOTIFY;
    if (checked(h, xcb_create_window_checked(h->c, XCB_COPY_FROM_PARENT,
        h->window, it.data->root, 0, 0, 1, 1, 0, XCB_WINDOW_CLASS_INPUT_OUTPUT,
        it.data->root_visual, XCB_CW_EVENT_MASK, &events))) goto fail;
    h->atoms.clipboard = atom(h->c, "CLIPBOARD");
    h->atoms.utf8 = atom(h->c, "UTF8_STRING");
    h->atoms.targets = atom(h->c, "TARGETS");
    h->atoms.timestamp = atom(h->c, "TIMESTAMP");
    h->atoms.incr = atom(h->c, "INCR");
    h->atoms.clock = atom(h->c, "_FR_CLIPBOARD_CLOCK");
    if (!h->atoms.clipboard || !h->atoms.utf8 || !h->atoms.targets ||
        !h->atoms.timestamp || !h->atoms.incr || !h->atoms.clock) goto fail;
    *atoms = h->atoms; *window = h->window; return h;
fail:
    fr_clip_close(h); return NULL;
}
int fr_clip_tick(struct fr_clip *h, uint32_t *sequence) {
    const uint8_t value = 0;
    xcb_void_cookie_t cookie = xcb_change_property_checked(h->c, XCB_PROP_MODE_REPLACE,
        h->window, h->atoms.clock, XCB_ATOM_INTEGER, 8, 1, &value);
    *sequence = cookie.sequence;
    return checked(h, cookie);
}
int fr_clip_owner(struct fr_clip *h, uint32_t *owner) {
    xcb_generic_error_t *e = NULL;
    xcb_get_selection_owner_reply_t *r = xcb_get_selection_owner_reply(h->c,
        xcb_get_selection_owner(h->c, h->atoms.clipboard), &e);
    int rc = r && !e ? 0 : -1;
    if (!rc) *owner = r->owner;
    free(e); free(r); return rc;
}
int fr_clip_publish(struct fr_clip *h, uint32_t time) {
    if (!time) return -2;
    return checked(h, xcb_set_selection_owner_checked(h->c, h->window,
        h->atoms.clipboard, time));
}
int fr_clip_watch(struct fr_clip *h, uint32_t window, int enabled) {
    uint32_t events = enabled ?
        XCB_EVENT_MASK_PROPERTY_CHANGE | XCB_EVENT_MASK_STRUCTURE_NOTIFY : 0;
    if (window == h->window) return -2;
    return checked(h, xcb_change_window_attributes_checked(h->c, window,
        XCB_CW_EVENT_MASK, &events));
}
int fr_clip_property(struct fr_clip *h, uint32_t window, uint32_t property,
                     uint32_t type, uint8_t format, uint32_t count, const void *data) {
    if (!window || !property || !type || (format != 8 && format != 32) ||
        count > 16384u / (format / 8) || (count && !data)) return -2;
    return checked(h, xcb_change_property_checked(h->c, XCB_PROP_MODE_REPLACE,
        window, property, type, format, count, data));
}
int fr_clip_notify(struct fr_clip *h, const struct fr_clip_event *request,
                   uint32_t property) {
    xcb_selection_notify_event_t e;
    memset(&e, 0, sizeof(e));
    e.response_type = XCB_SELECTION_NOTIFY;
    e.requestor = request->window; e.selection = request->selection;
    e.target = request->target; e.property = property; e.time = request->time;
    char wire_event[32] = {0};
    memcpy(wire_event, &e, sizeof(e));
    return checked(h, xcb_send_event_checked(h->c, 0, request->window,
        XCB_EVENT_MASK_NO_EVENT, wire_event));
}
int fr_clip_next(struct fr_clip *h, struct fr_clip_event *out) {
    memset(out, 0, sizeof(*out));
    xcb_generic_event_t *event = xcb_poll_for_event(h->c);
    if (!event) return xcb_connection_has_error(h->c) ? -1 : 0;
    out->sequence = event->full_sequence;
    /* Only server-generated notifications drive ownership or INCR progress. */
    if (!(event->response_type & 128)) switch (event->response_type) {
    case XCB_SELECTION_REQUEST: {
        const xcb_selection_request_event_t *e = (const void *)event;
        if (e->owner != h->window) break;
        out->kind = 1; out->window = e->requestor; out->property = e->property;
        out->target = e->target; out->time = e->time; out->selection = e->selection;
        break;
    }
    case XCB_SELECTION_CLEAR: {
        const xcb_selection_clear_event_t *e = (const void *)event;
        if (e->owner == h->window && e->selection == h->atoms.clipboard) {
            out->kind = 2; out->time = e->time;
        }
        break;
    }
    case XCB_PROPERTY_NOTIFY: {
        const xcb_property_notify_event_t *e = (const void *)event;
        out->window = e->window; out->property = e->atom; out->time = e->time;
        if (e->state == XCB_PROPERTY_DELETE) out->kind = 3;
        else if (e->window == h->window && e->atom == h->atoms.clock) out->kind = 4;
        break;
    }
    case XCB_DESTROY_NOTIFY: {
        const xcb_destroy_notify_event_t *e = (const void *)event;
        out->kind = 5; out->window = e->window; break;
    }
    default: break;
    }
    free(event); return 1;
}
