/* Cross-process consent exclusion for cooperating FrankenRemote native owners.
 * This is NOT an X11 security sandbox. The selected UID and its local filesystem
 * namespace are trusted, as are the selected X server and arbitrary same-user
 * applications. No configurable path, environment lookup, deletion or contents. */
#define _GNU_SOURCE
#include "input_gate.h"
#include <sys/file.h>
#include <sys/stat.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <inttypes.h>
#include <stdio.h>
#include <string.h>

/* Same accepted :N[.S] grammar as the native owners. All screens of a server
 * share a gate. Leading zeroes cannot create aliases that bypass exclusion. */
static int number(const char **p, unsigned *value) {
    unsigned count = 0, result = 0;
    while (**p >= '0' && **p <= '9') {
        unsigned digit = (unsigned)(*(*p)++ - '0');
        if (++count > 31 || result > (65535u - digit) / 10u) return 0;
        result = result * 10 + digit;
    }
    *value = result;
    return count != 0;
}
static int private_directory(int fd, uid_t uid) {
    struct stat s;
    return fstat(fd, &s) == 0 && S_ISDIR(s.st_mode) && s.st_uid == uid &&
           (s.st_mode & 07777) == 0700;
}
static int gate_file(int fd) {
    struct stat s;
    return fstat(fd, &s) == 0 && S_ISREG(s.st_mode) && s.st_uid == getuid() &&
           (s.st_mode & 07777) == 0600 && s.st_nlink == 1 && s.st_size == 0;
}
int fr_input_gate_open(const char *display) {
    unsigned server, screen;
    if (!display || strnlen(display, 33) > 32 || *display++ != ':' ||
        !number(&display, &server)) return -1;
    if (*display == '.') { ++display; if (!number(&display, &screen)) return -1; }
    if (*display || getuid() != geteuid()) return -1;
    /* The fixed private directory lives alongside local X11 socket naming.
     * PrivateTmp deployments must expose the SAME namespace to UI and executor;
     * do not claim cross-container or cross-UID coordination. */
    int tmp = open("/tmp", O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC);
    if (tmp < 0) return -1;
    struct stat parent;
    if (fstat(tmp, &parent) || parent.st_uid != 0 ||
        ((parent.st_mode & 0022) && !(parent.st_mode & S_ISVTX))) {
        close(tmp); return -1;
    }
    char directory[64], file[32];
    int n = snprintf(directory, sizeof(directory), "frankenremote-consent-%ju", (uintmax_t)getuid());
    int m = snprintf(file, sizeof(file), "display-%u.lock", server);
    if (n < 0 || (size_t)n >= sizeof(directory) || m < 0 || (size_t)m >= sizeof(file)) {
        close(tmp); return -1;
    }
    if (mkdirat(tmp, directory, 0700) && errno != EEXIST) { close(tmp); return -1; }
    int dir = openat(tmp, directory, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC);
    close(tmp);
    if (dir < 0) return -1;
    if (!private_directory(dir, getuid())) { close(dir); return -1; }
    int fd = openat(dir, file, O_RDWR | O_CREAT | O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK, 0600);
    close(dir);
    if (fd < 0) return -1;
    if (!gate_file(fd)) { close(fd); return -1; }
    return fd;
}
int fr_input_gate_lock(int fd, int exclusive) {
    if (!gate_file(fd)) return -1;
    if (flock(fd, (exclusive ? LOCK_EX : LOCK_SH) | LOCK_NB) == 0) return 1;
    return errno == EWOULDBLOCK || errno == EAGAIN ? 0 : -1;
}
int fr_input_gate_unlock(int fd) { return flock(fd, LOCK_UN | LOCK_NB) == 0; }
void fr_input_gate_close(int fd) { if (fd >= 0) close(fd); }
