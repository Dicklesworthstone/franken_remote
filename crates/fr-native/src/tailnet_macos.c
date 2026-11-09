/* The installed Tailscale CLI is the macOS discovery bridge (plan 6.2).
 * Spawn directly and suspended: no unverified image executes user-space code.
 * Apple documents dynamic SecCode validation as resistant to image replacement.
 * The unreaped child pins its PID through verification, reads and group cleanup.
 * No LocalAPI token, response text, path or system diagnostic is logged here.
 */
#define _DARWIN_C_SOURCE 1
#include <CoreFoundation/CoreFoundation.h>
#include <Security/Security.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <pwd.h>
#include <signal.h>
#include <spawn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

struct fr_tailnet_snapshot {
    pid_t pid;
    int output;
    int suspended;
};

/* All owners are confined to the caller's drained blocking operation. */
void fr_tailnet_macos_destroy(struct fr_tailnet_snapshot *snapshot) {
    if (snapshot == NULL) return;
    if (snapshot->pid > 0) {
        /* Group identity cannot be reused while our leader remains unreaped.
         * Darwin may refuse signalling an exited zombie; waitpid still reaps it.
         */
        (void)kill(-snapshot->pid, SIGKILL);
        /* A resumed image may have moved groups; its retained PID is still ours. */
        (void)kill(snapshot->pid, SIGKILL);
        int status;
        while (waitpid(snapshot->pid, &status, 0) < 0 && errno == EINTR) {}
    }
    if (snapshot->output >= 0) (void)close(snapshot->output);
    free(snapshot);
}

static int above_stdio(int fd) {
    if (fd < 0 || fd > STDERR_FILENO) return fd;
    int moved = fcntl(fd, F_DUPFD_CLOEXEC, STDERR_FILENO + 1);
    (void)close(fd);
    return moved;
}

static int trusted_child(pid_t pid) {
    /* Team and app IDs are published by Tailscale's MDM documentation and
     * version/prop.go. This accepts either official GUI installation variant,
     * never an arbitrary Apple-signed binary or a similarly named executable.
     */
    static const CFStringRef text = CFSTR(
        "anchor apple generic and certificate leaf[subject.OU] = \"W5364U7YZB\" "
        "and (identifier \"io.tailscale.ipn.macsys\" "
        "or identifier \"io.tailscale.ipn.macos\")");
    CFNumberRef number = CFNumberCreate(NULL, kCFNumberIntType, &pid);
    if (number == NULL) return 0;
    const void *keys[] = {kSecGuestAttributePid};
    const void *values[] = {number};
    CFDictionaryRef attributes = CFDictionaryCreate(NULL, keys, values, 1,
        &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    SecCodeRef code = NULL;
    SecRequirementRef requirement = NULL;
    int trusted = attributes != NULL
        && SecCodeCopyGuestWithAttributes(NULL, attributes, kSecCSDefaultFlags,
                                         &code) == errSecSuccess
        && SecRequirementCreateWithString(text, kSecCSDefaultFlags,
                                          &requirement) == errSecSuccess
        && SecCodeCheckValidity(code, kSecCSDefaultFlags, requirement) == errSecSuccess;
    if (requirement != NULL) CFRelease(requirement);
    if (code != NULL) CFRelease(code);
    if (attributes != NULL) CFRelease(attributes);
    CFRelease(number);
    return trusted;
}

/* The safe Rust entry point supplies only the fixed installed app path. The
 * image argument also permits a macOS-only negative test of actual suspended
 * Apple-signed code; neither caller-controlled argv nor environment is accepted.
 * `peers` selects one of two fixed argument lists: the full status snapshot,
 * or the same snapshot without its peer map (`--peers=false`).
 * Return 1 for unavailable OS resources, 2 for a refused signature, 0 on success.
 * Success deliberately leaves the child suspended until the original caller's
 * cancellation/deadline check authorizes resume. No local budget is restarted.
 */
int fr_tailnet_macos_start(const char *image, int peers,
                          struct fr_tailnet_snapshot **out, pid_t *started) {
    *out = NULL;
    if (started != NULL) *started = 0;
    struct fr_tailnet_snapshot *snapshot = calloc(1, sizeof(*snapshot));
    if (snapshot == NULL) return 1;
    snapshot->output = -1;
    int pipefd[2] = {-1, -1};
    int nullfd = -1, actions_ready = 0, attributes_ready = 0, result = 1;
    posix_spawn_file_actions_t actions;
    posix_spawnattr_t attributes;
    /* Derive HOME from the OS user record, never an inherited override. */
    struct passwd user, *found = NULL;
    char user_buffer[16384], home[PATH_MAX + 6];
    if (getpwuid_r(getuid(), &user, user_buffer, sizeof(user_buffer), &found) != 0
        || found == NULL || user.pw_dir == NULL || user.pw_dir[0] != '/') goto done;
    int home_length = snprintf(home, sizeof(home), "HOME=%s", user.pw_dir);
    if (home_length < 0 || (size_t)home_length >= sizeof(home)) goto done;
    char *const with_peers[] = {(char *)image, "status", "--json", NULL};
    char *const without_peers[] = {(char *)image, "status", "--json", "--peers=false", NULL};
    char *const *argv = peers ? with_peers : without_peers;
    char *const environment[] = {"TAILSCALE_BE_CLI=1", home,
        "PATH=/usr/bin:/bin:/usr/sbin:/sbin", "LANG=C", NULL};
    if (pipe(pipefd) < 0) goto done;
    pipefd[0] = above_stdio(pipefd[0]);
    pipefd[1] = above_stdio(pipefd[1]);
    if (pipefd[0] < 0 || pipefd[1] < 0
        || fcntl(pipefd[0], F_SETFD, FD_CLOEXEC) < 0
        || fcntl(pipefd[1], F_SETFD, FD_CLOEXEC) < 0
        || fcntl(pipefd[0], F_SETFL, O_NONBLOCK) < 0) goto done;
    nullfd = above_stdio(open("/dev/null", O_RDWR | O_CLOEXEC));
    if (nullfd < 0 || posix_spawn_file_actions_init(&actions) != 0) goto done;
    actions_ready = 1;
    if (posix_spawn_file_actions_adddup2(&actions, nullfd, STDIN_FILENO) != 0
        || posix_spawn_file_actions_adddup2(&actions, pipefd[1], STDOUT_FILENO) != 0
        || posix_spawn_file_actions_adddup2(&actions, nullfd, STDERR_FILENO) != 0
        || posix_spawn_file_actions_addchdir_np(&actions, "/") != 0
        || posix_spawnattr_init(&attributes) != 0) goto done;
    attributes_ready = 1;
    sigset_t mask, defaults;
    sigemptyset(&mask);
    sigemptyset(&defaults);
    sigaddset(&defaults, SIGINT);
    sigaddset(&defaults, SIGQUIT);
    sigaddset(&defaults, SIGTERM);
    sigaddset(&defaults, SIGHUP);
    sigaddset(&defaults, SIGPIPE);
    short flags = POSIX_SPAWN_START_SUSPENDED | POSIX_SPAWN_SETPGROUP
        | POSIX_SPAWN_CLOEXEC_DEFAULT | POSIX_SPAWN_SETSIGMASK | POSIX_SPAWN_SETSIGDEF;
    if (posix_spawnattr_setflags(&attributes, flags) != 0
        || posix_spawnattr_setpgroup(&attributes, 0) != 0
        || posix_spawnattr_setsigmask(&attributes, &mask) != 0
        || posix_spawnattr_setsigdefault(&attributes, &defaults) != 0) goto done;
    pid_t spawned = 0;
    if (posix_spawn(&spawned, image, &actions, &attributes, (char *const *)argv,
                    environment) != 0)
        goto done; /* A failed call's unspecified PID output is never owned. */
    snapshot->pid = spawned;
    if (started != NULL) *started = snapshot->pid;
    snapshot->suspended = 1;
    if (!trusted_child(snapshot->pid)) {
        result = 2;
        goto done;
    }
    snapshot->output = pipefd[0];
    pipefd[0] = -1;
    *out = snapshot;
    snapshot = NULL;
    result = 0;
done:
    if (attributes_ready) (void)posix_spawnattr_destroy(&attributes);
    if (actions_ready) (void)posix_spawn_file_actions_destroy(&actions);
    if (nullfd >= 0) (void)close(nullfd);
    if (pipefd[0] >= 0) (void)close(pipefd[0]);
    if (pipefd[1] >= 0) (void)close(pipefd[1]);
    fr_tailnet_macos_destroy(snapshot);
    return result;
}

int fr_tailnet_macos_resume(struct fr_tailnet_snapshot *snapshot) {
    if (!snapshot->suspended || kill(snapshot->pid, SIGCONT) < 0) return -1;
    snapshot->suspended = 0;
    return 0;
}

/* One syscall per step: interrupted/nonready reads yield to the original clock. */
int fr_tailnet_macos_read(struct fr_tailnet_snapshot *snapshot, uint8_t *buffer,
                         size_t capacity, size_t *length) {
    *length = 0;
    if (snapshot->suspended || capacity == 0 || capacity > 65536) return -1;
    ssize_t count = read(snapshot->output, buffer, capacity);
    if (count > 0) {
        *length = (size_t)count;
        return 2;
    }
    if (count == 0) return 1;
    return errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR ? 0 : -1;
}

/* Observe, never reap: cleanup must signal our original group before PID reuse. */
int fr_tailnet_macos_try_wait(struct fr_tailnet_snapshot *snapshot) {
    siginfo_t info = {0};
    if (waitid(P_PID, (id_t)snapshot->pid, &info, WEXITED | WNOHANG | WNOWAIT) < 0)
        return errno == EINTR ? 0 : -1;
    if (info.si_pid == 0) return 0;
    return info.si_code == CLD_EXITED && info.si_status == 0 ? 1 : 2;
}
