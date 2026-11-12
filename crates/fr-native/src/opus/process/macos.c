/* Private macOS Opus child: same-user process containment, NOT an OS sandbox.
 * No device, certificate or input capability is inherited. The fixed IPC and
 * parent supervisor bound codec state, packets, work and restart population.
 * A separate kernel parent-exit event ends even a decoder stuck in native code.
 * The process still trusts its OS/user and statically linked codec package. */
#include <errno.h>
#include <libproc.h>
#include <limits.h>
#include <pthread.h>
#include <stdint.h>
#include <stdlib.h>
#include <sys/event.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/types.h>
#include <sys/un.h>
#include <unistd.h>

/* Darwin has no closefrom(3). Enumerate the actual descriptor table instead of
 * a possibly lowered RLIMIT_NOFILE, before starting the only helper thread.
 * This is a bounded private child: a full/truncated or malformed inventory is
 * refusal, never permission to retain an uninspected inherited descriptor. */
#define FR_OPUS_INHERITED_FDS 1024
static int close_inherited(void) {
    struct proc_fdinfo descriptors[FR_OPUS_INHERITED_FDS];
    int size = (int)sizeof(descriptors);
    int bytes = proc_pidinfo(getpid(), PROC_PIDLISTFDS, 0, descriptors, size);
    if (bytes <= 0 || bytes >= size || bytes % (int)sizeof(descriptors[0])) return 0;
    int count = bytes / (int)sizeof(descriptors[0]);
    for (int i = 0; i < count; ++i) {
        int descriptor = descriptors[i].proc_fd;
        if (descriptor < 0) return 0;
        if (descriptor >= 3 && close(descriptor) < 0) return 0;
    }
    /* No other child thread may open a descriptor between inventory and proof.
     * If runtime initialization violated that contract, refuse before libopus. */
    bytes = proc_pidinfo(getpid(), PROC_PIDLISTFDS, 0, descriptors, size);
    if (bytes <= 0 || bytes >= size || bytes % (int)sizeof(descriptors[0])) return 0;
    count = bytes / (int)sizeof(descriptors[0]);
    for (int i = 0; i < count; ++i) {
        if (descriptors[i].proc_fd < 0 || descriptors[i].proc_fd >= 3) return 0;
    }
    return 1;
}

static void *parent_exited(void *context) {
    int queue = (int)(intptr_t)context;
    struct kevent event;
    int count;
    do {
        count = kevent(queue, NULL, 0, &event, 1, NULL);
    } while (count < 0 && errno == EINTR);
    /* An unavailable watcher is as terminal as the expected parent's exit. */
    _exit(70);
}

int fr_opus_macos_prepare(uint32_t parent) {
    if (parent <= 1 || parent > INT_MAX || getppid() != (pid_t)parent ||
        getuid() != geteuid() || getgid() != getegid()) return 0;
    for (int descriptor = 0; descriptor < 2; ++descriptor) {
        int type = 0;
        socklen_t size = sizeof(type);
        uid_t user;
        gid_t group;
        pid_t peer = 0;
        socklen_t peer_size = sizeof(peer);
        if (getsockopt(descriptor, SOL_SOCKET, SO_TYPE, &type, &size) ||
            size != sizeof(type) || type != SOCK_STREAM ||
            getpeereid(descriptor, &user, &group) || user != getuid() ||
            getsockopt(descriptor, SOL_LOCAL, LOCAL_PEERPID, &peer, &peer_size) ||
            peer_size != sizeof(peer) || peer != (pid_t)parent) return 0;
    }
    struct rlimit zero = {0, 0};
    if (setrlimit(RLIMIT_CORE, &zero) || setrlimit(RLIMIT_FSIZE, &zero)) return 0;
    /* The watcher is created only after all unrelated inherited fds close. */
    if (!close_inherited()) return 0;
    int queue = kqueue();
    if (queue < 0) return 0;
    struct kevent watch;
    EV_SET(&watch, parent, EVFILT_PROC, EV_ADD | EV_ONESHOT, NOTE_EXIT, 0, NULL);
    if (kevent(queue, &watch, 1, NULL, 0, NULL) < 0 ||
        getppid() != (pid_t)parent) {
        close(queue);
        return 0;
    }
    pthread_attr_t attributes;
    if (pthread_attr_init(&attributes)) {
        close(queue);
        return 0;
    }
    pthread_t thread;
    int result = pthread_attr_setdetachstate(&attributes, PTHREAD_CREATE_DETACHED);
    if (!result) result = pthread_create(&thread, &attributes, parent_exited,
                                         (void *)(intptr_t)queue);
    pthread_attr_destroy(&attributes);
    if (result) {
        close(queue);
        return 0;
    }
    return getppid() == (pid_t)parent;
}
