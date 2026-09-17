/* Local viewer input boundary. Owns ONE XCB connection and an InputOnly clock
 * window, never the renderer's window. No grabs, injection, callbacks or Rust
 * pointers. XKB requests use the system protocol structs, not a second library.
 * Input is logical X-server input; SendEvent cannot create positive evidence.
 * XTest and the selected user's X server remain explicit trust limits. */
#include <xcb/xcb.h>
#include <xcb/xcbext.h>
#include <X11/extensions/XKBproto.h>
#include <X11/extensions/XI2proto.h>
#include <X11/extensions/XI2.h>
#include <sys/uio.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

/* Fixed output only; no native allocation or names escape this boundary. */
struct fr_viewer_held { uint8_t keys[32], buttons[32]; };
struct fr_viewer_input {
    xcb_connection_t *c;
    xcb_window_t window, clock;
    xcb_atom_t barrier;
    uint8_t xkb_event;
    uint32_t serial;
    uint8_t xi_opcode;
    uint16_t pointer;
    uint32_t keys_query, pointer_query;
    int held_pending;
    struct fr_viewer_held held;
};
/* Matches Rust Raw; every consumed event (even ignored) counts toward its bound. */
struct fr_viewer_event { uint32_t kind, time, detail; int32_t x, y; };
enum { IGNORE, KEY_DOWN, KEY_UP, BUTTON_DOWN, BUTTON_UP, MOTION, BARRIER,
       FOCUS_LOST, WINDOW_LOST, GEOMETRY, KEYMAP_CHANGED, SYNTHETIC, DEVICES_CHANGED };
static xcb_extension_t xkb = { "XKEYBOARD", 0 };
static xcb_extension_t xi = { "XInputExtension", 0 };
static uint32_t extension_request(struct fr_viewer_input *h, xcb_extension_t *ext,
                                  uint8_t opcode, int isvoid, void *body, size_t size) {
    struct iovec parts[4] = {{0}};
    parts[2].iov_base = body; parts[2].iov_len = size;
    parts[3].iov_base = NULL; parts[3].iov_len = (-size) & 3;
    xcb_protocol_request_t req = {2, ext, opcode, (uint8_t)isvoid};
    return xcb_send_request(h->c, XCB_REQUEST_CHECKED, parts+2, &req);
}
static uint32_t request(struct fr_viewer_input *h, uint8_t opcode, int isvoid,
                        void *body, size_t size) {
    return extension_request(h, &xkb, opcode, isvoid, body, size);
}
static uint32_t xi_request(struct fr_viewer_input *h, uint8_t opcode, int isvoid,
                           void *body, size_t size) {
    return extension_request(h, &xi, opcode, isvoid, body, size);
}
static void *reply(struct fr_viewer_input *h, uint32_t seq) {
    xcb_generic_error_t *error = NULL;
    void *r = xcb_wait_for_reply(h->c, seq, &error);
    if (error || xcb_connection_has_error(h->c)) { free(r); r = NULL; }
    free(error); return r;
}
static int checked(struct fr_viewer_input *h, xcb_void_cookie_t cookie) {
    xcb_generic_error_t *e = xcb_request_check(h->c, cookie);
    int ok = !e && !xcb_connection_has_error(h->c);
    free(e); return ok;
}
void fr_viewer_input_close(struct fr_viewer_input *h) {
    if (!h) return;
    if (h->c) xcb_disconnect(h->c); /* releases ONLY our resources and event mask */
    free(h);
}
static int keyboard(struct fr_viewer_input *h, uint8_t names[256][4]) {
    const xcb_query_extension_reply_t *ext = xcb_get_extension_data(h->c, &xkb);
    if (!ext || !ext->present) return 0;
    h->xkb_event = ext->first_event;
    xkbUseExtensionReq use = {0}; use.wantedMajor = 1; use.wantedMinor = 0;
    xkbUseExtensionReply *u = reply(h, request(h, X_kbUseExtension, 0, &use, sizeof(use)));
    int ok = u && u->supported && !u->length;
    free(u); if (!ok) return 0;
    xkbSelectEventsReq select = {0}; select.deviceSpec = XkbUseCoreKbd;
    select.affectWhich = select.selectAll = XkbNewKeyboardNotifyMask | XkbMapNotifyMask | XkbNamesNotifyMask;
    select.affectMap = select.map = XkbAllMapComponentsMask;
    xcb_void_cookie_t cookie = { request(h, X_kbSelectEvents, 1, &select, sizeof(select)) };
    if (!checked(h, cookie)) return 0;
    xkbPerClientFlagsReq flags = {0}; flags.deviceSpec = XkbUseCoreKbd;
    flags.change = flags.value = XkbPCF_DetectableAutoRepeatMask;
    xkbPerClientFlagsReply *f = reply(h, request(h, X_kbPerClientFlags, 0, &flags, sizeof(flags)));
    ok = f && !f->length && (f->supported & XkbPCF_DetectableAutoRepeatMask) &&
        (f->value & XkbPCF_DetectableAutoRepeatMask);
    free(f); if (!ok) return 0;
    xkbGetNamesReq get = {0}; get.deviceSpec = XkbUseCoreKbd; get.which = XkbKeyNamesMask;
    xkbGetNamesReply *n = reply(h, request(h, X_kbGetNames, 0, &get, sizeof(get)));
    ok = n && n->which == XkbKeyNamesMask && n->nKeys &&
        (unsigned)n->firstKey + n->nKeys <= 256 && n->length == n->nKeys;
    if (ok) memcpy(names[n->firstKey], n+1, (size_t)n->nKeys * 4);
    free(n); return ok;
}
/* The core pointer mask covers only buttons 1-5. XI2 queries the FULL logical
 * button mask (including Back/Forward). Core delivery can use multiple masters;
 * do not assume its ClientPointer identifies every event. This adapter admits
 * exactly one enabled master pair, verifies its actual ClientPointer, and ends
 * the capture generation on ANY hierarchy change. No hard-coded device ID. */
static int single_master_pair(struct fr_viewer_input *h, const xXIQueryDeviceReply *r) {
    if (!r || r->num_devices != 2 || r->length > 4096) return 0;
    const uint8_t *cursor = (const uint8_t *)(r + 1);
    const uint8_t *end = cursor + (size_t)r->length * 4;
    uint16_t pointer = 0, keyboard = 0, pointer_pair = 0, keyboard_pair = 0;
    for (unsigned i = 0; i < 2; i++) {
        if ((size_t)(end - cursor) < sizeof(xXIDeviceInfo)) return 0;
        xXIDeviceInfo d; memcpy(&d, cursor, sizeof(d)); cursor += sizeof(d);
        if (!d.enabled || d.deviceid <= 1 || d.num_classes > 64) return 0;
        if (d.use == XIMasterPointer && !pointer) {
            pointer = d.deviceid; pointer_pair = d.attachment;
        } else if (d.use == XIMasterKeyboard && !keyboard) {
            keyboard = d.deviceid; keyboard_pair = d.attachment;
        } else return 0;
        size_t name = ((size_t)d.name_len + 3) & ~(size_t)3;
        if (name > (size_t)(end - cursor)) return 0;
        cursor += name;
        for (unsigned j = 0; j < d.num_classes; j++) {
            if ((size_t)(end - cursor) < sizeof(xXIAnyInfo)) return 0;
            xXIAnyInfo c; memcpy(&c, cursor, sizeof(c));
            size_t size = (size_t)c.length * 4;
            if (size < sizeof(c) || size > (size_t)(end - cursor)) return 0;
            cursor += size;
        }
    }
    if (cursor != end || !pointer || !keyboard ||
        pointer_pair != keyboard || keyboard_pair != pointer) return 0;
    h->pointer = pointer; return 1;
}
static uint32_t pointer_query(struct fr_viewer_input *h) {
    xXIQueryPointerReq q = {0}; q.win = h->window; q.deviceid = h->pointer;
    return xi_request(h, X_XIQueryPointer, 0, &q, sizeof(q));
}
static int pointer_bits(const xXIQueryPointerReply *r, uint8_t bits[32]) {
    if (!r || !r->same_screen || r->buttons_len > 8 ||
        r->length != 6u + r->buttons_len) return 0;
    memset(bits, 0, 32);
    memcpy(bits, r + 1, (size_t)r->buttons_len * 4);
    return !(bits[0] & 1); /* XI button indices start at ONE, not zero. */
}
static int held_setup(struct fr_viewer_input *h) {
    const xcb_query_extension_reply_t *ext = xcb_get_extension_data(h->c, &xi);
    if (!ext || !ext->present) return 0;
    h->xi_opcode = ext->major_opcode;
    xXIQueryVersionReq v = {0}; v.major_version = 2;
    xXIQueryVersionReply *version = reply(h, xi_request(h, X_XIQueryVersion, 0, &v, sizeof(v)));
    int ok = version && !version->length && version->major_version >= 2;
    free(version); if (!ok) return 0;
    /* Hierarchy metadata only, on OUR InputOnly window; no root/raw-input mask. */
    struct { xXISelectEventsReq request; xXIEventMask event; uint32_t mask; } select = {0};
    select.request.win = h->clock; select.request.num_masks = 1;
    select.event.deviceid = XIAllDevices; select.event.mask_len = 1;
    select.mask = XI_HierarchyChangedMask;
    xcb_void_cookie_t cookie = { xi_request(h, X_XISelectEvents, 1, &select, sizeof(select)) };
    if (!checked(h, cookie)) return 0;
    xXIQueryDeviceReq q = {0}; q.deviceid = XIAllMasterDevices;
    xXIQueryDeviceReply *devices = reply(h, xi_request(h, X_XIQueryDevice, 0, &q, sizeof(q)));
    ok = single_master_pair(h, devices); free(devices); if (!ok) return 0;
    xXIGetClientPointerReq get = {0}; /* None means THIS requesting client. */
    xXIGetClientPointerReply *client = reply(h, xi_request(h, X_XIGetClientPointer, 0, &get, sizeof(get)));
    ok = client && !client->length && client->set && client->deviceid == h->pointer;
    free(client); if (!ok) return 0;
    xXIQueryPointerReply *p = reply(h, pointer_query(h));
    uint8_t bits[32]; ok = pointer_bits(p, bits); free(p);
    if (ok) for (unsigned i = 0; i < 32; i++) if (bits[i]) ok = 0;
    return ok; /* A preexisting Back/Forward press is not an adopted gesture. */
}
/* One fixed pair, asynchronous after setup. The owning Rust turn enforces its
 * original 50ms deadline and cancellation. No wait_for_reply in this path. */
int fr_viewer_input_held_begin(struct fr_viewer_input *h) {
    if (!h || h->held_pending || xcb_connection_has_error(h->c)) return 0;
    memset(&h->held, 0, sizeof(h->held));
    h->keys_query = xcb_query_keymap(h->c).sequence;
    h->pointer_query = pointer_query(h);
    h->held_pending = 1;
    return h->keys_query && h->pointer_query;
}
int fr_viewer_input_held_poll(struct fr_viewer_input *h, struct fr_viewer_held *out) {
    if (!h || !out || !h->held_pending || xcb_connection_has_error(h->c)) return -1;
    if (h->keys_query) {
        void *raw = NULL; xcb_generic_error_t *error = NULL;
        int ready = xcb_poll_for_reply(h->c, h->keys_query, &raw, &error);
        if (ready) {
            xcb_query_keymap_reply_t *r = raw;
            int ok = r && !error && r->length == 2;
            if (ok) memcpy(h->held.keys, r->keys, 32);
            free(raw); free(error); if (!ok) return -1;
            h->keys_query = 0;
        }
    }
    if (h->pointer_query) {
        void *raw = NULL; xcb_generic_error_t *error = NULL;
        int ready = xcb_poll_for_reply(h->c, h->pointer_query, &raw, &error);
        if (ready) {
            int ok = !error && pointer_bits(raw, h->held.buttons);
            free(raw); free(error); if (!ok) return -1;
            h->pointer_query = 0;
        }
    }
    if (xcb_connection_has_error(h->c)) return -1;
    if (h->keys_query || h->pointer_query) return 0;
    *out = h->held; h->held_pending = 0; return 1;
}

/* Setup only. Native event processing uses no synchronous X request/reply. */
struct fr_viewer_input *fr_viewer_input_open(const char *display, uint32_t window,
                                           uint32_t width, uint32_t height,
                                           uint8_t names[256][4]) {
    if (!display || !window || !names || !width || !height || width > 32767 || height > 32767) return NULL;
    struct fr_viewer_input *h = calloc(1, sizeof(*h));
    if (!h) return NULL;
    h->c = xcb_connect(display, NULL); h->window = window;
    if (!h->c || xcb_connection_has_error(h->c)) goto fail;
    /* Reject a root before subscribing to ANY input. Requery after subscription
     * so a resize during setup cannot establish a stale layout generation. */
    xcb_get_geometry_reply_t *initial = reply(h, xcb_get_geometry(h->c, window).sequence);
    int selected = initial && initial->root != window;
    free(initial); if (!selected) goto fail;
    const uint32_t mask = XCB_EVENT_MASK_KEY_PRESS | XCB_EVENT_MASK_KEY_RELEASE |
        XCB_EVENT_MASK_BUTTON_PRESS | XCB_EVENT_MASK_BUTTON_RELEASE |
        XCB_EVENT_MASK_POINTER_MOTION | XCB_EVENT_MASK_FOCUS_CHANGE |
        XCB_EVENT_MASK_STRUCTURE_NOTIFY | XCB_EVENT_MASK_VISIBILITY_CHANGE;
    if (!checked(h, xcb_change_window_attributes_checked(h->c, window, XCB_CW_EVENT_MASK, &mask))) goto fail;
    xcb_get_geometry_reply_t *g = reply(h, xcb_get_geometry(h->c, window).sequence);
    if (!g) goto fail;
    uint32_t root = g->root;
    int ok = window != root && g->width == width && g->height == height;
    free(g); if (!ok) goto fail;
    xcb_get_window_attributes_reply_t *a = reply(h, xcb_get_window_attributes(h->c, window).sequence);
    ok = a && a->map_state == XCB_MAP_STATE_VIEWABLE && a->_class == XCB_WINDOW_CLASS_INPUT_OUTPUT;
    free(a); if (!ok) goto fail;
    xcb_get_input_focus_reply_t *focus = reply(h, xcb_get_input_focus(h->c).sequence);
    ok = focus && focus->focus == window;
    free(focus); if (!ok) goto fail;
    xcb_query_keymap_reply_t *keys = reply(h, xcb_query_keymap(h->c).sequence);
    ok = keys != NULL;
    if (keys) for (int i=0;i<32;i++) if (keys->keys[i]) ok = 0;
    free(keys); if (!ok) goto fail;
    xcb_query_pointer_reply_t *p = reply(h, xcb_query_pointer(h->c, window).sequence);
    ok = p && !(p->mask & (XCB_BUTTON_MASK_1 | XCB_BUTTON_MASK_2 | XCB_BUTTON_MASK_3 |
                           XCB_BUTTON_MASK_4 | XCB_BUTTON_MASK_5));
    free(p); if (!ok || !keyboard(h, names)) goto fail;
    h->clock = xcb_generate_id(h->c);
    uint32_t clockmask = XCB_EVENT_MASK_PROPERTY_CHANGE;
    if (!checked(h, xcb_create_window_checked(h->c, 0, h->clock, root, 0, 0, 1, 1, 0,
        XCB_WINDOW_CLASS_INPUT_ONLY, 0, XCB_CW_EVENT_MASK, &clockmask))) goto fail;
    const char name[] = "_FR_VIEWER_INPUT_CLOCK";
    xcb_intern_atom_reply_t *atom = reply(h, xcb_intern_atom(h->c, 0, sizeof(name)-1, name).sequence);
    if (!atom) goto fail;
    h->barrier = atom->atom; free(atom);
    if (!h->barrier || !held_setup(h)) goto fail;
    return h;
fail:
    fr_viewer_input_close(h); return NULL;
}
int fr_viewer_input_barrier(struct fr_viewer_input *h) {
    if (!h || xcb_connection_has_error(h->c)) return 0;
    h->serial++;
    xcb_change_property(h->c, XCB_PROP_MODE_REPLACE, h->clock, h->barrier,
                        XCB_ATOM_CARDINAL, 32, 1, &h->serial);
    return xcb_flush(h->c) > 0;
}
int fr_viewer_input_next(struct fr_viewer_input *h, struct fr_viewer_event *out) {
    if (!h || !out || xcb_connection_has_error(h->c)) return -1;
    xcb_generic_event_t *e = xcb_poll_for_event(h->c);
    if (!e) return xcb_connection_has_error(h->c) ? -1 : 0;
    memset(out, 0, sizeof(*out));
    uint8_t kind = e->response_type & 0x7f;
    if (!kind) { free(e); return -1; }
    if (kind == h->xkb_event || kind == XCB_MAPPING_NOTIFY) out->kind = KEYMAP_CHANGED;
    else switch (kind) {
    case XCB_GE_GENERIC: {
        xcb_ge_generic_event_t *v = (void*)e;
        if (v->extension == h->xi_opcode && v->event_type == XI_HierarchyChanged)
            out->kind = DEVICES_CHANGED;
        break;
    }
    case XCB_PROPERTY_NOTIFY: {
        xcb_property_notify_event_t *v = (void*)e;
        if (!(e->response_type & 0x80) && v->window == h->clock && v->atom == h->barrier && v->state == XCB_PROPERTY_NEW_VALUE) {
            out->kind = BARRIER; out->time = v->time;
        } break;
    }
    case XCB_KEY_PRESS: case XCB_KEY_RELEASE:
    case XCB_BUTTON_PRESS: case XCB_BUTTON_RELEASE: case XCB_MOTION_NOTIFY: {
        xcb_key_press_event_t *v = (void*)e; /* same 32-byte common fields */
        if (v->event != h->window) break;
        if ((e->response_type & 0x80) || !v->same_screen) { out->kind = SYNTHETIC; break; }
        out->kind = kind == XCB_KEY_PRESS ? KEY_DOWN : kind == XCB_KEY_RELEASE ? KEY_UP :
            kind == XCB_BUTTON_PRESS ? BUTTON_DOWN : kind == XCB_BUTTON_RELEASE ? BUTTON_UP : MOTION;
        out->time = v->time; out->detail = v->detail;
        out->x = v->event_x; out->y = v->event_y; break;
    }
    case XCB_FOCUS_OUT:
        if (((xcb_focus_out_event_t*)e)->event == h->window) out->kind = FOCUS_LOST;
        break;
    case XCB_UNMAP_NOTIFY:
        if (((xcb_unmap_notify_event_t*)e)->window == h->window) out->kind = WINDOW_LOST;
        break;
    case XCB_DESTROY_NOTIFY:
        if (((xcb_destroy_notify_event_t*)e)->window == h->window) out->kind = WINDOW_LOST;
        break;
    case XCB_CONFIGURE_NOTIFY: {
        xcb_configure_notify_event_t *v = (void*)e;
        if (v->window == h->window) { out->kind = GEOMETRY; out->x=v->width; out->y=v->height; }
        break;
    }
    case XCB_VISIBILITY_NOTIFY:
        if (((xcb_visibility_notify_event_t*)e)->window == h->window &&
            ((xcb_visibility_notify_event_t*)e)->state != XCB_VISIBILITY_UNOBSCURED) out->kind = WINDOW_LOST;
        break;
    default: break;
    }
    free(e); return 1;
}
