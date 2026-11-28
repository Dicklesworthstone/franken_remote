/* Included after viewer_text.c. One explicit no-input interval on the SAME
 * capture owner, for the local file chooser. No X request blocks for a reply;
 * Rust drains bounded complete native batches and owns the pause/return gates.
 * Five fixed queries prove a fresh return to the original window. They do not
 * create remote readiness, input authority, a frame or a new source clock. */
#define FR_CHOICE_TURN_US UINT64_C(50000)

uint64_t fr_viewer_input_choice_now(void) {
    struct timespec now;
    if (clock_gettime(CLOCK_BOOTTIME, &now) || now.tv_sec < 0 ||
        now.tv_nsec < 0 || now.tv_nsec >= 1000000000L) return 0;
    uint64_t seconds = (uint64_t)now.tv_sec;
    uint64_t micros = (uint64_t)now.tv_nsec / 1000;
    if (seconds > (UINT64_MAX - micros) / UINT64_C(1000000)) return 0;
    return seconds * UINT64_C(1000000) + micros;
}

static void choice_discard(struct fr_viewer_input *h) {
    uint32_t *queries[] = {&h->choice_focus, &h->choice_geometry, &h->choice_attributes,
                           &h->choice_keys, &h->choice_pointer};
    for (unsigned i = 0; i < sizeof(queries) / sizeof(queries[0]); i++) {
        if (*queries[i]) xcb_discard_reply(h->c, *queries[i]);
        *queries[i] = 0;
    }
}
static int choice_fail(struct fr_viewer_input *h) {
    h->choice_failed = h->choice_changed = 1;
    h->choice_result = -1;
    choice_discard(h);
    return -1;
}
static int choice_text_busy(const struct fr_viewer_input *h) {
    const struct fr_viewer_text *t = h->text;
    return t && (t->active || t->armed || t->ic || t->draft_serial || t->pending_serial ||
        t->pending_kind || t->pending_length || t->awaiting_result ||
        text_bits(t->local_keys) || text_bits(t->local_buttons) ||
        text_bits(t->forwarded_keys) || text_bits(t->forwarded_buttons));
}
static int choice_busy(const struct fr_viewer_input *h) {
    return h->held_pending || h->keys_query || h->pointer_query || h->map_query ||
        h->names_query || h->release_state || h->release_query ||
        h->grab_focus || h->grab_geometry || h->grab_attributes || h->grab_keys || h->grab_query ||
        (h->grab_state && h->grab_state != FR_GRAB_ACQUIRED);
}
static int choice_native_failed(const struct fr_viewer_input *h) {
    return xcb_connection_has_error(h->c) || h->map_failed || h->choice_failed ||
        (h->text && atomic_load(&h->text->failed));
}

/* 1 paused, 0 refused WITHOUT mutation, -1 native failure. Rust has checked
 * its physical input ledger and original source queue before this call. */
int fr_viewer_input_choice_begin(struct fr_viewer_input *h) {
    if (!h || !h->c) return -1;
    if (choice_native_failed(h)) return -1;
    if (h->choice_mode || choice_busy(h) || choice_text_busy(h)) return 0;
    h->choice_mode = 1;
    h->choice_changed = h->choice_probe_state = h->choice_result = 0;
    h->choice_probe_at = 0;
    /* A successful, original keyboard grab is the ONLY grab released here.
     * No guessed ownership, pointer grab or new connection is involved. */
    if (h->grab_state == FR_GRAB_ACQUIRED &&
        !xcb_ungrab_keyboard(h->c, XCB_CURRENT_TIME).sequence) return choice_fail(h);
    if (!shortcuts_title(h, 0)) return choice_fail(h);
    h->grab_state = h->grab_attempted = h->grab_title = h->grab_focus_out = 0;
    h->grab_sequence = 0;
    return 1;
}

/* One set of five queries. 1 queued, 0 busy/refused, -1 terminal failure.
 * Every new probe resets its change marker before any native I/O. Draining
 * a focus/visibility/input event after this point invalidates its answer. */
int fr_viewer_input_choice_probe_begin(struct fr_viewer_input *h) {
    if (!h || !h->c || !h->choice_mode) return -1;
    if (choice_native_failed(h)) return choice_fail(h);
    if (h->choice_probe_state == 1 || choice_busy(h) || choice_text_busy(h)) return 0;
    uint64_t now = fr_viewer_input_choice_now();
    if (!now) return choice_fail(h);
    h->choice_changed = 0;
    h->choice_result = 1;
    h->choice_probe_state = 1;
    h->choice_probe_at = now;
    h->choice_focus = xcb_get_input_focus(h->c).sequence;
    h->choice_geometry = xcb_get_geometry(h->c, h->window).sequence;
    h->choice_attributes = xcb_get_window_attributes(h->c, h->window).sequence;
    h->choice_keys = xcb_query_keymap(h->c).sequence;
    h->choice_pointer = pointer_query(h);
    if (!h->choice_focus || !h->choice_geometry || !h->choice_attributes ||
        !h->choice_keys || !h->choice_pointer || xcb_flush(h->c) <= 0) return choice_fail(h);
    return 1;
}

static int choice_in_time(struct fr_viewer_input *h) {
    uint64_t now = fr_viewer_input_choice_now();
    return now && h->choice_probe_at && now >= h->choice_probe_at &&
        now - h->choice_probe_at < FR_CHOICE_TURN_US;
}

/* 0 pending; 1 eligible snapshot; 2 answered but not eligible; -1 terminal.
 * An ineligible focus/held-state answer never abandons the other replies. */
int fr_viewer_input_choice_probe_poll(struct fr_viewer_input *h) {
    if (!h || !h->c || !h->choice_mode || !h->choice_probe_state) return -1;
    if (choice_native_failed(h) || !choice_in_time(h)) return choice_fail(h);
    if (h->choice_probe_state == 2)
        return h->choice_result == 1 && !h->choice_changed && h->unobscured ? 1 : 2;
    if (h->choice_focus) {
        void *raw = NULL; xcb_generic_error_t *error = NULL;
        if (xcb_poll_for_reply(h->c, h->choice_focus, &raw, &error)) {
            xcb_get_input_focus_reply_t *r = raw;
            int ok = r && !error && r->response_type == 1 && !r->length;
            if (ok && r->focus != h->window) h->choice_result = 2;
            free(raw); free(error); h->choice_focus = 0;
            if (!ok) return choice_fail(h);
        }
    }
    if (h->choice_geometry) {
        void *raw = NULL; xcb_generic_error_t *error = NULL;
        if (xcb_poll_for_reply(h->c, h->choice_geometry, &raw, &error)) {
            xcb_get_geometry_reply_t *r = raw;
            int ok = r && !error && r->response_type == 1 && !r->length &&
                r->root != h->window && r->width == h->width && r->height == h->height;
            free(raw); free(error); h->choice_geometry = 0;
            if (!ok) return choice_fail(h);
        }
    }
    if (h->choice_attributes) {
        void *raw = NULL; xcb_generic_error_t *error = NULL;
        if (xcb_poll_for_reply(h->c, h->choice_attributes, &raw, &error)) {
            xcb_get_window_attributes_reply_t *r = raw;
            int ok = r && !error && r->response_type == 1 && r->length == 3 &&
                r->map_state == XCB_MAP_STATE_VIEWABLE && r->_class == XCB_WINDOW_CLASS_INPUT_OUTPUT;
            free(raw); free(error); h->choice_attributes = 0;
            if (!ok) return choice_fail(h);
        }
    }
    if (h->choice_keys) {
        void *raw = NULL; xcb_generic_error_t *error = NULL;
        if (xcb_poll_for_reply(h->c, h->choice_keys, &raw, &error)) {
            xcb_query_keymap_reply_t *r = raw;
            int ok = r && !error && r->response_type == 1 && r->length == 2;
            if (ok && text_bits((const uint8_t *)r->keys)) h->choice_result = 2;
            free(raw); free(error); h->choice_keys = 0;
            if (!ok) return choice_fail(h);
        }
    }
    if (h->choice_pointer) {
        void *raw = NULL; xcb_generic_error_t *error = NULL;
        if (xcb_poll_for_reply(h->c, h->choice_pointer, &raw, &error)) {
            xXIQueryPointerReply *r = raw;
            uint8_t bits[32];
            int ok = r && !error && r->repType == 1 && pointer_bits(r, bits);
            if (ok && text_bits(bits)) h->choice_result = 2;
            free(raw); free(error); h->choice_pointer = 0;
            if (!ok) return choice_fail(h);
        }
    }
    if (choice_native_failed(h) || !choice_in_time(h)) return choice_fail(h);
    if (h->choice_focus || h->choice_geometry || h->choice_attributes ||
        h->choice_keys || h->choice_pointer) return 0;
    h->choice_probe_state = 2;
    return h->choice_result == 1 && !h->choice_changed && h->unobscured ? 1 : 2;
}

/* Rust has drained through its final original-window barrier, discarded all
 * paused input, and reset its local ledgers. Any later native change refuses;
 * an old query cannot restore a focus, visibility, key or button transition. */
int fr_viewer_input_choice_end(struct fr_viewer_input *h) {
    if (!h || !h->c || !h->choice_mode) return -1;
    if (choice_native_failed(h)) return choice_fail(h);
    if (h->choice_probe_state != 2 || h->choice_result != 1 || h->choice_changed ||
        !h->unobscured || choice_busy(h) || choice_text_busy(h)) return 0;
    if (!choice_in_time(h)) return choice_fail(h);
    h->choice_mode = h->choice_probe_state = h->choice_result = 0;
    h->choice_probe_at = 0;
    return 1;
}
