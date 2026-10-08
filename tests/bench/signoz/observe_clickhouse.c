#define _GNU_SOURCE
#ifdef NDEBUG
#error "This diagnostic requires enabled assertions, including checked system calls."
#endif
#include <assert.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static unsigned long long monotonic_nanoseconds(void) {
    struct timespec value;
    assert(clock_gettime(CLOCK_MONOTONIC, &value) == 0);
    return (unsigned long long)value.tv_sec * 1000000000ULL + (unsigned long long)value.tv_nsec;
}

static void read_proc(const char *path, char *buffer, size_t capacity) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) {
        perror(path);
        exit(1);
    }
    size_t used = 0;
    ssize_t count;
    while ((count = read(fd, buffer + used, capacity - used - 1)) > 0) {
        used += (size_t)count;
        assert(used < capacity - 1);
    }
    if (count < 0) {
        perror(path);
        exit(1);
    }
    assert(close(fd) == 0);
    while (used && buffer[used - 1] == '\n')
        --used;
    buffer[used] = 0;
}

static void task_field(const char *tid, const char *field, char *buffer, size_t size) {
    char path[512];
    int length = snprintf(path, sizeof(path), "/proc/1/task/%s/%s", tid, field);
    assert(length > 0 && (size_t)length < sizeof(path));
    unsigned long long begin = monotonic_nanoseconds();
    read_proc(path, buffer, size);
    unsigned long long end = monotonic_nanoseconds();
    assert(end >= begin);
    if (end - begin > 10000000ULL)
        fprintf(stderr, "slow_read\t%s\t%s\t%llu\t%llu\n", tid, field, begin, end - begin);
}

static void boundary(const char *name, long sample) {
    char uptime[128];
    struct timespec now;
    read_proc("/proc/uptime", uptime, sizeof(uptime));
    assert(clock_gettime(CLOCK_REALTIME, &now) == 0);
    printf("%s\t%ld\t%lld.%09ld\t%s\n", name, sample, (long long)now.tv_sec, now.tv_nsec, uptime);
}

int main(int argc, char **argv) {
    assert(argc == 3);
    char *end;
    errno = 0;
    long samples = strtol(argv[1], &end, 10);
    assert(!errno && end != argv[1] && !*end && samples > 0);
    assert(strcmp(argv[2], "fifo") == 0 || strcmp(argv[2], "other") == 0);
    int policy = strcmp(argv[2], "fifo") == 0 ? SCHED_FIFO : SCHED_OTHER;
    // FIFO keeps the observer outside the fair scheduler debt under investigation.
    struct sched_param priority = {.sched_priority = policy == SCHED_FIFO ? 1 : 0};
    // musl's POSIX wrapper returns ENOSYS; the Linux syscall sets this task's policy.
    if (syscall(SYS_sched_setscheduler, 0, policy, &priority) != 0) {
        perror("sched_setscheduler (requires CAP_SYS_NICE)");
        return 1;
    }
    assert(syscall(SYS_sched_getscheduler, 0) == policy);
    fprintf(stderr, "observer_pid=%d policy=%s priority=%d\n", getpid(), argv[2],
            priority.sched_priority);
    char budget[128], period[128];
    read_proc("/proc/sys/kernel/sched_rt_runtime_us", budget, sizeof(budget));
    read_proc("/proc/sys/kernel/sched_rt_period_us", period, sizeof(period));
    fprintf(stderr, "sched_rt_runtime_us=%s sched_rt_period_us=%s\n", budget, period);
    static char output_buffer[1024 * 1024];
    assert(setvbuf(stdout, output_buffer, _IOFBF, sizeof(output_buffer)) == 0);
    const char *keys[] = {"State:",
                          "Uid:",
                          "Cpus_allowed_list:",
                          "SigPnd:",
                          "SigBlk:",
                          "voluntary_ctxt_switches:",
                          "nonvoluntary_ctxt_switches:"};
    for (long sample = 0; sample < samples; ++sample) {
        char buffer[16384];
        unsigned common = 0, background = 0;
        read_proc("/sys/fs/cgroup/cpu.stat", buffer, sizeof(buffer));
        boundary("sample_begin", sample);
        printf("cgroup_cpu\t%s\n", buffer);
        assert(fflush(stdout) == 0);
        DIR *tasks = opendir("/proc/1/task");
        assert(tasks);
        struct dirent *entry;
        for (;;) {
            errno = 0;
            entry = readdir(tasks);
            if (!entry) {
                assert(errno == 0);
                break;
            }
            if (entry->d_name[0] == '.')
                continue;
            task_field(entry->d_name, "comm", buffer, sizeof(buffer));
            if (strcmp(buffer, "Common") == 0)
                ++common;
            else if (strcmp(buffer, "BgSchPool") == 0)
                ++background;
            else
                continue;
            assert(syscall(SYS_sched_getscheduler, atoi(entry->d_name)) == SCHED_OTHER);
            printf("thread\t%s\t%s\n", entry->d_name, buffer);
            task_field(entry->d_name, "status", buffer, sizeof(buffer));
            unsigned fields = 0;
            char *save;
            for (char *line = strtok_r(buffer, "\n", &save); line;
                 line = strtok_r(NULL, "\n", &save)) {
                for (size_t key = 0; key < sizeof(keys) / sizeof(*keys); ++key) {
                    if (strncmp(line, keys[key], strlen(keys[key])) == 0) {
                        puts(line);
                        ++fields;
                    }
                }
            }
            assert(fields == sizeof(keys) / sizeof(*keys));
            const char *files[] = {"schedstat", "syscall", "wchan"};
            for (size_t field = 0; field < sizeof(files) / sizeof(*files); ++field) {
                task_field(entry->d_name, files[field], buffer, sizeof(buffer));
                printf("%s\t%s\n", files[field], buffer);
            }
        }
        assert(closedir(tasks) == 0 && common && background);
        printf("thread_counts\tCommon\t%u\tBgSchPool\t%u\n", common, background);
        read_proc("/proc/self/schedstat", buffer, sizeof(buffer));
        printf("observer\t%d\t%s\n", getpid(), buffer);
        boundary("sample_end", sample);
        assert(fflush(stdout) == 0);
        if (sample + 1 < samples) {
            struct timespec interval = {.tv_sec = 1};
            assert(nanosleep(&interval, NULL) == 0);
        }
    }
    return 0;
}
