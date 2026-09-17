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
