/*
 * Compile-time check that include/fr_mobile.h describes the Rust ABI
 * (src/ffi.rs, src/events.rs). build.rs compiles this with -Werror for every
 * Apple target; ffi/tests.rs asserts the same sizes and offsets from Rust.
 */
#include "fr_mobile.h"

#include <stddef.h>

_Static_assert(sizeof(FrEvent) == 32, "FrEvent size");
_Static_assert(offsetof(FrEvent, picture_height) == 24, "FrEvent layout");
_Static_assert(sizeof(FrFrameInfo) == 32, "FrFrameInfo size");
_Static_assert(offsetof(FrFrameInfo, full_range) == 24, "FrFrameInfo layout");
_Static_assert(sizeof(FrStats) == 56, "FrStats size");
_Static_assert(offsetof(FrStats, hardware) == 40, "FrStats layout");
_Static_assert(offsetof(FrStats, end) == 48, "FrStats layout");
_Static_assert(sizeof(FrCursor) == 32, "FrCursor size");
_Static_assert(offsetof(FrCursor, visible) == 24, "FrCursor layout");
_Static_assert(sizeof(FrConfig) == 32, "FrConfig size");
_Static_assert(offsetof(FrConfig, port) == 8, "FrConfig layout");
_Static_assert(offsetof(FrConfig, control) == 10, "FrConfig layout");
_Static_assert(offsetof(FrConfig, direct) == 11, "FrConfig layout");
_Static_assert(offsetof(FrConfig, pin) == 16, "FrConfig layout");
_Static_assert(offsetof(FrConfig, picture_width) == 24, "FrConfig layout");
_Static_assert(offsetof(FrConfig, picture_height) == 28, "FrConfig layout");
_Static_assert(sizeof(bool) == 1, "C bool is Rust bool");
_Static_assert(sizeof(FrHandle) == 8, "FrHandle is u64");
_Static_assert(sizeof(FrDisplayRequest) == 16, "FrDisplayRequest size");
_Static_assert(offsetof(FrDisplayRequest, count) == 8, "FrDisplayRequest layout");
_Static_assert(sizeof(FrDisplayInfo) == 40, "FrDisplayInfo size");
_Static_assert(offsetof(FrDisplayInfo, x) == 16, "FrDisplayInfo layout");
_Static_assert(offsetof(FrDisplayInfo, scale_numerator) == 24, "FrDisplayInfo layout");
_Static_assert(offsetof(FrDisplayInfo, rotation) == 32, "FrDisplayInfo layout");

_Static_assert(_Generic(&fr_session_start_with_display,
    int32_t (*_Nonnull)(const FrConfig *_Nullable, uint32_t, FrHandle *_Nullable): 1, default: 0),
    "fr_session_start_with_display signature");
_Static_assert(_Generic(&fr_session_display_request,
    bool (*_Nonnull)(FrHandle, FrDisplayRequest *_Nullable): 1, default: 0),
    "fr_session_display_request signature");
_Static_assert(_Generic(&fr_session_display_info,
    bool (*_Nonnull)(FrHandle, uint64_t, uint32_t, FrDisplayInfo *_Nullable): 1, default: 0),
    "fr_session_display_info signature");
_Static_assert(_Generic(&fr_session_select_display,
    int32_t (*_Nonnull)(FrHandle, uint64_t, uint32_t): 1, default: 0),
    "fr_session_select_display signature");
_Static_assert(_Generic(&fr_session_cancel_display,
    int32_t (*_Nonnull)(FrHandle, uint64_t): 1, default: 0),
    "fr_session_cancel_display signature");
_Static_assert(_Generic(&fr_session_frame_display,
    bool (*_Nonnull)(FrHandle, uint64_t, uint32_t *_Nullable, uint32_t *_Nullable): 1, default: 0),
    "fr_session_frame_display signature");
_Static_assert(_Generic(&fr_session_reopen_status,
    int32_t (*_Nonnull)(FrHandle): 1, default: 0),
    "fr_session_reopen_status signature");

/* Captured typing has no new structs. Pin its scalar and pointer signatures
 * too, so a header edit cannot silently change what Swift passes to ffi.rs. */
_Static_assert(_Generic(&fr_session_begin_typing,
    int32_t (*_Nonnull)(FrHandle, uint64_t *_Nullable): 1, default: 0),
    "fr_session_begin_typing signature");
_Static_assert(_Generic(&fr_session_type_captured,
    int32_t (*_Nonnull)(FrHandle, uint64_t, uint32_t, uint16_t, uint8_t): 1, default: 0),
    "fr_session_type_captured signature");
_Static_assert(_Generic(&fr_session_end_typing,
    void (*_Nonnull)(FrHandle, uint64_t): 1, default: 0),
    "fr_session_end_typing signature");
_Static_assert(_Generic(&fr_session_supports_text,
    bool (*_Nonnull)(FrHandle): 1, default: 0),
    "fr_session_supports_text signature");
