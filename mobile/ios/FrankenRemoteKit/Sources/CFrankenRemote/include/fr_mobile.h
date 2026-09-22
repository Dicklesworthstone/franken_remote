/*
 * fr_mobile.h — FrankenRemote Mobile C-ABI Interface
 *
 * Provides a generation-checked, lifecycle-safe C ABI boundary over fr-client
 * for iOS (Swift) and Android (Kotlin/JNI).
 *
 * Constitutional Invariants (ADR 0003 & Plan Section 16.2):
 * 1. All logic stays in Rust (fr-client).
 * 2. Generation-checked opaque handles: stale handle access returns FR_ERR_STALE_HANDLE (-2).
 * 3. Double-free operations are safely detected and rejected without undefined behavior.
 * 4. Decoded video never crosses FFI as CPU pixels (CAMetalLayer / ANativeWindow direct handoff).
 */

#ifndef FR_MOBILE_H
#define FR_MOBILE_H

#include <stdint.h>
#include <stdbool.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Result / Error codes */
#define FR_OK                         0
#define FR_ERR_INVALID_ARGUMENT      -1
#define FR_ERR_STALE_HANDLE          -2
#define FR_ERR_ALREADY_CLOSED        -3
#define FR_ERR_CONNECTION_FAILED     -4
#define FR_ERR_TIMEOUT               -5
#define FR_ERR_PERMISSION_DENIED     -6
#define FR_ERR_UNSUPPORTED           -7

/* Opaque generation-checked handle */
typedef uint64_t fr_session_handle;

/* Session state */
typedef enum {
    FR_SESSION_STATE_DISCONNECTED = 0,
    FR_SESSION_STATE_CONNECTING = 1,
    FR_SESSION_STATE_AUTHENTICATING = 2,
    FR_SESSION_STATE_CONNECTED = 3,
    FR_SESSION_STATE_RECONNECTING = 4,
    FR_SESSION_STATE_FAILED = 5
} fr_session_state;

/* Pointer action */
typedef enum {
    FR_POINTER_ACTION_MOVE = 0,
    FR_POINTER_ACTION_DOWN = 1,
    FR_POINTER_ACTION_UP = 2,
    FR_POINTER_ACTION_CANCEL = 3
} fr_pointer_action;

/* Pointer button */
typedef enum {
    FR_POINTER_BUTTON_NONE = 0,
    FR_POINTER_BUTTON_PRIMARY = 1,
    FR_POINTER_BUTTON_SECONDARY = 2,
    FR_POINTER_BUTTON_MIDDLE = 3
} fr_pointer_button;

/* Key action */
typedef enum {
    FR_KEY_ACTION_DOWN = 0,
    FR_KEY_ACTION_UP = 1
} fr_key_action;

/* Connection quality metric snapshot */
typedef struct {
    uint32_t rtt_ms;
    uint32_t jitter_ms;
    uint32_t loss_permille;
    uint32_t fps;
    uint32_t bitrate_kbps;
    uint32_t decode_time_us;
    uint32_t render_time_us;
    uint32_t quality_tier; /* 0=Excellent, 1=Good, 2=Degraded, 3=Poor */
} fr_connection_quality;

/* Callback function signatures */
typedef void (*fr_state_callback_fn)(fr_session_handle session, int32_t state, const char* detail, void* user_data);
typedef void (*fr_geometry_callback_fn)(fr_session_handle session, uint32_t width, uint32_t height, uint32_t scale_num, uint32_t scale_den, void* user_data);
typedef void (*fr_quality_callback_fn)(fr_session_handle session, const fr_connection_quality* quality, void* user_data);
typedef void (*fr_clipboard_callback_fn)(fr_session_handle session, const char* text_utf8, void* user_data);

typedef struct {
    fr_state_callback_fn on_state;
    fr_geometry_callback_fn on_geometry;
    fr_quality_callback_fn on_quality;
    fr_clipboard_callback_fn on_clipboard;
    void* user_data;
} fr_session_callbacks;

/* Library initialization */
int32_t fr_mobile_init(void);

/* Session Lifecycle */
int32_t fr_session_create(const char* host_addr, const char* auth_token, fr_session_handle* out_session);
int32_t fr_session_connect(fr_session_handle session);
int32_t fr_session_disconnect(fr_session_handle session);
int32_t fr_session_destroy(fr_session_handle session);

/* Surface attachment for zero-copy presentation */
int32_t fr_session_attach_metal_layer(fr_session_handle session, void* metal_layer);
int32_t fr_session_attach_native_window(fr_session_handle session, void* anative_window);

/* Callbacks registration */
int32_t fr_session_set_callbacks(fr_session_handle session, const fr_session_callbacks* callbacks);

/* Input submission */
int32_t fr_session_send_pointer(fr_session_handle session, int32_t x, int32_t y, uint32_t action, uint32_t button);
int32_t fr_session_send_key(fr_session_handle session, uint32_t keycode, uint32_t action);
int32_t fr_session_send_scroll(fr_session_handle session, int32_t dx, int32_t dy);
int32_t fr_session_send_text(fr_session_handle session, const char* text_utf8);

/* Audio uplink */
int32_t fr_session_set_mic_enabled(fr_session_handle session, bool enabled);

/* Clipboard */
int32_t fr_session_send_clipboard(fr_session_handle session, const char* text_utf8);

/* Diagnostics / Quality query */
int32_t fr_session_get_quality(fr_session_handle session, fr_connection_quality* out_quality);

#ifdef __cplusplus
}
#endif

#endif /* FR_MOBILE_H */
