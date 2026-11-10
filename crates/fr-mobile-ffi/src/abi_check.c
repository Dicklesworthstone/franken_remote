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
_Static_assert(sizeof(FrConfig) == 24, "FrConfig size");
_Static_assert(offsetof(FrConfig, port) == 8, "FrConfig layout");
_Static_assert(offsetof(FrConfig, control) == 10, "FrConfig layout");
_Static_assert(offsetof(FrConfig, direct) == 11, "FrConfig layout");
_Static_assert(offsetof(FrConfig, pin) == 16, "FrConfig layout");
_Static_assert(sizeof(bool) == 1, "C bool is Rust bool");
_Static_assert(sizeof(FrHandle) == 8, "FrHandle is u64");
