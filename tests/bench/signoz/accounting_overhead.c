#define _GNU_SOURCE
#include <err.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static uint64_t now_ns(void) {
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) err(1, "clock_gettime");
    return (uint64_t)ts.tv_sec * 1000000000 + ts.tv_nsec;
}

static void send_byte(int fd) {
    char value = 'x';
    if (write(fd, &value, 1) != 1) err(1, "write");
}

static void receive_byte(int fd) {
    char value;
    if (read(fd, &value, 1) != 1) err(1, "read");
}

int main(void) {
    cpu_set_t cpus;
    CPU_ZERO(&cpus);
    CPU_SET(0, &cpus);
    if (sched_setaffinity(0, sizeof(cpus), &cpus) != 0) err(1, "sched_setaffinity");
    const int calls = 2000000;
    const int rounds = 100000;
    for (int run = 0; run < 5; ++run) {
        uint64_t start = now_ns();
        for (int i = 0; i < calls; ++i) syscall(SYS_getpid);
        uint64_t syscall_ns = now_ns() - start;
        int request[2], response[2];
        if (pipe(request) != 0 || pipe(response) != 0) err(1, "pipe");
        pid_t child = fork();
        if (child < 0) err(1, "fork");
        if (child == 0) {
            close(request[1]);
            close(response[0]);
            for (int i = 0; i < rounds; ++i) {
                receive_byte(request[0]);
                send_byte(response[1]);
            }
            _exit(0);
        }
        close(request[0]);
        close(response[1]);
        start = now_ns();
        for (int i = 0; i < rounds; ++i) {
            send_byte(request[1]);
            receive_byte(response[0]);
        }
        uint64_t pipe_ns = now_ns() - start;
        close(request[1]);
        close(response[0]);
        int status;
        if (waitpid(child, &status, 0) != child) err(1, "waitpid");
        if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) errx(1, "child failed");
        printf("{\"run\":%d,\"getpid_ns\":%.2f,\"pipe_roundtrip_ns\":%.2f}\n",
               run, (double)syscall_ns / calls, (double)pipe_ns / rounds);
        fflush(stdout);
    }
    return 0;
}
