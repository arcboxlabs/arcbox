// macOS RLIMIT_NOFILE rules, and how many concurrent HV vsock connections each
// placement of the daemon end allows (docs/logs/2026-11-02-daemon-nofile-limit.md).
//
// A "connection" is what HvMachine::connect_vsock_hv does to the daemon end:
// socketpair, F_DUPFD_CLOEXEC to a position derived from the host port (a
// counter from 50000), close the original. The probe holds up to 200 of them.
//
// Build and run: clang -O1 -o /tmp/nofile-probe probe.c && /tmp/nofile-probe
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/sysctl.h>
#include <unistd.h>

static rlim_t soft_limit(void) {
    struct rlimit rl;
    getrlimit(RLIMIT_NOFILE, &rl);
    return rl.rlim_cur;
}

static void set_soft(rlim_t cur, const char *label) {
    struct rlimit rl;
    getrlimit(RLIMIT_NOFILE, &rl);
    rl.rlim_cur = cur;
    int rc = setrlimit(RLIMIT_NOFILE, &rl);
    int e = errno;
    if (label)
        printf("setrlimit soft=%-20llu (%s): %s, reads back %llu\n", (unsigned long long)cur,
               label, rc == 0 ? "ok" : strerror(e), (unsigned long long)soft_limit());
}

// Before 2026-11-02: min_fd = host port, clamped to soft - 64 above 128.
static int old_place(int fd, unsigned port) {
    int limit = (int)soft_limit();
    int min_fd = (int)port;
    if (limit > 128 && min_fd > limit - 64) min_fd = limit - 64;
    return fcntl(fd, F_DUPFD_CLOEXEC, min_fd);
}

// After: a rotating start above a floor of 1024 (or half the limit), then
// the floor, then any free number.
static int new_place(int fd, unsigned port) {
    rlim_t soft = soft_limit();
    int limit = soft > INT_MAX ? INT_MAX : (int)soft;
    int floor = limit / 2 < 1024 ? limit / 2 : 1024;
    int rotation = limit - floor < 1024 ? limit - floor : 1024;
    if (rotation < 1) rotation = 1;
    int mins[3] = {floor + (int)(port % (unsigned)rotation), floor, 0};
    for (int i = 0; i < 3; i++) {
        int d = fcntl(fd, F_DUPFD_CLOEXEC, mins[i]);
        if (d >= 0 || (errno != EMFILE && errno != EINVAL)) return d;
    }
    return -1;
}

static void capacity(const char *name, int (*place)(int, unsigned), unsigned port0) {
    enum { N = 200 };
    int held[N], ok = 0, lo = INT_MAX, hi = -1, err = 0, reused = 0, last_closed = -1;
    // Reuse check first: open, close, open the next port.
    int sp[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sp) == 0) {
        last_closed = place(sp[0], port0);
        close(sp[0]);
        close(sp[1]);
        if (last_closed >= 0) close(last_closed);
    }
    for (int i = 0; i < N; i++) {
        if (socketpair(AF_UNIX, SOCK_STREAM, 0, sp) != 0) { err = errno; break; }
        int d = place(sp[0], port0 + 1 + (unsigned)i);
        int e = errno;
        close(sp[0]);
        close(sp[1]);
        if (d < 0) { err = e; break; }
        if (i == 0 && d == last_closed) reused = 1;
        held[ok++] = d;
        if (d < lo) lo = d;
        if (d > hi) hi = d;
    }
    printf("  %s soft=%-7llu port0=%-6u held %3d/%d (fds %d..%d)%s%s%s\n", name,
           (unsigned long long)soft_limit(), port0, ok, N, ok ? lo : -1, hi,
           reused ? ", reused the fd just closed" : "", err ? ", then: " : "",
           err ? strerror(err) : "");
    for (int i = 0; i < ok; i++) close(held[i]);
}

int main(void) {
    int perproc = 0, maxfiles = 0;
    size_t len = sizeof(perproc);
    sysctlbyname("kern.maxfilesperproc", &perproc, &len, NULL, 0);
    len = sizeof(maxfiles);
    sysctlbyname("kern.maxfiles", &maxfiles, &len, NULL, 0);
    struct rlimit rl;
    getrlimit(RLIMIT_NOFILE, &rl);
    printf("uid=%d kern.maxfiles=%d kern.maxfilesperproc=%d OPEN_MAX=%d\n", getuid(), maxfiles,
           perproc, OPEN_MAX);
    printf("inherited soft=%llu hard=%llu%s\n", (unsigned long long)rl.rlim_cur,
           (unsigned long long)rl.rlim_max, rl.rlim_max == RLIM_INFINITY ? " (RLIM_INFINITY)" : "");

    puts("-- setrlimit above the per-process ceiling");
    set_soft(RLIM_INFINITY, "RLIM_INFINITY");
    set_soft((rlim_t)perproc + 1, "maxfilesperproc+1");
    set_soft((rlim_t)OPEN_MAX + 1, "OPEN_MAX+1");
    set_soft((rlim_t)perproc, "maxfilesperproc");
    set_soft(RLIM_INFINITY, NULL);
    int below = fcntl(0, F_DUPFD_CLOEXEC, perproc - 1);
    printf("soft=RLIM_INFINITY: F_DUPFD at maxfilesperproc-1: %s\n",
           below >= 0 ? "ok" : strerror(errno));
    if (below >= 0) close(below);
    int at = fcntl(0, F_DUPFD_CLOEXEC, perproc);
    printf("soft=RLIM_INFINITY: F_DUPFD at maxfilesperproc:   %s\n",
           at >= 0 ? "ok" : strerror(errno));

    puts("-- concurrent vsock connections by placement");
    rlim_t limits[] = {256, 2560, 10240, 61440, (rlim_t)perproc};
    for (size_t i = 0; i < sizeof(limits) / sizeof(limits[0]); i++) {
        set_soft(limits[i], NULL);
        capacity("old", old_place, 50000);
        capacity("new", new_place, 50000);
        // A daemon that has made more connections than limit - 50064.
        if (limits[i] == 61440) {
            capacity("old", old_place, 61400);
            capacity("new", new_place, 61400);
        }
    }
    return 0;
}
