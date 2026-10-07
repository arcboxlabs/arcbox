#define _GNU_SOURCE
#include <err.h>
#include <inttypes.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <time.h>
#include <unistd.h>

/* This probe tests profiler sample bias, not the original ClickHouse stall. */
#define SECOND_NS UINT64_C(1000000000)
#define RAW_PROF_CLOCK ((clockid_t) - 4)

static volatile sig_atomic_t real_active;
static volatile sig_atomic_t sched_count, sched_in_real, sched_overrun;
static volatile sig_atomic_t prof_count, prof_in_real, prof_overrun;

static uint64_t now_ns(clockid_t clock) {
    struct timespec ts;
    if (clock_gettime(clock, &ts) != 0)
        _exit(2);
    return (uint64_t)ts.tv_sec * SECOND_NS + (uint64_t)ts.tv_nsec;
}

static void real_handler(int signal) {
    (void)signal;
    real_active = 1;
    uint64_t prof_start = now_ns(RAW_PROF_CLOCK);
    uint64_t wall_start = now_ns(CLOCK_MONOTONIC);
    static const char entered[] = "{\"event\":\"entered\"}\n";
    if (write(STDOUT_FILENO, entered, sizeof(entered) - 1) != sizeof(entered) - 1)
        _exit(3);
    while (now_ns(RAW_PROF_CLOCK) - prof_start < SECOND_NS) {
        if (now_ns(CLOCK_MONOTONIC) - wall_start > 30 * SECOND_NS)
            _exit(4);
    }
    real_active = 0;
}

static void timer_handler(int signal, siginfo_t *info, void *context) {
    (void)context;
    if (signal == SIGUSR2) {
        ++sched_count;
        sched_in_real = real_active;
        sched_overrun = info->si_overrun;
    } else {
        ++prof_count;
        prof_in_real = real_active;
        prof_overrun = info->si_overrun;
    }
}

static timer_t arm_timer(clockid_t clock, int signal) {
    timer_t timer;
    struct sigevent event = {.sigev_notify = SIGEV_SIGNAL, .sigev_signo = signal};
    struct itimerspec timeout = {.it_value = {.tv_sec = 4}};
    if (timer_create(clock, &event, &timer) != 0)
        err(1, "timer_create");
    if (timer_settime(timer, 0, &timeout, NULL) != 0)
        err(1, "timer_settime");
    return timer;
}

int main(void) {
    cpu_set_t cpus;
    CPU_ZERO(&cpus);
    CPU_SET(1, &cpus);
    if (sched_setaffinity(0, sizeof(cpus), &cpus) != 0)
        err(1, "sched_setaffinity");

    struct sigaction real_action = {.sa_handler = real_handler};
    struct sigaction timer_action = {.sa_sigaction = timer_handler, .sa_flags = SA_SIGINFO};
    sigemptyset(&real_action.sa_mask);
    sigemptyset(&timer_action.sa_mask);
    if (sigaction(SIGUSR1, &real_action, NULL) != 0 ||
        sigaction(SIGUSR2, &timer_action, NULL) != 0 ||
        sigaction(SIGALRM, &timer_action, NULL) != 0)
        err(1, "sigaction");
    sigset_t signals;
    sigemptyset(&signals);
    sigaddset(&signals, SIGUSR1);
    sigaddset(&signals, SIGUSR2);
    sigaddset(&signals, SIGALRM);
    if (sigprocmask(SIG_UNBLOCK, &signals, NULL) != 0)
        err(1, "sigprocmask");

    timer_t sched_timer = arm_timer(CLOCK_THREAD_CPUTIME_ID, SIGUSR2);
    timer_t prof_timer = arm_timer(RAW_PROF_CLOCK, SIGALRM);
    uint64_t sched_before = now_ns(CLOCK_THREAD_CPUTIME_ID);
    uint64_t prof_before = now_ns(RAW_PROF_CLOCK);
    uint64_t wall_before = now_ns(CLOCK_MONOTONIC);
    if (raise(SIGUSR1) != 0)
        err(1, "raise");
    uint64_t sched_after = now_ns(CLOCK_THREAD_CPUTIME_ID);
    uint64_t prof_after = now_ns(RAW_PROF_CLOCK);
    uint64_t wall_after = now_ns(CLOCK_MONOTONIC);
    if (timer_delete(sched_timer) != 0 || timer_delete(prof_timer) != 0)
        err(1, "timer_delete");

    printf("{\"event\":\"result\",\"cpu\":1,"
           "\"sched_before_ns\":%" PRIu64 ",\"sched_after_ns\":%" PRIu64 ","
           "\"prof_before_ns\":%" PRIu64 ",\"prof_after_ns\":%" PRIu64 ","
           "\"wall_before_ns\":%" PRIu64 ",\"wall_after_ns\":%" PRIu64 ","
           "\"sched_delta_ns\":%" PRIu64 ",\"prof_delta_ns\":%" PRIu64 ","
           "\"wall_delta_ns\":%" PRIu64 ","
           "\"sched_count\":%d,\"sched_in_real\":%d,\"sched_overrun\":%d,"
           "\"prof_count\":%d,\"prof_in_real\":%d,\"prof_overrun\":%d}\n",
           sched_before, sched_after, prof_before, prof_after, wall_before, wall_after,
           sched_after - sched_before, prof_after - prof_before, wall_after - wall_before,
           (int)sched_count, (int)sched_in_real, (int)sched_overrun, (int)prof_count,
           (int)prof_in_real, (int)prof_overrun);
    return 0;
}
