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

static pthread_cond_t condition = PTHREAD_COND_INITIALIZER;
static pthread_mutex_t mutex = PTHREAD_MUTEX_INITIALIZER;
static int gate[2];
static int done[2];
static atomic_uint tids[2];
static atomic_uint handler_entered, handler_exited, signaler_entered, signaler_returned;
static double signaler_start, signaler_end, signaler_cpu;

_Static_assert(ATOMIC_INT_LOCK_FREE == 2, "Signal handler requires lock-free atomic integers");

static void check(int rc) {
    if (rc) {
        fprintf(stderr, "pthread error: %s\n", strerror(rc));
        exit(1);
    }
}

static double seconds(clockid_t clock) {
    struct timespec t;
    assert(clock_gettime(clock, &t) == 0);
    return (double)t.tv_sec + (double)t.tv_nsec / 1e9;
}

static void poll_pause(void) {
    struct timespec t = {.tv_nsec = 100000};
    assert(nanosleep(&t, NULL) == 0);
}

static void wait_until_futex(unsigned index) {
    while (!atomic_load(&tids[index]))
        poll_pause();
    unsigned tid = atomic_load(&tids[index]);
    char path[80], line[256];
    snprintf(path, sizeof(path), "/proc/self/task/%u/syscall", tid);
    for (;;) {
        FILE *stream = fopen(path, "r");
        assert(stream);
        char *result = fgets(line, sizeof(line), stream);
        assert(result);
        assert(fclose(stream) == 0);
        long number;
        unsigned long address, operation;
        if (sscanf(line, "%ld %lx %lx", &number, &address, &operation) == 3 &&
            number == SYS_futex && address >= (unsigned long)&condition &&
            address < (unsigned long)(&condition + 1) &&
            ((operation & FUTEX_CMD_MASK) == FUTEX_WAIT ||
             (operation & FUTEX_CMD_MASK) == FUTEX_WAIT_BITSET)) {
            printf("{\"waiter\":%u,\"tid\":%u,\"futex_address\":\"0x%lx\",\"operation\":%lu}\n",
                   index, tid, address, operation);
            return;
        }
        poll_pause();
    }
}

static void hold_waiter(int signal) {
    (void)signal;
    int saved_errno = errno;
    char byte;
    atomic_store(&handler_entered, 1);
    ssize_t n;
    do {
        n = read(gate[0], &byte, 1);
    } while (n < 0 && errno == EINTR);
    if (n != 1)
        _exit(2);
    atomic_store(&handler_exited, 1);
    errno = saved_errno;
}

static void *waiter(void *arg) {
    size_t index = (size_t)arg;
    atomic_store(&tids[index], (unsigned)syscall(SYS_gettid));
    check(pthread_mutex_lock(&mutex));
    while (!done[index])
        check(pthread_cond_wait(&condition, &mutex));
    check(pthread_mutex_unlock(&mutex));
    return NULL;
}

static void *signaler(void *arg) {
    (void)arg;
    check(pthread_mutex_lock(&mutex));
    done[1] = 1;
    double cpu_start = seconds(CLOCK_THREAD_CPUTIME_ID);
    signaler_start = seconds(CLOCK_MONOTONIC);
    atomic_store(&signaler_entered, 1);
    check(pthread_cond_signal(&condition));
    signaler_end = seconds(CLOCK_MONOTONIC);
    signaler_cpu = seconds(CLOCK_THREAD_CPUTIME_ID) - cpu_start;
    atomic_store(&signaler_returned, 1);
    check(pthread_mutex_unlock(&mutex));
    return NULL;
}

int main(int argc, char **argv) {
    assert(argc == 3);
    int delayed = strcmp(argv[1], "delay") == 0;
    assert(delayed || strcmp(argv[1], "control") == 0);
    int expect_wait = strcmp(argv[2], "wait") == 0;
    assert(expect_wait || strcmp(argv[2], "fast") == 0);
    setvbuf(stdout, NULL, _IOLBF, 0);
    alarm(15);
    cpu_set_t cpus;
    CPU_ZERO(&cpus);
    CPU_SET(2, &cpus);
    assert(sched_setaffinity(0, sizeof(cpus), &cpus) == 0);
    assert(pipe(gate) == 0);
    struct sigaction action = {.sa_handler = hold_waiter, .sa_flags = SA_RESTART};
    assert(sigemptyset(&action.sa_mask) == 0);
    assert(sigaction(SIGUSR1, &action, NULL) == 0);

    pthread_t a, b, s;
    check(pthread_create(&a, NULL, waiter, (void *)0));
    wait_until_futex(0);
    check(pthread_kill(a, SIGUSR1));
    while (!atomic_load(&handler_entered))
        poll_pause();

    check(pthread_mutex_lock(&mutex));
    done[0] = 1;
    double first_start = seconds(CLOCK_MONOTONIC);
    check(pthread_cond_signal(&condition));
    double first_elapsed = seconds(CLOCK_MONOTONIC) - first_start;
    check(pthread_mutex_unlock(&mutex));

    check(pthread_create(&b, NULL, waiter, (void *)1));
    wait_until_futex(1);
    double released_at = 0;
    if (!delayed) {
        released_at = seconds(CLOCK_MONOTONIC);
        assert(write(gate[1], "x", 1) == 1);
        while (!atomic_load(&handler_exited))
            poll_pause();
    }
    check(pthread_create(&s, NULL, signaler, NULL));
    while (!atomic_load(&signaler_entered))
        poll_pause();
    double observed_at = 0;
    unsigned returned_before_release = 0;
    if (delayed) {
        observed_at = seconds(CLOCK_MONOTONIC);
        struct timespec delay = {.tv_sec = 2};
        assert(nanosleep(&delay, NULL) == 0);
        returned_before_release = atomic_load(&signaler_returned);
        assert(returned_before_release == !expect_wait);
        printf(
            "{\"stage\":\"before releasing A\",\"signaler_returned\":%u,\"handler_exited\":%u}\n",
            returned_before_release, atomic_load(&handler_exited));
        assert(!atomic_load(&handler_exited));
        released_at = seconds(CLOCK_MONOTONIC);
        assert(write(gate[1], "x", 1) == 1);
    }
    check(pthread_join(s, NULL));
    check(pthread_join(a, NULL));
    check(pthread_join(b, NULL));
    double second_elapsed = signaler_end - signaler_start;
    printf("{\"result\":\"%s\",\"expected\":\"%s\",\"libc\":\"%s\",\"cpu\":2,"
           "\"first_signal_seconds\":%.9f,\"second_signal_seconds\":%.9f,"
           "\"second_signal_cpu_seconds\":%.9f,\"gate_hold_seconds\":%.9f,"
           "\"after_release_seconds\":%.9f,\"returned_before_release\":%u}\n",
           argv[1], argv[2], gnu_get_libc_version(), first_elapsed, second_elapsed, signaler_cpu,
           delayed ? released_at - observed_at : 0, signaler_end - released_at,
           returned_before_release);
    assert(expect_wait ? second_elapsed >= 1.95 : second_elapsed < 0.5);
    assert(signaler_end - released_at < 0.5);
    assert(close(gate[0]) == 0 && close(gate[1]) == 0);
    alarm(0);
    return 0;
}
