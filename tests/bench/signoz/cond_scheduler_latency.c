#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <gnu/libc-version.h>
#include <linux/futex.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

enum { CANDIDATES = 8 };
struct candidate;
struct waiter {
    struct candidate *owner;
    unsigned slot;
    atomic_uint tid, completed;
};
struct candidate {
    pthread_cond_t condition;
    pthread_mutex_t mutex;
    pthread_t thread;
    struct waiter waiters[2];
    int done[2];
    atomic_uint entered, release, exited;
    atomic_ullong exit_ns;
};
struct sample {
    unsigned long long begin, end, runtime, delay, dispatches, prof;
    unsigned exited, completed, returned;
    char state;
};
static struct candidate candidates[CANDIDATES], *victim;
static _Thread_local struct candidate *current;
static atomic_uint cleanup, signaler_entered, signaler_returned;
static unsigned long long signaler_start, signaler_end, signaler_cpu;
static unsigned returned_before_exit, returned_before_release;

_Static_assert(ATOMIC_INT_LOCK_FREE == 2 && ATOMIC_LLONG_LOCK_FREE == 2,
               "Signal handler requires lock-free atomics");

static void check(int rc) {
    if (rc) {
        fprintf(stderr, "pthread error: %s\n", strerror(rc));
        exit(1);
    }
}

static unsigned long long nanoseconds(clockid_t clock) {
    struct timespec t;
    if (clock_gettime(clock, &t) != 0)
        _exit(2);
    return (unsigned long long)t.tv_sec * 1000000000ULL + (unsigned long long)t.tv_nsec;
}

static void poll_pause(void) {
    struct timespec t = {.tv_nsec = 1000000};
    assert(nanosleep(&t, NULL) == 0);
}

static void pin_cpu(unsigned cpu) {
    cpu_set_t cpus;
    CPU_ZERO(&cpus);
    CPU_SET(cpu, &cpus);
    assert(sched_setaffinity(0, sizeof(cpus), &cpus) == 0);
}

static void wait_until_futex(struct waiter *w) {
    while (!atomic_load(&w->tid))
        poll_pause();
    char path[80], line[256];
    snprintf(path, sizeof(path), "/proc/self/task/%u/syscall", atomic_load(&w->tid));
    for (;;) {
        FILE *stream = fopen(path, "r");
        assert(stream && fgets(line, sizeof(line), stream));
        assert(fclose(stream) == 0);
        long number;
        unsigned long address, operation;
        if (sscanf(line, "%ld %lx %lx", &number, &address, &operation) == 3 &&
            number == SYS_futex && address >= (unsigned long)&w->owner->condition &&
            address < (unsigned long)(&w->owner->condition + 1) &&
            ((operation & FUTEX_CMD_MASK) == FUTEX_WAIT ||
             (operation & FUTEX_CMD_MASK) == FUTEX_WAIT_BITSET))
            return;
        poll_pause();
    }
}

static void hold_waiter(int signal) {
    (void)signal;
    int saved_errno = errno;
    atomic_store(&current->entered, 1);
    while (!atomic_load(&current->release))
        atomic_signal_fence(memory_order_seq_cst);
    atomic_store(&current->exit_ns, nanoseconds(CLOCK_MONOTONIC));
    atomic_store(&current->exited, 1);
    errno = saved_errno;
}

static void *waiter(void *arg) {
    struct waiter *w = arg;
    struct candidate *c = w->owner;
    pin_cpu(w->slot == 0 ? 1 : 2);
    current = c;
    atomic_store(&w->tid, (unsigned)syscall(SYS_gettid));
    check(pthread_mutex_lock(&c->mutex));
    while (!c->done[w->slot])
        check(pthread_cond_wait(&c->condition, &c->mutex));
    check(pthread_mutex_unlock(&c->mutex));
    atomic_store(&w->completed, 1);
    // Keep /proc data available until the controller finishes its snapshots.
    while (!atomic_load(&cleanup))
        poll_pause();
    return NULL;
}

static struct sample snapshot(struct candidate *c) {
    struct sample s = {.begin = nanoseconds(CLOCK_MONOTONIC)};
    unsigned tid = atomic_load(&c->waiters[0].tid);
    char path[80], line[256];
    snprintf(path, sizeof(path), "/proc/self/task/%u/schedstat", tid);
    FILE *stream = fopen(path, "r");
    assert(stream && fscanf(stream, "%llu %llu %llu", &s.runtime, &s.delay, &s.dispatches) == 3);
    assert(fclose(stream) == 0);
    snprintf(path, sizeof(path), "/proc/self/task/%u/status", tid);
    stream = fopen(path, "r");
    assert(stream);
    while (fgets(line, sizeof(line), stream))
        if (sscanf(line, "State:\t%c", &s.state) == 1)
            break;
    assert(s.state);
    assert(fclose(stream) == 0);
    // Raw PROF does not force a remote task_sched_runtime() accounting update.
    s.prof = nanoseconds((clockid_t)((~tid << 3) | 4U));
    s.exited = atomic_load(&c->exited);
    s.completed = atomic_load(&c->waiters[0].completed);
    s.returned = atomic_load(&signaler_returned);
    s.end = nanoseconds(CLOCK_MONOTONIC);
    return s;
}

static void report(const char *stage, unsigned index, struct sample s) {
    struct candidate *c = &candidates[index];
    printf("{\"stage\":\"%s\",\"candidate\":%u,\"tid\":%u,\"state\":\"%c\","
           "\"begin_ns\":%llu,\"end_ns\":%llu,\"runtime_ns\":%llu,\"delay_ns\":%llu,"
           "\"dispatches\":%llu,\"raw_prof_ns\":%llu,\"handler_exited\":%u,"
           "\"waiter_completed\":%u,\"signaler_returned\":%u}\n",
           stage, index, atomic_load(&c->waiters[0].tid), s.state, s.begin, s.end, s.runtime,
           s.delay, s.dispatches, s.prof, s.exited, s.completed, s.returned);
}

static void *signaler(void *arg) {
    (void)arg;
    check(pthread_mutex_lock(&victim->mutex));
    victim->done[1] = 1;
    unsigned long long cpu_start = nanoseconds(CLOCK_THREAD_CPUTIME_ID);
    signaler_start = nanoseconds(CLOCK_MONOTONIC);
    atomic_store(&signaler_entered, 1);
    check(pthread_cond_signal(&victim->condition));
    signaler_end = nanoseconds(CLOCK_MONOTONIC);
    signaler_cpu = nanoseconds(CLOCK_THREAD_CPUTIME_ID) - cpu_start;
    returned_before_exit = !atomic_load(&victim->exited);
    returned_before_release = !atomic_load(&victim->release);
    atomic_store(&signaler_returned, 1);
    check(pthread_mutex_unlock(&victim->mutex));
    return NULL;
}

static void finish_candidates(void) {
    atomic_store(&cleanup, 1);
    for (unsigned i = 0; i < CANDIDATES; i++) {
        struct candidate *c = &candidates[i];
        check(pthread_mutex_lock(&c->mutex));
        c->done[0] = 1;
        check(pthread_cond_signal(&c->condition));
        check(pthread_mutex_unlock(&c->mutex));
        atomic_store(&c->release, 1);
    }
    for (unsigned i = 0; i < CANDIDATES; i++)
        check(pthread_join(candidates[i].thread, NULL));
}

int main(int argc, char **argv) {
    assert(argc == 3);
    int paused = strcmp(argv[1], "pause") == 0;
    assert(paused || strcmp(argv[1], "control") == 0);
    int expect_wait = strcmp(argv[2], "wait") == 0;
    assert(expect_wait || strcmp(argv[2], "fast") == 0);
    setvbuf(stdout, NULL, _IOLBF, 0);
    alarm(150);
    pin_cpu(2);
    assert(sched_getscheduler(0) == SCHED_OTHER);
    struct sigaction action = {.sa_handler = hold_waiter, .sa_flags = SA_RESTART};
    assert(sigemptyset(&action.sa_mask) == 0);
    assert(sigaction(SIGUSR1, &action, NULL) == 0);
    for (unsigned i = 0; i < CANDIDATES; i++) {
        struct candidate *c = &candidates[i];
        check(pthread_cond_init(&c->condition, NULL));
        check(pthread_mutex_init(&c->mutex, NULL));
        for (unsigned slot = 0; slot < 2; slot++) {
            c->waiters[slot].owner = c;
            c->waiters[slot].slot = slot;
        }
        check(pthread_create(&c->thread, NULL, waiter, &c->waiters[0]));
        wait_until_futex(&c->waiters[0]);
        check(pthread_kill(c->thread, SIGUSR1));
        while (!atomic_load(&c->entered))
            poll_pause();
    }
    struct sample before[CANDIDATES], after[CANDIDATES];
    for (unsigned i = 0; i < CANDIDATES; i++) {
        before[i] = snapshot(&candidates[i]);
        assert(before[i].state == 'R');
        report("before", i, before[i]);
    }
    printf("{\"stage\":\"ready\",\"case\":\"%s\",\"expected\":\"%s\",\"libc\":\"%s\","
           "\"candidates\":%u}\n",
           argv[1], argv[2], gnu_get_libc_version(), CANDIDATES);
    char command[16];
    assert(fgets(command, sizeof(command), stdin) && strcmp(command, "go\n") == 0);
    unsigned long long resumed_at = nanoseconds(CLOCK_MONOTONIC);
    unsigned selected = 0, eligible;
    do {
        eligible = 0;
        for (unsigned i = 0; i < CANDIDATES; i++) {
            after[i] = snapshot(&candidates[i]);
            double excess = (double)(after[i].runtime - before[i].runtime) -
                            (double)(after[i].prof - before[i].prof);
            if (excess >= 8000000000.0) {
                selected = i;
                eligible++;
            }
        }
        if (!paused || eligible || nanoseconds(CLOCK_MONOTONIC) - resumed_at >= 500000000ULL)
            break;
        poll_pause();
    } while (1);
    for (unsigned i = 0; i < CANDIDATES; i++)
        report("after", i, after[i]);
    if (paused && eligible != 1) {
        printf(
            "{\"result\":\"inconclusive\",\"reason\":\"nonunique_overcharge\",\"eligible\":%u}\n",
            eligible);
        finish_candidates();
        return 2;
    }
    if (!paused)
        selected = 0;
    victim = &candidates[selected];
    check(pthread_mutex_lock(&victim->mutex));
    victim->done[0] = 1;
    unsigned long long first_start = nanoseconds(CLOCK_MONOTONIC);
    check(pthread_cond_signal(&victim->condition));
    unsigned long long first_elapsed = nanoseconds(CLOCK_MONOTONIC) - first_start;
    check(pthread_mutex_unlock(&victim->mutex));
    pthread_t b, s;
    check(pthread_create(&b, NULL, waiter, &victim->waiters[1]));
    wait_until_futex(&victim->waiters[1]);
    check(pthread_create(&s, NULL, signaler, NULL));
    while (!atomic_load(&signaler_entered))
        poll_pause();
    assert(!atomic_load(&victim->exited));
    struct sample held = snapshot(victim);
    assert(held.state == 'R');
    report("before_release", selected, held);
    unsigned long long released_at = nanoseconds(CLOCK_MONOTONIC);
    atomic_store(&victim->release, 1);
    report("release", selected, snapshot(victim));
    unsigned peer = (selected + 1) % CANDIDATES;
    report("peer_release", peer, snapshot(&candidates[peer]));
    unsigned long long next_report = released_at + 1000000000ULL;
    while (!atomic_load(&victim->waiters[0].completed) || !atomic_load(&signaler_returned)) {
        if (nanoseconds(CLOCK_MONOTONIC) >= next_report) {
            report("waiting", selected, snapshot(victim));
            report("peer_waiting", peer, snapshot(&candidates[peer]));
            next_report = nanoseconds(CLOCK_MONOTONIC) + 1000000000ULL;
        }
        poll_pause();
    }
    report("completed", selected, snapshot(victim));
    report("peer_completed", peer, snapshot(&candidates[peer]));
    check(pthread_join(s, NULL));
    unsigned long long exited_at = atomic_load(&victim->exit_ns);
    double second_elapsed = (signaler_end - signaler_start) / 1e9;
    double release_to_exit = (exited_at - released_at) / 1e9;
    finish_candidates();
    check(pthread_join(b, NULL));
    printf("{\"result\":\"%s\",\"expected\":\"%s\",\"victim\":%u,\"libc\":\"%s\","
           "\"resumed_ns\":%llu,\"release_ns\":%llu,\"handler_exit_ns\":%llu,"
           "\"signaler_start_ns\":%llu,\"signaler_end_ns\":%llu,"
           "\"first_signal_seconds\":%.9f,\"second_signal_seconds\":%.9f,"
           "\"second_signal_cpu_seconds\":%.9f,\"release_to_exit_seconds\":%.9f,"
           "\"returned_before_handler_exit\":%u,\"returned_before_release\":%u}\n",
           argv[1], argv[2], selected, gnu_get_libc_version(), resumed_at, released_at, exited_at,
           signaler_start, signaler_end, first_elapsed / 1e9, second_elapsed, signaler_cpu / 1e9,
           release_to_exit, returned_before_exit, returned_before_release);
    assert(first_elapsed < 500000000ULL);
    assert(paused ? release_to_exit > 40 : release_to_exit < 0.5);
    if (paused && expect_wait) {
        assert(!returned_before_exit && signaler_end >= exited_at);
        assert(second_elapsed > 40 && signaler_end - exited_at < 500000000ULL);
    } else {
        assert(second_elapsed < 0.5);
        assert(!paused || returned_before_exit);
    }
    alarm(0);
    return 0;
}
