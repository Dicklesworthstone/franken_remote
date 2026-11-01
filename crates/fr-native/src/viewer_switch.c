/* Included only by viewer_window.c: the locally owned Change Display gesture.
 * Physical XKB names match the controlling adapter. No capture window, input
 * injection, grab, peer-controlled string or catalog identity is introduced. */
static xcb_extension_t switch_xkb = { "XKEYBOARD", 0 };
static uint32_t switch_request(struct fr_viewer_window *h, uint8_t opcode,
        int isvoid, void *body, size_t size) {
    struct iovec parts[4] = {{0}};
    parts[2].iov_base = body; parts[2].iov_len = size;
    parts[3].iov_len = (-size) & 3;
    xcb_protocol_request_t request = {2, &switch_xkb, opcode, (uint8_t)isvoid};
    return xcb_send_request(h->c, XCB_REQUEST_CHECKED, parts + 2, &request);
}
static void *switch_reply(struct fr_viewer_window *h, uint32_t sequence) {
    xcb_generic_error_t *error = NULL;
    void *reply = xcb_wait_for_reply(h->c, sequence, &error);
    if (error || xcb_connection_has_error(h->c)) { free(reply); reply = NULL; }
    free(error); return reply;
}
static int switch_map(struct fr_viewer_window *h) {
    static const char names[7][5] = {
        "LCTL", "RCTL", "LALT", "RALT", "LFSH", "RTSH", "FK07"
    };
    const xcb_query_extension_reply_t *extension = xcb_get_extension_data(h->c, &switch_xkb);
    if (!extension || !extension->present) return 0;
    h->switch_xkb = extension->first_event;
    xkbUseExtensionReq use = {0}; use.wantedMajor = 1;
    xkbUseExtensionReply *version = switch_reply(h,
        switch_request(h, X_kbUseExtension, 0, &use, sizeof(use)));
    int ok = version && version->supported && !version->length;
    free(version); if (!ok) return 0;
    xkbSelectEventsReq select = {0}; select.deviceSpec = XkbUseCoreKbd;
    select.affectWhich = select.selectAll = XkbNewKeyboardNotifyMask | XkbMapNotifyMask | XkbNamesNotifyMask;
    select.affectMap = select.map = XkbAllMapComponentsMask;
    xcb_void_cookie_t cookie = { switch_request(h, X_kbSelectEvents, 1, &select, sizeof(select)) };
    if (!checked(h, cookie)) return 0;
    xkbPerClientFlagsReq flags = {0}; flags.deviceSpec = XkbUseCoreKbd;
    flags.change = flags.value = XkbPCF_DetectableAutoRepeatMask;
    xkbPerClientFlagsReply *repeat = switch_reply(h,
        switch_request(h, X_kbPerClientFlags, 0, &flags, sizeof(flags)));
    ok = repeat && !repeat->length && (repeat->supported & XkbPCF_DetectableAutoRepeatMask) &&
        (repeat->value & XkbPCF_DetectableAutoRepeatMask);
    free(repeat); if (!ok) return 0;
    xkbGetNamesReq get = {0}; get.deviceSpec = XkbUseCoreKbd; get.which = XkbKeyNamesMask;
    xkbGetNamesReply *reply = switch_reply(h,
        switch_request(h, X_kbGetNames, 0, &get, sizeof(get)));
    ok = reply && reply->which == XkbKeyNamesMask && reply->nKeys &&
        (unsigned)reply->firstKey + reply->nKeys <= 256 && reply->length == reply->nKeys;
    memset(h->switch_keys, 0, sizeof(h->switch_keys));
    if (ok) {
        const char (*actual)[4] = (const void *)(reply + 1);
        for (unsigned i = 0; i < reply->nKeys; ++i) {
            for (unsigned slot = 0; slot < 7; ++slot) {
                if (!memcmp(actual[i], names[slot], 4)) {
                    unsigned code = (unsigned)reply->firstKey + i;
                    if (code < 8 || h->switch_keys[slot]) ok = 0;
                    h->switch_keys[slot] = (uint8_t)code;
                }
            }
        }
        for (unsigned group = 0; group < 3; ++group)
            if (!h->switch_keys[group * 2] && !h->switch_keys[group * 2 + 1]) ok = 0;
        if (!h->switch_keys[6]) ok = 0;
    }
    free(reply);
    if (!ok) memset(h->switch_keys, 0, sizeof(h->switch_keys));
    return ok;
}
/* Both shortcuts and file drops share ONE event-mask owner. Changing either
 * feature must not remove the other's subscription or title affordance. */
static int switch_surface(struct fr_viewer_window *h, int control_offered) {
    const char *title;
    if (h->switch_mode && h->switch_keys[6]) {
        title = h->control_key ?
            "FrankenRemote - View only - F8 requests control - Ctrl+Alt+Shift+F7 changes display" :
            h->switch_mode == 2 ?
            "FrankenRemote - Ctrl+Alt+Shift+F7 changes display - Ctrl+Alt+Shift+Escape releases control" :
            "FrankenRemote - Ctrl+Alt+Shift+F7 changes display";
    } else {
        title = !control_offered ? "FrankenRemote" : h->control_key ?
            "FrankenRemote - View only - F8 requests control" :
            "FrankenRemote - View only - F8 unavailable";
    }
    uint32_t mask = XCB_EVENT_MASK_STRUCTURE_NOTIFY | XCB_EVENT_MASK_FOCUS_CHANGE;
    if (h->control_key || (h->switch_mode == 1 && h->switch_keys[6]))
        mask |= XCB_EVENT_MASK_KEY_PRESS | XCB_EVENT_MASK_KEY_RELEASE | XCB_EVENT_MASK_PROPERTY_CHANGE;
    if (h->files) mask |= XCB_EVENT_MASK_PROPERTY_CHANGE;
    return checked(h, xcb_change_window_attributes_checked(h->c, h->window,
        XCB_CW_EVENT_MASK, &mask)) &&
        property(h, XCB_ATOM_WM_NAME, XCB_ATOM_STRING, 8, (uint32_t)strlen(title), title) &&
        property(h, h->net_name, h->utf8, 8, (uint32_t)strlen(title), title);
}
static int switch_barrier(struct fr_viewer_window *h, uint8_t kind) {
    if (h->switch_sequence) return 0;
    uint32_t value = kind;
    xcb_void_cookie_t request = xcb_change_property_checked(h->c, XCB_PROP_MODE_REPLACE,
        h->window, h->switch_clock, XCB_ATOM_CARDINAL, 32, 1, &value);
    h->switch_sequence = request.sequence; h->switch_barrier_kind = kind;
    return request.sequence && xcb_flush(h->c) > 0;
}
void fr_viewer_window_display_switch_reset(struct fr_viewer_window *h) {
    if (!h) return;
    h->switch_pressed = h->switch_released = 0;
    h->switch_age = UINT32_MAX;
    memset(h->switch_held, 0, sizeof(h->switch_held));
    if (h->switch_barrier_kind == 2) h->switch_barrier_kind = 0;
}
/* 0 disabled; 1 viewing (this connection observes keys); 2 controlling (the
 * original native input-capture owner consumes the physical chord instead). */
int fr_viewer_window_display_switch_offer(struct fr_viewer_window *h, int mode) {
    if (!h || mode < 0 || mode > 2 || xcb_connection_has_error(h->c)) return -1;
    fr_viewer_window_display_switch_reset(h);
    h->switch_sequence = h->switch_ready = h->switch_barrier_kind = 0;
    h->switch_mode = (uint8_t)mode;
    memset(h->switch_keys, 0, sizeof(h->switch_keys));
    if (mode) {
        if (!switch_map(h)) return xcb_connection_has_error(h->c) ? -1 :
            (switch_surface(h, h->control_key != 0) ? 0 : -1);
        int held = control_held(h);
        if (held < 0) return -1;
        h->switch_blocked = (uint8_t)held;
        if (!h->switch_clock) h->switch_clock = atom(h, "_FR_VIEWER_DISPLAY_SWITCH_CLOCK");
        if (!h->switch_clock) return -1;
    }
    if (!switch_surface(h, h->control_key != 0)) return -1;
    if (mode == 1 && !switch_barrier(h, 1)) return -1;
    return 1;
}
uint32_t fr_viewer_window_display_switch_age(struct fr_viewer_window *h) {
    return h ? h->switch_age : UINT32_MAX;
}
int fr_viewer_window_display_switch_released(struct fr_viewer_window *h) {
    if (!h || xcb_connection_has_error(h->c)) return -1;
    if (h->switch_mode != 1 || !h->switch_keys[6] ||
        h->switch_age >= 250 || h->switch_blocked) return 0;
    xcb_generic_error_t *error = NULL;
    xcb_get_input_focus_reply_t *focus = xcb_get_input_focus_reply(h->c,
        xcb_get_input_focus(h->c), &error);
    int valid = focus && !error, focused = valid && focus->focus == h->window;
    free(focus); free(error);
    if (!valid) return -1;
    if (!focused) return 0;
    int held = control_held(h);
    return held < 0 ? -1 : !held;
}
static int switch_event(struct fr_viewer_window *h, xcb_generic_event_t *event, uint32_t *kind) {
    uint8_t type = event->response_type & 0x7f;
    int synthetic = (event->response_type & 0x80) != 0;
    if (type == XCB_MAPPING_NOTIFY || (h->switch_xkb && type == h->switch_xkb)) {
        fr_viewer_window_display_switch_reset(h);
        h->switch_ready = 0; *kind = SWITCH_REMAP;
        return 1;
    }
    if (type == XCB_FOCUS_OUT || type == XCB_UNMAP_NOTIFY || type == XCB_DESTROY_NOTIFY ||
        type == XCB_CLIENT_MESSAGE) {
        fr_viewer_window_display_switch_reset(h);
        return 1;
    }
    if (h->switch_mode != 1 || !h->switch_keys[6]) return 1;
    if (type == XCB_PROPERTY_NOTIFY) {
        const xcb_property_notify_event_t *property_event = (const void *)event;
        if (!synthetic && property_event->window == h->window &&
            property_event->atom == h->switch_clock && property_event->state == XCB_PROPERTY_NEW_VALUE &&
            h->switch_sequence && event->full_sequence == h->switch_sequence) {
            uint8_t pending = h->switch_barrier_kind;
            h->switch_sequence = h->switch_barrier_kind = 0;
            if (pending == 1) {
                int held = control_held(h);
                if (held < 0) return 0;
                h->switch_baseline = property_event->time;
                h->switch_blocked = (uint8_t)held; h->switch_ready = !held;
            } else if (pending == 2) {
                uint32_t age = property_event->time - h->switch_press_at;
                if (age < 249) { h->switch_age = age + 1; *kind = SWITCH_CHOSEN; }
            }
        }
        return 1;
    }
    if (type != XCB_KEY_PRESS && type != XCB_KEY_RELEASE) return 1;
    const xcb_key_press_event_t *key = (const void *)event;
    if (key->event != h->window) return 1;
    if (synthetic || !key->same_screen) {
        fr_viewer_window_display_switch_reset(h); *kind = SWITCH_RESET; return 1;
    }
    if (!h->switch_ready || h->switch_blocked) return 1;
    unsigned slot = 7;
    for (unsigned i = 0; i < 7; ++i) if (h->switch_keys[i] == key->detail) { slot = i; break; }
    if (slot == 7) { fr_viewer_window_display_switch_reset(h); *kind = SWITCH_RESET; return 1; }
    int down = type == XCB_KEY_PRESS, was_down = h->switch_held[slot];
    h->switch_held[slot] = (uint8_t)down;
    if (slot == 6 && down && !was_down) {
        uint32_t since = key->time - h->switch_baseline;
        h->switch_pressed = (uint8_t)(since && since < (1u << 31) &&
            (h->switch_held[0] || h->switch_held[1]) &&
            (h->switch_held[2] || h->switch_held[3]) &&
            (h->switch_held[4] || h->switch_held[5]));
        h->switch_released = 0;
        if (h->switch_pressed) h->switch_press_at = key->time;
    }
    if (slot == 6 && !down && h->switch_pressed) h->switch_released = 1;
    int held = 0;
    for (unsigned i = 0; i < 7; ++i) held |= h->switch_held[i];
    if (!held && h->switch_pressed && h->switch_released && !h->switch_sequence) {
        h->switch_release_at = key->time;
        if (!switch_barrier(h, 2)) return 0;
        h->switch_pressed = h->switch_released = 0;
        *kind = SWITCH_CLOCK;
    }
    return 1;
}
