#define _GNU_SOURCE
#include <err.h>
#include <errno.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

// Linux encodes current-thread CPUCLOCK_PROF as (~0U << 3) | 4.
// This clock reads raw ticks only when generic virtual accounting is inactive.
static const clockid_t raw_prof = (clockid_t)-4;

static uint64_t nanoseconds(struct timespec value) {
    return (uint64_t)value.tv_sec * 1000000000 + value.tv_nsec;
}

static uint64_t now(clockid_t clock) {
    struct timespec value;
    if (clock_gettime(clock, &value) != 0)
        err(1, "clock_gettime(%d)", clock);
    return nanoseconds(value);
}

static unsigned argument(const char *text, unsigned maximum) {
    char *end;
    errno = 0;
    unsigned long value = strtoul(text, &end, 10);
    if (errno || *end || end == text || value > maximum)
        errx(1, "invalid argument: %s", text);
    return (unsigned)value;
}

int main(int argc, char **argv) {
    if (argc != 4 || (strcmp(argv[3], "busy") && strcmp(argv[3], "sleep")))
        errx(1, "usage: thread_accounting CPU SECONDS busy|sleep");
    unsigned cpu = argument(argv[1], CPU_SETSIZE - 1);
    unsigned seconds = argument(argv[2], 300);
    if (!seconds)
        errx(1, "SECONDS must be positive");
    cpu_set_t cpus;
    CPU_ZERO(&cpus);
    CPU_SET(cpu, &cpus);
    if (sched_setaffinity(0, sizeof(cpus), &cpus) != 0)
        err(1, "sched_setaffinity");
    struct timespec resolution;
    if (clock_getres(raw_prof, &resolution) != 0)
        err(1, "clock_getres");
    printf("{\"pid\":%d,\"cpu\":%u,\"mode\":\"%s\",\"prof_resolution_ns\":%llu}\n", getpid(), cpu,
           argv[3], (unsigned long long)nanoseconds(resolution));
    uint64_t start = now(CLOCK_MONOTONIC);
    uint64_t initial_sched = now(CLOCK_THREAD_CPUTIME_ID);
    uint64_t initial_prof = now(raw_prof);
    uint64_t next = start;
    volatile uint64_t work = 1;
    for (;;) {
        uint64_t before = now(CLOCK_MONOTONIC);
        if (before >= next) {
            uint64_t runtime = now(CLOCK_THREAD_CPUTIME_ID) - initial_sched;
            uint64_t prof = now(raw_prof) - initial_prof;
            uint64_t after = now(CLOCK_MONOTONIC);
            printf("{\"elapsed_ns\":%llu,\"sched_ns\":%llu,\"prof_ns\":%llu,"
                   "\"read_span_ns\":%llu}\n",
                   (unsigned long long)(after - start), (unsigned long long)runtime,
                   (unsigned long long)prof, (unsigned long long)(after - before));
            fflush(stdout);
            if (after - start >= (uint64_t)seconds * 1000000000)
                break;
            next = after + 1000000000;
        }
        if (!strcmp(argv[3], "sleep")) {
            struct timespec delay = {.tv_sec = 0, .tv_nsec = 10000000};
            if (nanosleep(&delay, NULL) != 0)
                err(1, "nanosleep");
        } else {
            for (unsigned i = 0; i < 100000; ++i)
                work = work * 6364136223846793005ULL + 1;
        }
    }
    return 0;
}
