/* Checked MIT-SHM 1.2 requests on the ORIGINAL Xlib connection. Xlib keeps
   event ownership. No process-global X error trap, SysV segment, new socket,
   asynchronous image queue or unbounded allocation is introduced. */
#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include "x11_image.h"
#include <X11/Xutil.h>
#include <X11/Xmd.h>
#include <X11/extensions/shmproto.h>
#include <xcb/xcb.h>
#include <xcb/xcbext.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <fcntl.h>
#include <unistd.h>
#include <stdlib.h>
#include <string.h>
#include <limits.h>

/* Public libX11-xcb ABI; this borrows, never closes or takes event ownership. */
extern xcb_connection_t *XGetXCBConnection(Display *);
static xcb_extension_t fr_shm_extension = {"MIT-SHM", 0};
enum { IMAGE_OK=0, IMAGE_INVALID=-1, IMAGE_UNAVAILABLE=-2,
       IMAGE_MEMORY=-3, IMAGE_DISPLAY=-6 };
struct FrXImageTransfer {
    Display *display;
    XImage *image;
    xcb_connection_t *connection;
    uint32_t segment;
    int capture, failed, ready;
    size_t length;
    FrXImageStats stats;
};

/* Xproto's fixed structures are the system wire ABI. xcb supplies the major
   opcode/length, checked error routing and descriptor transport. Two preceding
   iovecs are reserved as required by xcb_send_request. All sizes are 4-aligned. */
static unsigned int fr_shm_request(xcb_connection_t *c, uint8_t opcode,
                                  int is_void, void *body, size_t size, int fd) {
    struct iovec parts[3] = {{0}, {0}, {body, size}};
    xcb_protocol_request_t request = {1, &fr_shm_extension, opcode, is_void};
    if (fd >= 0) {
        /* xcb takes ownership even if the connection is in error. */
        return xcb_send_request_with_fds(c, XCB_REQUEST_CHECKED, parts+2, &request, 1, &fd);
    }
    return xcb_send_request(c, XCB_REQUEST_CHECKED, parts+2, &request);
}
static int fr_shm_checked(xcb_connection_t *c, unsigned int sequence) {
    if (!sequence) return 0;
    xcb_generic_error_t *error = xcb_request_check(c, (xcb_void_cookie_t){sequence});
    int ok = !error && !xcb_connection_has_error(c);
    free(error);
    return ok;
}
static int fr_shm_version(FrXImageTransfer *t) {
    const xcb_query_extension_reply_t *extension =
        xcb_get_extension_data(t->connection, &fr_shm_extension);
    if (!extension || !extension->present) return 1;
    struct sockaddr_storage address;
    socklen_t length = sizeof(address);
    if (getsockname(ConnectionNumber(t->display), (struct sockaddr *)&address, &length) != 0 ||
        address.ss_family != AF_UNIX) return 2;
    xShmQueryVersionReq request = {0};
    unsigned int seq = fr_shm_request(t->connection, X_ShmQueryVersion, 0,
                                      &request, sizeof(request), -1);
    if (!seq) return 2;
    xcb_generic_error_t *error = NULL;
    xShmQueryVersionReply *reply = xcb_wait_for_reply(t->connection, seq, &error);
    int supported = !error && reply && reply->majorVersion == 1 && reply->minorVersion >= 2;
    free(error); free(reply);
    return supported ? 0 : 2;
}
static int fr_shm_allocate(FrXImageTransfer *t) {
    int fd = memfd_create("fr-x11-image", MFD_CLOEXEC | MFD_ALLOW_SEALING);
    if (fd < 0) return 3;
    if (ftruncate(fd, (off_t)t->length) != 0 ||
        fcntl(fd, F_ADD_SEALS, F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_SEAL) != 0) {
        close(fd); return 3;
    }
    void *data = mmap(NULL, t->length, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (data == MAP_FAILED) { close(fd); return 3; }
    uint32_t segment = xcb_generate_id(t->connection);
    if (segment == UINT32_MAX || segment == 0) {
        munmap(data, t->length); close(fd); return 4;
    }
    xShmAttachFdReq request = {0};
    request.shmseg = segment;
    request.readOnly = !t->capture;
    unsigned int seq = fr_shm_request(t->connection, X_ShmAttachFd, 1,
                                      &request, sizeof(request), fd);
    if (!fr_shm_checked(t->connection, seq)) {
        munmap(data, t->length); return 4;
    }
    t->segment = segment;
    t->image->data = data;
    t->stats.path = 1;
    t->stats.retained_bytes = t->length;
    return 0;
}
int fr_ximage_new(Display *display, Visual *visual, int depth, int width, int height,
                  int capture, FrXImageTransfer **out) {
    if (!out) return IMAGE_INVALID;
    *out = NULL;
    if (!display || !visual || depth != 24 || width < 16 || height < 16 ||
        width > 8192 || height > 8192 || (int64_t)width*height > 16777216 ||
        (width & 1) || (height & 1) || (capture != 0 && capture != 1) ||
        visual->class != TrueColor || visual->red_mask != 0xff0000 ||
        visual->green_mask != 0xff00 || visual->blue_mask != 0xff)
        return IMAGE_INVALID;
    FrXImageTransfer *t = calloc(1, sizeof(*t));
    if (!t) return IMAGE_MEMORY;
    t->display = display;
    t->capture = capture;
    t->image = XCreateImage(display, visual, depth, ZPixmap, 0, NULL, width, height, 32, 0);
    if (!t->image) { free(t); return IMAGE_MEMORY; }
    if (t->image->bits_per_pixel != 32 || t->image->byte_order != LSBFirst ||
        t->image->bytes_per_line != width*4) {
        fr_ximage_free(t); return IMAGE_UNAVAILABLE;
    }
    /* Widen before multiplication, then check native representation before mmap. */
    uint64_t bytes = (uint64_t)t->image->bytes_per_line * (uint64_t)height;
    if (!bytes || bytes > 67108864 || bytes > SIZE_MAX || bytes > INT64_MAX) {
        fr_ximage_free(t); return IMAGE_INVALID;
    }
    t->length = (size_t)bytes;
    t->stats.path = 2;
    XFlush(display);
    t->connection = XGetXCBConnection(display);
    int fallback = t->connection ? fr_shm_version(t) : 2;
    if (!fallback) fallback = fr_shm_allocate(t);
    t->stats.fallback = (uint32_t)fallback;
    if (t->connection && xcb_connection_has_error(t->connection)) {
        fr_ximage_free(t); return IMAGE_DISPLAY;
    }
    if (!capture && !t->image->data) {
        t->image->data = calloc(1, t->length);
        if (!t->image->data) { fr_ximage_free(t); return IMAGE_MEMORY; }
        t->stats.retained_bytes = t->length;
    }
    *out = t;
    return IMAGE_OK;
}
void fr_ximage_free(FrXImageTransfer *t) {
    if (!t) return;
    if (t->segment) {
        /* All image operations below are synchronous. Detach is checked before
           unmapping, so the server cannot keep using a recycled client buffer. */
        XFlush(t->display);
        xShmDetachReq request = {0};
        request.shmseg = t->segment;
        (void)fr_shm_checked(t->connection, fr_shm_request(t->connection, X_ShmDetach,
                              1, &request, sizeof(request), -1));
        munmap(t->image->data, t->length);
        t->image->data = NULL; /* XDestroyImage must never free mmap memory. */
    }
    if (t->image) XDestroyImage(t->image);
    free(t);
}
static int fr_ximage_layout(const FrXImageTransfer *t, const XImage *image) {
    return image && image->data && image->width == t->image->width &&
        image->height == t->image->height && image->depth == 24 &&
        image->bits_per_pixel == 32 && image->byte_order == LSBFirst &&
        image->bytes_per_line == t->image->bytes_per_line;
}
static void fr_image_count(uint64_t *value, uint64_t increment) {
    *value = UINT64_MAX - *value < increment ? UINT64_MAX : *value + increment;
}
int fr_ximage_capture(FrXImageTransfer *t, Drawable drawable, int x, int y,
                     uint8_t *out, size_t length) {
    if (!t || !t->capture || t->failed || !out || length != t->length ||
        x < 0 || y < 0 || x > INT16_MAX || y > INT16_MAX) return IMAGE_INVALID;
    XImage *image = t->image;
    XFlush(t->display);
    if (t->segment) {
        xShmGetImageReq request = {0};
        request.drawable = (uint32_t)drawable;
        request.x = (int16_t)x; request.y = (int16_t)y;
        request.width = (uint16_t)image->width; request.height = (uint16_t)image->height;
        request.planeMask = UINT32_MAX; request.format = ZPixmap;
        request.shmseg = t->segment;
        unsigned int seq = fr_shm_request(t->connection, X_ShmGetImage, 0,
                                          &request, sizeof(request), -1);
        xcb_generic_error_t *error = NULL;
        xShmGetImageReply *reply = seq ? xcb_wait_for_reply(t->connection, seq, &error) : NULL;
        int ok = !error && reply && reply->depth == 24 && reply->size == length;
        free(error); free(reply);
        if (!ok) { t->failed = 1; return IMAGE_DISPLAY; }
    } else {
        image = XGetImage(t->display, drawable, x, y, image->width, image->height, AllPlanes, ZPixmap);
        if (!fr_ximage_layout(t, image)) {
            if (image) XDestroyImage(image);
            t->failed = 1; return IMAGE_DISPLAY;
        }
        fr_image_count(&t->stats.socket_pixel_bytes, length);
    }
    memcpy(out, image->data, length);
    /* Depth-24 padding is not pixel data, and must not leak native memory. */
    for (size_t i = 3; i < length; i += 4) out[i] = 255;
    if (!t->segment) XDestroyImage(image);
    fr_image_count(&t->stats.images, 1);
    fr_image_count(&t->stats.copied_bytes, length);
    return IMAGE_OK;
}
/* Only new pictures copy into the one owned buffer. Idle expose repair draws
   it again without another copy or any source/decode freshness assertion. */
int fr_ximage_store(FrXImageTransfer *t, const uint8_t *pixels, size_t length) {
    if (!t || t->capture || t->failed || !pixels || length != t->length ||
        !t->image->data) return IMAGE_INVALID;
    memcpy(t->image->data, pixels, length);
    t->ready = 1;
    fr_image_count(&t->stats.copied_bytes, length);
    return IMAGE_OK;
}
int fr_ximage_ready(const FrXImageTransfer *t) {
    return t && !t->capture && !t->failed && t->ready;
}
int fr_ximage_draw(FrXImageTransfer *t, Drawable drawable, GC gc) {
    if (!fr_ximage_ready(t) || !gc) return IMAGE_INVALID;
    XImage *image = t->image;
    XFlushGC(t->display, gc);
    XFlush(t->display);
    if (t->segment) {
        xShmPutImageReq request = {0};
        request.drawable = (uint32_t)drawable;
        request.gc = (uint32_t)XGContextFromGC(gc);
        request.totalWidth = request.srcWidth = (uint16_t)image->width;
        request.totalHeight = request.srcHeight = (uint16_t)image->height;
        request.depth = 24; request.format = ZPixmap;
        request.shmseg = t->segment;
        if (!fr_shm_checked(t->connection, fr_shm_request(t->connection,
                X_ShmPutImage, 1, &request, sizeof(request), -1))) {
            t->failed = 1; return IMAGE_DISPLAY;
        }
    } else {
        XPutImage(t->display, drawable, gc, image, 0, 0, 0, 0, image->width, image->height);
        XSync(t->display, False);
        fr_image_count(&t->stats.socket_pixel_bytes, t->length);
    }
    /* Checked request completion (or the socket path's XSync) precedes every
       reuse, idle repaint, detach and unmap. Never overwrite an in-flight image.
       This is server submission completion, not compositor/scanout visibility. */
    fr_image_count(&t->stats.images, 1);
    return IMAGE_OK;
}
void fr_ximage_stats(const FrXImageTransfer *t, FrXImageStats *out) {
    if (!out) return;
    memset(out, 0, sizeof(*out));
    if (t) *out = t->stats;
}
