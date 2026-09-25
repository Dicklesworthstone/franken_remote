/* One thread-confined image buffer, borrowing its original Display until free.
   No pixels, descriptors or X resources escape this native boundary. */
#ifndef FR_X11_IMAGE_H
#define FR_X11_IMAGE_H
#include <stdint.h>
#include <stddef.h>
#include <X11/Xlib.h>
typedef struct FrXImageTransfer FrXImageTransfer;
typedef struct {
    uint32_t path, fallback;
    uint64_t images, copied_bytes, socket_pixel_bytes, retained_bytes;
} FrXImageStats;
/* path: 1 fd-backed MIT-SHM, 2 socket. fallback: 0 none, 1 no extension,
   2 no fd-capable extension/local socket, 3 local allocation, 4 rejected attach.
   Counts describe this boundary only, not codec/GPU or server-internal copies. */
int fr_ximage_new(Display *, Visual *, int depth, int width, int height,
                  int capture, FrXImageTransfer **);
void fr_ximage_free(FrXImageTransfer *);
int fr_ximage_capture(FrXImageTransfer *, Drawable, int x, int y, uint8_t *, size_t);
void fr_ximage_stats(const FrXImageTransfer *, FrXImageStats *);
#endif
