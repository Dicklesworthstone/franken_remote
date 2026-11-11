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
 * Coordinates: input positions are ENCODED PICTURE pixels (the size in
 * FR_EVENT_VIDEO / FrFrameInfo), after the app undid its own zoom, pan,
 * letterbox, rotation and safe-area placement. A point outside the picture
 * is refused (FR_OUTSIDE), never moved onto an edge.
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
                                 key press/text: the host's input queue is
                                 backed up, nothing sent: send it again next
                                 frame */
#define FR_STALE (-8)         /* the handle names no live session */

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

typedef struct FrCursor {
    uint64_t generation;
    int32_t x;               /* picture pixels */
    int32_t y;
    uint16_t width;
    uint16_t height;
    uint16_t hotspot_x;
    uint16_t hotspot_y;
    uint32_t visible;
    uint32_t reserved;
} FrCursor;

bool fr_host_name_valid(const char *_Nullable name);

int32_t fr_session_start(const FrConfig *_Nullable config, FrHandle *_Nullable out);
bool fr_session_poll_event(FrHandle session, FrEvent *_Nullable event);
size_t fr_session_copy_pin(FrHandle session, char *_Nullable buffer, size_t capacity);
size_t fr_session_copy_detail(FrHandle session, char *_Nullable buffer, size_t capacity);

/* +1 IOSurface-backed NV12 picture newer than after_sequence, else NULL. */
CVPixelBufferRef _Nullable fr_session_copy_frame(FrHandle session,
                                                 uint64_t after_sequence,
                                                 FrFrameInfo *_Nullable info)
    CF_RETURNS_RETAINED;
void fr_session_frame_drawn(FrHandle session, uint64_t frame);
void fr_session_set_visible(FrHandle session, bool visible);

int32_t fr_session_request_control(FrHandle session);
void fr_session_release_control(FrHandle session);
int32_t fr_session_pointer(FrHandle session, double x, double y);
int32_t fr_session_button(FrHandle session, uint32_t button, bool pressed,
                          double x, double y);
int32_t fr_session_scroll(FrHandle session, double dx, double dy, double x, double y);
void fr_session_scroll_end(FrHandle session);
int32_t fr_session_key(FrHandle session, uint16_t usage, bool pressed);
int32_t fr_session_text(FrHandle session, const char *_Nullable utf8);
int32_t fr_session_release_all(FrHandle session);

bool fr_session_cursor(FrHandle session, FrCursor *_Nullable out);
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
