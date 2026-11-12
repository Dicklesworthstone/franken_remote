/* Actual Darwin boundary fixture, linked with ../src/opus/process/macos.c.
 * No codec, replacement OS API, or simulated parent-death mechanism is used.
 * Run only in the native Mac lane; every subprocess has a bounded exit path. */
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/event.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

int fr_opus_macos_prepare(uint32_t parent);

static uint64_t now_ms(void) {
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now)) abort();
    return (uint64_t)now.tv_sec * 1000 + (uint64_t)now.tv_nsec / 1000000;
}

static int receive(int descriptor, void *output, size_t length) {
    uint64_t until = now_ms() + 3000;
    unsigned char *bytes = output;
    while (length) {
        uint64_t now = now_ms();
        if (now >= until) return 0;
        struct pollfd poller = {descriptor, POLLIN, 0};
        int ready = poll(&poller, 1, (int)(until - now));
        if (ready < 0 && errno == EINTR) continue;
        if (ready != 1 || (poller.revents & (POLLERR | POLLNVAL))) return 0;
        ssize_t read_count = read(descriptor, bytes, length);
        if (read_count < 0 && errno == EINTR) continue;
        if (read_count <= 0) return 0;
        bytes += (size_t)read_count;
        length -= (size_t)read_count;
    }
    return 1;
}

static int reap(pid_t child, int expected) {
    uint64_t until = now_ms() + 3000;
    int status;
    while (now_ms() < until) {
        pid_t result = waitpid(child, &status, WNOHANG);
        if (result == child) return WIFEXITED(status) && WEXITSTATUS(status) == expected;
        if (result < 0 && errno != EINTR) return 0;
        struct timespec step = {0, 10000000};
        nanosleep(&step, NULL);
    }
    kill(child, SIGKILL);
    until = now_ms() + 3000;
    while (now_ms() < until) {
        pid_t result = waitpid(child, &status, WNOHANG);
        if (result == child || (result < 0 && errno != EINTR)) break;
        struct timespec step = {0, 10000000};
        nanosleep(&step, NULL);
    }
    return 0;
}

static void streams(int socket) {
    if (dup2(socket, STDIN_FILENO) != STDIN_FILENO ||
        dup2(socket, STDOUT_FILENO) != STDOUT_FILENO) _exit(91);
    int sink = open("/dev/null", O_WRONLY);
    if (sink < 0 || dup2(sink, STDERR_FILENO) != STDERR_FILENO) _exit(92);
    /* The production boundary, not this fixture, closes the originals. */
}

static int original_pair_and_inherited_fd(int wrong_parent) {
    int pair[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, pair)) return 0;
    int inherited = fcntl(pair[0], F_DUPFD, 64);
    if (inherited < 0) {
        close(pair[0]); close(pair[1]);
        return 0;
    }
    pid_t child = fork();
    if (child == 0) {
        streams(pair[1]);
        struct rlimit files;
        if (getrlimit(RLIMIT_NOFILE, &files)) _exit(93);
        files.rlim_cur = 32; // fd 64 already exists; a limit-based loop would miss it.
        if (setrlimit(RLIMIT_NOFILE, &files)) _exit(94);
        uint32_t parent = (uint32_t)(wrong_parent ? getpid() : getppid());
        int prepared = fr_opus_macos_prepare(parent);
        if (wrong_parent) _exit(prepared == 0 ? 0 : 95);
        if (prepared != 1) _exit(96);
        errno = 0;
        if (fcntl(inherited, F_GETFD) != -1 || errno != EBADF) _exit(97);
        struct rlimit core, file_size;
        if (getrlimit(RLIMIT_CORE, &core) || getrlimit(RLIMIT_FSIZE, &file_size) ||
            core.rlim_cur || core.rlim_max || file_size.rlim_cur || file_size.rlim_max)
            _exit(98);
        unsigned char ready = 1;
        if (write(STDOUT_FILENO, &ready, 1) != 1) _exit(99);
        _exit(0);
    }
    close(pair[1]);
    close(inherited);
    unsigned char ready = 0;
    int success = child > 0 && (wrong_parent || (receive(pair[0], &ready, 1) && ready == 1));
    close(pair[0]);
    if (child > 0) success = reap(child, 0) && success;
    return success;
}

/* The intermediate parent creates the real socketpair, just like the Rust
 * supervisor. The outer test observes both processes without being the codec's
 * peer. Exiting this parent must kill a child which never reads IPC again. */
static void watcher_parent(int report) {
    if (setpgid(0, 0)) _exit(81);
    int pair[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, pair)) _exit(82);
    pid_t child = fork();
    if (child == 0) {
        streams(pair[1]);
        if (fr_opus_macos_prepare((uint32_t)getppid()) != 1) _exit(83);
        unsigned char ready = 1;
        if (write(STDOUT_FILENO, &ready, 1) != 1) _exit(84);
        for (;;) pause(); // The codec/main thread deliberately makes no progress.
    }
    close(pair[1]);
    unsigned char ready = 0;
    if (child <= 0 || !receive(pair[0], &ready, 1) || ready != 1 ||
        write(report, &child, sizeof(child)) != (ssize_t)sizeof(child)) _exit(85);
    unsigned char exit_now = 0;
    if (!receive(report, &exit_now, 1) || exit_now != 1) _exit(86);
    _exit(0);
}

static int parent_exit_stops_hung_child(void) {
    int report[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, report)) return 0;
    pid_t parent = fork();
    if (parent == 0) {
        close(report[0]);
        watcher_parent(report[1]);
    }
    close(report[1]);
    if (parent < 0) { close(report[0]); return 0; }
    // A private group permits bounded cleanup even if the fixture fails early.
    if (setpgid(parent, parent) && getpgid(parent) != parent) {
        kill(-parent, SIGKILL);
        kill(parent, SIGKILL);
        close(report[0]);
        reap(parent, 0);
        return 0;
    }
    pid_t child = 0;
    int queue = -1;
    int success = 0;
    if (!receive(report[0], &child, sizeof(child)) || child <= 1) goto done;
    queue = kqueue();
    if (queue < 0) goto done;
    struct kevent watch, event;
    EV_SET(&watch, child, EVFILT_PROC, EV_ADD | EV_ONESHOT, NOTE_EXIT, 0, NULL);
    if (kevent(queue, &watch, 1, NULL, 0, NULL) < 0) goto done;
    struct timespec immediate = {0, 0};
    if (kevent(queue, NULL, 0, &event, 1, &immediate) != 0) goto done;
    unsigned char exit_now = 1;
    if (write(report[0], &exit_now, 1) != 1) goto done;
    struct timespec timeout = {3, 0};
    int events = kevent(queue, NULL, 0, &event, 1, &timeout);
    success = events == 1 && event.ident == (uintptr_t)child &&
        event.filter == EVFILT_PROC && (event.fflags & NOTE_EXIT) && !(event.flags & EV_ERROR);
done:
    if (queue >= 0) close(queue);
    close(report[0]);
    if (!success) kill(-parent, SIGKILL);
    int parent_reaped = reap(parent, 0);
    // The orphaned decoder is reaped by launchd. NOTE_EXIT, not a mere signal
    // or socket close, is the positive evidence that its watcher terminated it.
    return success && parent_reaped;
}

int main(void) {
    // Broken fixture IPC must report failure, never terminate before cleanup.
    signal(SIGPIPE, SIG_IGN);
    if (!original_pair_and_inherited_fd(0)) {
        fputs("original socketpair / inherited descriptor boundary failed\n", stderr);
        return 1;
    }
    if (!original_pair_and_inherited_fd(1)) {
        fputs("wrong parent was not refused\n", stderr);
        return 1;
    }
    if (!parent_exit_stops_hung_child()) {
        fputs("parent-exit watcher did not terminate its hung child\n", stderr);
        return 1;
    }
    puts("Mac Opus boundary: socketpair, descriptor cleanup, parent refusal and parent exit passed");
    return 0;
}
