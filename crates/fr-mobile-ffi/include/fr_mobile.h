/*
 * FrankenRemote iOS client core (crates/fr-mobile-ffi), hand-written C ABI.
 *
 * A session connects to one FrankenRemote host by its MagicDNS name
 * (<machine>.<tailnet>.ts.net) over the device's Tailscale connection,
 * verifies the host's certificate for exactly that name, shows its screen
 * and, once the host grants control, sends pointer, button, scroll and key
 * input. See docs/ios-client.md.
 *
 * Threading: any thread may call any function, concurrently. No function
 * blocks on the network. A freed or invented handle is refused, never used.
 *
 * Coordinates: input positions use an ENCODED PICTURE-sized area
 * (FrFrameInfo), after the app undid its own zoom, pan, rotation and safe-area
 * placement. Fit the ORIGINAL DISPLAY aspect into that area with integer
 * dimensions and centered offsets; draw and target its image rectangle.
 * Its letterbox bars are refused (FR_OUTSIDE), never moved onto an edge.
 * fr_session_frame_display supplies the original dimensions for a live
 * frame owner. Cursor positions, shape sizes and hotspots use ORIGINAL
 * DISPLAY pixels and must be scaled through the same image rectangle.
 *
 * Privacy: nothing here logs pixels, keys, text, names or addresses.
 */
#ifndef FR_MOBILE_H
#define FR_MOBILE_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <CoreFoundation/CoreFoundation.h>
#include <CoreVideo/CVPixelBuffer.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Status codes (input calls and fr_session_start). */
#define FR_OK 0
#define FR_PENDING 1          /* accepted, nothing sent yet (sub-line scroll, key already in that state) */
#define FR_INVALID (-1)       /* bad argument, name, pin or usage */
#define FR_NOT_CONTROLLED (-2)
#define FR_OUTSIDE (-3)       /* position outside the picture */
#define FR_UNSUPPORTED (-4)   /* the host did not grant this kind of input */
#define FR_CONTROL_LOST (-5)  /* control (and so the session) is ending */
#define FR_CLOSED (-6)
#define FR_BUSY (-7)          /* start: all session slots in use, or no thread;
                                 input: nothing accepted. Physical keys may
                                 wait; completed text must be discarded,
                                 never queued or retried automatically. */
#define FR_STALE (-8)         /* the session or original request is no longer live */

/* Display policy for fr_session_start_with_display (FrConfig is unchanged). */
#define FR_DISPLAY_PRIMARY 0  /* desktop origin, else first approved display */
#define FR_DISPLAY_CHOOSE 1   /* one display: open; several: ask the user */
#define FR_MAX_DISPLAYS 8
#define FR_UPLOAD_STATUS_MAX_BYTES 16384
#define FR_DOWNLOAD_STATUS_MAX_BYTES 32768

/* Event kinds. */
#define FR_EVENT_STATE 1      /* value: FR_STATE_* */
#define FR_EVENT_PIN 2        /* a host pin is available: fr_session_copy_pin */
#define FR_EVENT_VIDEO 3      /* display or picture size (nonzero fields) */
#define FR_EVENT_ENDED 4      /* value: FR_END_*; always the last event */
#define FR_EVENT_OVERFLOW 5   /* older events were dropped; re-read state and stats */

/* Session states. */
#define FR_STATE_CONNECTING 1
#define FR_STATE_AUTHENTICATED 2
#define FR_STATE_AWAITING_APPROVAL 3
#define FR_STATE_VIEWING 4
#define FR_STATE_REQUESTING_CONTROL 5
#define FR_STATE_CONTROLLED 6
#define FR_STATE_RECONNECTING 7
#define FR_STATE_CLOSING 8
#define FR_STATE_CONTROL_ENDED 9

/* Why a session ended. */
#define FR_END_OK 0               /* closed by this app */
#define FR_END_HOST_CLOSED 1
#define FR_END_NAME_UNRESOLVED 2  /* MagicDNS gave no address: Tailscale off? */
#define FR_END_OUTSIDE_TAILNET 3  /* the name resolved outside the tailnet */
#define FR_END_IDENTITY_CHANGED 4 /* resolved addresses differ from the pin */
#define FR_END_TLS_REFUSED 5      /* certificate not valid for the name */
#define FR_END_UNREACHABLE 6
#define FR_END_HOST_REFUSED 7     /* host policy or protocol refusal */
#define FR_END_SESSION_FAILED 8
#define FR_END_CANCELLED 9
#define FR_END_INTERNAL 10
#define FR_END_CONTROL_STOPPED 11 /* this device stopped control (view hidden,
                                     input refused, picture resized); the
                                     session ended with it */
#define FR_END_HOST_BUSY 12       /* the host is serving another viewer */

/* A generation-checked session handle (ADR 0003). Never 0 when issued; a
 * handle used after fr_session_free finds nothing (FR_STALE). */
typedef uint64_t FrHandle;

typedef struct FrConfig {
    const char *_Nonnull host;      /* NUL-terminated MagicDNS name */
    uint16_t port;                  /* frd's port, nonzero */
    bool control;                   /* control-capable session */
    bool direct;                    /* dial the pin's addresses, no resolver
                                       (a directory machine); needs pin */
    const char *_Nullable pin;      /* pin saved from an earlier session */
    uint32_t picture_width;         /* this screen's pixels, longer side */
    uint32_t picture_height;        /* first; 0, 0 = host's native pixels.
                                       A Mac host then sends pictures that
                                       fit; each axis 0 or at least 64 */
} FrConfig;

typedef struct FrEvent {
    uint32_t kind;
    uint32_t value;
    uint32_t attempt;
    uint32_t display_width;   /* host display pixels (VIDEO) */
    uint32_t display_height;
    uint32_t picture_width;   /* encoded picture pixels (VIDEO) */
    uint32_t picture_height;
    uint32_t reserved;
} FrEvent;

typedef struct FrFrameInfo {
    uint64_t sequence;   /* pass back as after_sequence */
    uint64_t frame;      /* pass to fr_session_frame_drawn once presented */
    uint32_t width;
    uint32_t height;
    uint32_t full_range; /* 1 full-range NV12, 0 video range (BT.709) */
    uint32_t reserved;
} FrFrameInfo;

typedef struct FrStats {
    uint64_t presented;         /* pictures decoded for display */
    uint64_t drawn_frame;       /* newest frame id reported drawn */
    uint64_t decoded;           /* access units decoded */
    uint64_t decode_micros;     /* total decode wall time */
    uint64_t first_view_micros; /* start to first drawn picture, 0 if none */
    int32_t hardware;           /* 1 hardware decode, 0 software, -1 unknown */
    uint32_t controlled;
    uint32_t end;               /* FR_END_* once ended, else UINT32_MAX */
    uint32_t reserved;
} FrStats;

/* Content-free status of this original native text clipboard. */
typedef struct FrClipboardStatus {
    uint32_t requested;
    uint32_t enabled;
    uint32_t started;
    uint32_t reason; /* 0 none, 1 unavailable/declined, 2 Off/privacy, 3 failure */
    uint64_t received_submitted;
    uint64_t received_unknown;
    uint64_t received_refused;
} FrClipboardStatus;

/* iOS: start a control-capable session with text clipboard sharing. No text
 * access before the actual grant and bilateral clipboard readiness. Optional
 * upload and receive consent retain the existing file limits. Zero destination
 * means no receiving; a nonzero token is consumed even on failed start. */
int32_t fr_session_start_with_clipboard(const FrConfig *_Nullable config,
    uint32_t mode, bool upload, uint64_t destination, FrHandle *_Nullable out);
bool fr_session_clipboard_status(FrHandle handle, FrClipboardStatus *_Nullable out);
/* Call while the persistent On/Off control is visible in the active app.
 * A hidden control or over 300 ms without an answer retires its original owner. */
int32_t fr_session_clipboard_presence(FrHandle handle, bool visible);
int32_t fr_session_stop_clipboard(FrHandle handle);

typedef struct FrCursor {
    uint64_t generation;
    int32_t x;               /* original selected display pixels */
    int32_t y;
    uint16_t width;
    uint16_t height;
    uint16_t hotspot_x;
    uint16_t hotspot_y;
    uint32_t visible;
    uint32_t reserved;
} FrCursor;

/* A single approved catalog in one connection attempt. The token is local,
 * nonzero and never reused, even by a later session with identical rows.
 * Host-owned display aliases and geometry generations stay inside Rust. */
typedef struct FrDisplayRequest {
    uint64_t token;
    uint32_t count;           /* 1..FR_MAX_DISPLAYS */
    uint32_t reserved;
} FrDisplayRequest;

typedef struct FrDisplayInfo {
    uint32_t pixel_width;    /* post-rotation desktop pixels */
    uint32_t pixel_height;
    uint32_t logical_width;
    uint32_t logical_height;
    int32_t x;               /* desktop-space origin; may be negative */
    int32_t y;
    uint32_t scale_numerator;
    uint32_t scale_denominator;
    uint32_t rotation;       /* clockwise quarter turns, 0..3 */
    uint32_t reserved;
} FrDisplayInfo;

bool fr_host_name_valid(const char *_Nullable name);

int32_t fr_session_start(const FrConfig *_Nullable config, FrHandle *_Nullable out);
int32_t fr_session_start_with_display(const FrConfig *_Nullable config,
                                      uint32_t mode, FrHandle *_Nullable out);
/* Optional, empty upload negotiation on a control-capable session. No file
 * is selected or sent, and no control is requested by this call. Existing
 * start entry points and FrConfig retain their original ABI/behavior. */
int32_t fr_session_start_with_upload(const FrConfig *_Nullable config,
                                    uint32_t mode, FrHandle *_Nullable out);

/* Choose while observing, before the native picker covers the view. Tokens
 * belong to one connection attempt, expire after 120 s and are never reused.
 * Observe real visibility throughout the picker. No file contents are read
 * without a later fresh Send gesture and the host's current control grant. */
int32_t fr_session_begin_upload_selection(FrHandle session, uint64_t *_Nullable out);
/* Synchronous filesystem work: call only on a bounded background worker.
 * path is an absolute UTF-8 local regular-file path (at most 4096 bytes).
 * Keep the selected URL's security-scoped permission through preparation.
 * On successful Send keep it until upload_sources_released returns true. */
int32_t fr_session_prepare_upload(FrHandle session, uint64_t token,
                                  const char *_Nullable path);
/* Discard only this token. An outstanding prepare call must still return
 * before its native permission is released or another worker is launched. */
int32_t fr_session_discard_upload_selection(FrHandle session, uint64_t token);
/* Explicit once-only Send on a current controlled view. Every attempted Send
 * consumes the selection, including FR_BUSY/refusal; never retry it. */
int32_t fr_session_send_upload(FrHandle session, uint64_t token);
/* File-only commands for the exact displayed original batch (1..9). */
int32_t fr_session_cancel_upload(FrHandle session, uint32_t batch);
int32_t fr_session_pause_upload(FrHandle session, uint32_t batch, bool paused);
/* Positive source collection, even after an original cleanup failure.
 * A terminal event alone is insufficient. Retain this exact handle and the
 * native scopes until true, then free; free itself does not join workers.
 * False for a stale handle. A later collection never repairs a failure. */
bool fr_session_upload_sources_released(FrHandle session);
/* Bounded version-1 JSON: selection, current transfer, at most nine receipts,
 * ready and sources_released. No paths, names or file contents. Queued bytes
 * are not host publication. Returns required bytes EXCLUDING NUL, like
 * copy_pin; use FR_UPLOAD_STATUS_MAX_BYTES bytes including termination.
 * 0 means stale/unavailable, never successful transfer. */
size_t fr_session_copy_upload_status(FrHandle session, char *_Nullable buffer, size_t capacity);

/* Explicit receive consent. Preparation does filesystem work on a bounded
 * background worker, pins an absolute private directory, and returns a one-use
 * token. At most four destinations may be preparing/prepared at once. Limits
 * must be positive with max_session_bytes >= max_file_bytes. Conflicts keep
 * both files. No network, control grant, receive or automatic opening occurs.
 * On failure out is untouched. Discard a token if its foreground choice ends. */
int32_t fr_download_destination_prepare(const char *_Nullable path,
                                        uint64_t max_file_bytes,
                                        uint64_t max_session_bytes,
                                        uint64_t *_Nullable out);
int32_t fr_download_destination_discard(uint64_t destination);
/* Start on the foreground thread after rechecking the original user choice.
 * Consumes destination on EVERY attempt, even invalid arguments/start failure.
 * No filesystem I/O occurs here. Control-capable config required; control is
 * still requested separately. upload negotiates the existing empty send lane.
 * Reconnect/display replacement never reuses this receive permission. */
int32_t fr_session_start_with_files(const FrConfig *_Nullable config,
                                   uint32_t mode, bool upload,
                                   uint64_t destination, FrHandle *_Nullable out);
/* Stop receiving only; leave viewing/control alive and retain original
 * publication receipts. Cannot resume or redirect the original destination. */
int32_t fr_session_cancel_download(FrHandle session);
/* Terminal original session AND positive receive disk cleanup; false for
 * stale handles or failed/pending cleanup. Retain the
 * handle/private destination until true; free does not join disk workers. */
bool fr_session_download_cleanup_complete(FrHandle session);
/* Bounded version-1 JSON. Staged progress, atomic publication, durability,
 * omission and cleanup are separate fields. No names, paths or contents.
 * Required bytes exclude NUL; zero is unavailable, never success. */
size_t fr_session_copy_download_status(FrHandle session, char *_Nullable buffer, size_t capacity);

/* Poll before the first frame, while startup/approval expiry still runs.
 * false leaves out untouched. Copy every row with this same token; if any
 * copy fails, discard the entire snapshot. Never replay a choice against
 * a new token. Selection succeeds once and grants no input authority. */
bool fr_session_display_request(FrHandle session, FrDisplayRequest *_Nullable out);
bool fr_session_display_info(FrHandle session, uint64_t token, uint32_t index,
                             FrDisplayInfo *_Nullable out);
int32_t fr_session_select_display(FrHandle session, uint64_t token, uint32_t index);
/* Cancel/dismiss closes only the original still-pending startup. A stale
 * token cannot close a replacement attempt or an already accepted choice. */
int32_t fr_session_cancel_display(FrHandle session, uint64_t token);
/* After asking to close: FR_PENDING while running, FR_OK only when the
 * original service AND cleanup succeeded, FR_CLOSED otherwise; FR_STALE
 * for an unknown handle. An end event alone is insufficient. Retain the old
 * handle until FR_OK before opening a replacement observing session, with
 * a fresh catalog and no carried-over control request or input. */
int32_t fr_session_reopen_status(FrHandle session);
bool fr_session_poll_event(FrHandle session, FrEvent *_Nullable event);
size_t fr_session_copy_pin(FrHandle session, char *_Nullable buffer, size_t capacity);
size_t fr_session_copy_detail(FrHandle session, char *_Nullable buffer, size_t capacity);

/* +1 IOSurface-backed NV12 picture newer than after_sequence, else NULL. */
CVPixelBufferRef _Nullable fr_session_copy_frame(FrHandle session,
                                                 uint64_t after_sequence,
                                                 FrFrameInfo *_Nullable info)
    CF_RETURNS_RETAINED;
/* Original approved display for the live owner of FrFrameInfo.frame.
 * Recheck before repainting a retained frame. False for stale, retired or
 * closed owners, unknown handles or NULL outputs; refusal writes neither.
 * VIDEO events are lossy notifications, not presentation geometry authority.
 * This does not grant visibility or input authority; FrFrameInfo ABI is unchanged. */
bool fr_session_frame_display(FrHandle session, uint64_t frame,
                                uint32_t *_Nullable width, uint32_t *_Nullable height);
void fr_session_frame_drawn(FrHandle session, uint64_t frame);
void fr_session_set_visible(FrHandle session, bool visible);

int32_t fr_session_request_control(FrHandle session);
void fr_session_release_control(FrHandle session);
int32_t fr_session_pointer(FrHandle session, double x, double y);
int32_t fr_session_button(FrHandle session, uint32_t button, bool pressed,
                          double x, double y);
int32_t fr_session_scroll(FrHandle session, double dx, double dy, double x, double y);
/* Precise scroll is independently negotiated; false is not permission to
 * convert these distances to lines. The send call still rechecks authority.
 * dx/dy are logical native scroll units, unaffected by zoom, picture size or
 * screen density; x/y remain encoded picture coordinates. Signed 16.16 wire
 * quantization retains only a bounded original-deadline sub-unit fraction. */
bool fr_session_supports_pixel_scroll(FrHandle session);
int32_t fr_session_scroll_pixels(FrHandle session, double dx, double dy, double x, double y);
void fr_session_scroll_end(FrHandle session);
int32_t fr_session_key(FrHandle session, uint16_t usage, bool pressed);
/* Capture once BEFORE mapping/queueing a completed ASCII callback. Refusal
 * writes no token; never recapture that callback later. At most 64 outstanding
 * callbacks and 4096 strokes per callback. The original exclusive input bound
 * and source/view context cannot be renewed. */
int32_t fr_session_begin_typing(FrHandle session, uint64_t *_Nullable out);
/* One complete stroke: modifiers bits 0..7 name HID 0xE0..0xE7. index starts
 * at zero and advances only after FR_OK/FR_PENDING. FR_BUSY queued nothing:
 * retry only this same token/index/usage/modifiers, never a fresh capture. */
int32_t fr_session_type_captured(FrHandle session, uint64_t token, uint32_t index,
                                 uint16_t usage, uint8_t modifiers);
void fr_session_end_typing(FrHandle session, uint64_t token);
/* Current negotiated capability only; fr_session_text still checks readiness. */
bool fr_session_supports_text(FrHandle session);
/* One completed Unicode callback, captured synchronously once. utf8 is
 * nonempty, at most 4096 UTF-8 bytes and contains no embedded NUL. Refusal
 * admits none of the value; never split, queue or automatically retry it. */
int32_t fr_session_text(FrHandle session, const char *_Nullable utf8);
int32_t fr_session_release_all(FrHandle session);

bool fr_session_cursor(FrHandle session, FrCursor *_Nullable out);
/* Metadata for one retained picture: pass its FrFrameInfo.frame and sequence.
 * Position, shape and hotspot use original selected display pixels. Refuses
 * stale attempts and pictures at/below the latest decoder reset's sequence.
 * false leaves out untouched; discard any cached cursor overlay on refusal.
 * After copying RGBA separately, re-read this metadata and require the same
 * generation before rendering the copied shape. */
bool fr_session_frame_cursor(FrHandle session, uint64_t frame, uint64_t sequence,
                             FrCursor *_Nullable out);
size_t fr_session_copy_cursor_rgba(FrHandle session, uint8_t *_Nullable buffer,
                                   size_t capacity);
bool fr_session_stats(FrHandle session, FrStats *_Nullable out);

int32_t fr_session_close(FrHandle session);
int32_t fr_session_free(FrHandle session);

/*
 * Tailnet directory (frd run --directory, PROTOCOL_DIRECTORY.md): ask the
 * host in config for itself and this user's own untagged machines.
 * config->control is ignored. On FR_OK, done is called exactly once, on
 * another thread, with a JSON document valid only during the call:
 *   {"status":"ok","via":NAME,"truncated":BOOL,"machines":[{"name","node_id",
 *    "os","online","serving","ipv4","ipv6","pin"}]}  (absent values: null)
 *   {"status":"refused","via":NAME,"reason":"directory_disabled"|...}
 *   {"status":"failed","via":NAME,"end":FR_END_*,"detail":TYPED_FAILURE}
 * A machine's pin opens it with FrConfig.direct. Otherwise (FR_INVALID,
 * FR_BUSY) done is never called. context must stay valid until done returns.
 */
typedef void (*FrDirectoryDone)(void *_Nullable context, const char *_Nonnull json);
int32_t fr_directory_fetch(const FrConfig *_Nullable config, void *_Nullable context,
                           FrDirectoryDone _Nullable done);

#ifdef __cplusplus
}
#endif

#endif /* FR_MOBILE_H */
