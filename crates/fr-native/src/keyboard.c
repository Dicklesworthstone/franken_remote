/* Narrow XKB ABI access. All handles are borrowed from the thread-confined
 * Rust input owner; this file owns no display, authority, queue or key policy. */
#include <X11/Xlib.h>
#include <X11/XKBlib.h>
#include <string.h>
extern int XTestFakeKeyEvent(Display *, unsigned int, Bool, unsigned long);
int fr_key_probe(void *display) {
    int op, event, error, major = XkbMajorVersion, minor = XkbMinorVersion;
    return XkbQueryExtension(display, &op, &event, &error, &major, &minor);
}
int fr_key_code(void *display, const unsigned char name[4]) {
    XkbDescPtr kb = XkbGetMap(display, 0, XkbUseCoreKbd);
    if (!kb) return 0;
    int found = 0;
    if (XkbGetNames(display, XkbKeyNamesMask, kb) == Success && kb->names && kb->names->keys) {
        for (unsigned int k = kb->min_key_code; k <= kb->max_key_code; ++k) {
            if (memcmp(kb->names->keys[k].name, name, 4) == 0) {
                if (found) { found = 0; break; }
                found = (int)k;
            }
        }
    }
    XkbFreeKeyboard(kb, XkbAllComponentsMask, True);
    return found;
}
int fr_key_down(void *display, unsigned int code) {
    if (code < 8 || code > 255) return -1;
    char keys[32];
    if (!XQueryKeymap(display, keys)) return -1;
    return (((unsigned char *)keys)[code / 8] >> (code % 8)) & 1;
}
int fr_key_repeat(void *display, unsigned int code, int mode) {
    if (code < 8 || code > 255 || mode < -1 || mode > 1) return -1;
    if (mode >= 0) {
        XKeyboardControl control;
        memset(&control, 0, sizeof(control));
        control.key = (int)code;
        control.auto_repeat_mode = mode ? AutoRepeatModeOn : AutoRepeatModeOff;
        XChangeKeyboardControl(display, KBKey | KBAutoRepeatMode, &control);
    }
    XKeyboardState state;
    if (!XGetKeyboardControl(display, &state)) return -1;
    return (((unsigned char *)state.auto_repeats)[code / 8] >> (code % 8)) & 1;
}
int fr_key_event(void *display, unsigned int code, int pressed) {
    if (code < 8 || code > 255 || (pressed != 0 && pressed != 1)) return 0;
    int ok = XTestFakeKeyEvent(display, code, pressed, CurrentTime);
    XFlush(display);
    return ok;
}
