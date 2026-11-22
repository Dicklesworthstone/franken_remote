/* Shared construction boundary for project-owned XCB and Xlib connections. */
#ifndef FR_X11_CONNECTION_H
#define FR_X11_CONNECTION_H

#include <xcb/xcb.h>

xcb_connection_t *fr_xcb_connect(const char *display, int *screen);

/* Defined only when an enabled native feature already requires Xlib. */
struct _XDisplay;
struct _XDisplay *fr_x11_open_display(const char *display);

#endif
