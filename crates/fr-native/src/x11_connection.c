/* libXau's XauFileName uses process-global malloc/free-backed filename storage:
 * https://github.com/NetBSD/xsrc/blob/trunk/external/mit/libXau/dist/AuFileName.c
 * libxcb's authentication lookup does not serialize access to that storage.
 * Xlib thread initialization does not cover separate direct XCB constructors.
 *
 * One mutex, shared by every project-owned XCB/Xlib constructor, protects only
 * connection setup. It never surrounds event processing, input submission,
 * authority checks or the lifetime of a display. Callers still own and close
 * their connections on their original native threads. External toolkit users
 * must not open displays concurrently outside this construction boundary. */
#include "x11_connection.h"
#include <pthread.h>
#include <stddef.h>
#ifdef FR_NATIVE_XLIB
#include <X11/Xlib.h>
#endif

static pthread_mutex_t connection_mutex = PTHREAD_MUTEX_INITIALIZER;

xcb_connection_t *fr_xcb_connect(const char *display, int *screen) {
    if (pthread_mutex_lock(&connection_mutex) != 0) return NULL;
    xcb_connection_t *connection = xcb_connect(display, screen);
    if (pthread_mutex_unlock(&connection_mutex) != 0) {
        if (connection) xcb_disconnect(connection);
        return NULL;
    }
    return connection;
}

#ifdef FR_NATIVE_XLIB
Display *fr_x11_open_display(const char *display) {
    if (pthread_mutex_lock(&connection_mutex) != 0) return NULL;
    Display *connection = XOpenDisplay(display);
    if (pthread_mutex_unlock(&connection_mutex) != 0) {
        if (connection) XCloseDisplay(connection);
        return NULL;
    }
    return connection;
}
#endif
