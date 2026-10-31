/*
 * PID 1 of the HV checkpoint spike 3 and 4 guest (initramfs /init).
 *
 * Four I/O threads hammer one 4 KiB-block disk image, each in its own
 * region: threads 0 and 1 through /dev/vda (virtio-blk, the async queue
 * path), threads 2 and 3 through /dev/arcboxhvc0 (ArcBox's synchronous
 * HVC fast path to the same image; on VZ, which has no HVC path, all four
 * use /dev/vda). Every write stamps the whole block
 * from (thread, block, seq); every read checks a block against the seq the
 * thread keeps for it in RAM. O_DIRECT keeps the page cache out of the way,
 * so each operation is a real device request. A ticker prints one line a
 * second on hvc0:
 *
 *   tick <n> mono_ms <m> real <s> ops <a> <b> <c> <d> fails <f>
 *
 * After a restore, ticks that keep coming prove the timers work; ops that
 * keep growing on all four threads prove both block paths work; fails that
 * stay 0 prove the disk came back consistent with guest RAM. Mismatches are
 * printed as "FAIL ..." lines.
 *
 * With spike_online_at=<n> on the kernel command line (the kernel hands
 * unknown parameters to init as environment variables), tick n brings every
 * offline CPU online through sysfs, i.e. PSCI CPU_ON, and prints
 * "online <cpus>"; boot with maxcpus=<k> to leave CPUs offline until then.
 *
 * With spike_net=<echo>,<idle>,<stream> (spikes 4 and 5, host TCP ports),
 * eth0 comes up as 10.0.2.2/24 and four more threads run, each
 * reconnecting when its connection ends:
 *
 * - echo: sends an 8-byte counter to 10.0.2.1:<echo> every 10 ms and
 *   checks the echo, with a 5 s receive timeout;
 * - idle: connects to 10.0.2.1:<idle>, which never sends, and blocks in
 *   recv with no timeout, so it sends nothing a host could answer;
 * - download: reads the host's endless little-endian u64 counter stream
 *   from 10.0.2.1:<stream> and checks every word;
 * - a vsock echo server on port 5000, one host connection at a time.
 *
 * The tick line gains
 *
 *   ... net <ok> <reset> <timeout> vsock <ok> <ended> idle <ended>
 *       down <kib> <ended>
 *
 * <reset> counts ECONNRESET/EPIPE and <timeout> counts receive timeouts of
 * the echo client; every <ended> counts connections that ended, by EOF or
 * by error. Each connection that ends prints "<name>-end errno <e> ...".
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <net/if.h>
#include <netinet/in.h>
#include <arpa/inet.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/socket.h>
#include <linux/vm_sockets.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

#define BLK 4096
#define THREADS 4
#define BLOCKS_PER_THREAD 4096 /* 16 MiB per thread */
#define MAGIC 0x41524342434b5054ull /* "ARCBCKPT" */

static volatile uint64_t ops[THREADS];
static volatile uint64_t fails;

static uint64_t word(uint64_t seq, uint64_t block, uint64_t i) {
    uint64_t x = seq * 0x9e3779b97f4a7c15ull ^ block * 0xbf58476d1ce4e5b9ull ^ i;
    x ^= x >> 31;
    return x * 0x94d049bb133111ebull;
}

static void fill(uint64_t *buf, uint64_t thread, uint64_t block, uint64_t seq) {
    buf[0] = MAGIC;
    buf[1] = thread;
    buf[2] = block;
    buf[3] = seq;
    for (uint64_t i = 4; i < BLK / 8; i++)
        buf[i] = word(seq, block, i);
}

/* Returns the first mismatching word index, or -1. */
static int check(const uint64_t *buf, uint64_t thread, uint64_t block, uint64_t seq) {
    if (seq == 0) {
        for (int i = 0; i < BLK / 8; i++)
            if (buf[i] != 0)
                return i;
        return -1;
    }
    uint64_t head[4] = {MAGIC, thread, block, seq};
    for (int i = 0; i < 4; i++)
        if (buf[i] != head[i])
            return i;
    for (uint64_t i = 4; i < BLK / 8; i++)
        if (buf[i] != word(seq, block, i))
            return (int)i;
    return -1;
}

struct worker {
    uint64_t id;
    const char *dev;
    uint64_t seq[BLOCKS_PER_THREAD]; /* 0 = never written */
};

static void *run(void *arg) {
    struct worker *w = arg;
    int fd = open(w->dev, O_RDWR | O_DIRECT);
    if (fd < 0) {
        printf("FAIL open %s\n", w->dev);
        fails++;
        return NULL;
    }
    uint64_t *wbuf, *rbuf;
    if (posix_memalign((void **)&wbuf, BLK, BLK) || posix_memalign((void **)&rbuf, BLK, BLK))
        return NULL;
    uint64_t base = w->id * BLOCKS_PER_THREAD;
    uint64_t rng = 0x2545f4914f6cdd1dull ^ w->id;
    uint64_t next_seq = 1;
    for (;;) {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        uint64_t b = rng % BLOCKS_PER_THREAD;
        uint64_t block = base + b;
        uint64_t seq = next_seq++;
        fill(wbuf, w->id, block, seq);
        if (pwrite(fd, wbuf, BLK, (off_t)(block * BLK)) != BLK) {
            printf("FAIL write t%llu block %llu\n", (unsigned long long)w->id,
                   (unsigned long long)block);
            fails++;
        }
        w->seq[b] = seq;

        uint64_t r = (rng >> 20) % BLOCKS_PER_THREAD;
        if (pread(fd, rbuf, BLK, (off_t)((base + r) * BLK)) != BLK) {
            printf("FAIL read t%llu block %llu\n", (unsigned long long)w->id,
                   (unsigned long long)(base + r));
            fails++;
        } else {
            int bad = check(rbuf, w->id, base + r, w->seq[r]);
            if (bad >= 0) {
                printf("FAIL verify t%llu block %llu want seq %llu word %d got %016llx (seq %llu)\n",
                       (unsigned long long)w->id, (unsigned long long)(base + r),
                       (unsigned long long)w->seq[r], bad, (unsigned long long)rbuf[bad],
                       (unsigned long long)rbuf[3]);
                fails++;
            }
        }
        if (seq % 256 == 0)
            fdatasync(fd);
        ops[w->id]++;
    }
}

static void online_all(void) {
    mkdir("/sys", 0755);
    mount("sysfs", "/sys", "sysfs", 0, NULL);
    for (int cpu = 1; cpu < 64; cpu++) {
        char path[64];
        snprintf(path, sizeof path, "/sys/devices/system/cpu/cpu%d/online", cpu);
        int fd = open(path, O_WRONLY);
        if (fd < 0)
            continue;
        if (write(fd, "1", 1) != 1)
            printf("FAIL online cpu%d\n", cpu);
        close(fd);
    }
    printf("online %ld\n", sysconf(_SC_NPROCESSORS_ONLN));
}

static uint64_t now_ms(clockid_t c) {
    struct timespec ts;
    clock_gettime(c, &ts);
    return (uint64_t)ts.tv_sec * 1000 + (uint64_t)ts.tv_nsec / 1000000;
}

static volatile uint64_t net_ok, net_reset, net_timeout, vsock_ok, vsock_ended;
static volatile uint64_t idle_ended, down_bytes, down_ended;
static uint16_t ports[3];

static int if_set(int fd, unsigned long req, const char *addr) {
    struct ifreq ifr = {0};
    strcpy(ifr.ifr_name, "eth0");
    struct sockaddr_in *sin = (struct sockaddr_in *)&ifr.ifr_addr;
    sin->sin_family = AF_INET;
    inet_pton(AF_INET, addr, &sin->sin_addr);
    return ioctl(fd, req, &ifr);
}

static int eth0_up(void) {
    int fd = socket(AF_INET, SOCK_DGRAM, 0);
    struct ifreq ifr = {0};
    strcpy(ifr.ifr_name, "eth0");
    if (fd < 0 || if_set(fd, SIOCSIFADDR, "10.0.2.2") || if_set(fd, SIOCSIFNETMASK, "255.255.255.0") ||
        ioctl(fd, SIOCGIFFLAGS, &ifr))
        return -1;
    ifr.ifr_flags |= IFF_UP | IFF_RUNNING;
    int r = ioctl(fd, SIOCSIFFLAGS, &ifr);
    close(fd);
    return r;
}

/* Reads or writes exactly n bytes; 0 on success, else errno (or -1 on EOF). */
static int full(int fd, void *buf, size_t n, int writing) {
    for (size_t done = 0; done < n;) {
        ssize_t r = writing ? send(fd, (char *)buf + done, n - done, MSG_NOSIGNAL)
                            : recv(fd, (char *)buf + done, n - done, 0);
        if (r == 0)
            return -1;
        if (r < 0) {
            if (errno == EINTR)
                continue;
            return errno;
        }
        done += (size_t)r;
    }
    return 0;
}

/* Connects to the host at 10.0.2.1:port, retrying every 100 ms. */
static int dial(uint16_t port, int timeout_s) {
    struct sockaddr_in to = {.sin_family = AF_INET, .sin_port = htons(port)};
    inet_pton(AF_INET, "10.0.2.1", &to.sin_addr);
    struct timeval tmo = {.tv_sec = timeout_s};
    for (;;) {
        int fd = socket(AF_INET, SOCK_STREAM, 0);
        if (timeout_s) {
            setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tmo, sizeof tmo);
            setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &tmo, sizeof tmo);
        }
        if (!connect(fd, (struct sockaddr *)&to, sizeof to))
            return fd;
        close(fd);
        usleep(100000);
    }
}

static void *idle_client(void *arg) {
    (void)arg;
    for (;;) {
        int fd = dial(ports[1], 0);
        char c;
        ssize_t r = recv(fd, &c, 1, 0);
        idle_ended++;
        printf("idle-end errno %d\n", r < 0 ? errno : -1);
        close(fd);
    }
}

static void *download_client(void *arg) {
    (void)arg;
    static uint64_t buf[8192];
    for (;;) {
        int fd = dial(ports[2], 0);
        uint64_t next = 0;
        size_t have = 0; /* bytes of a partial word in carry */
        ssize_t r;
        while ((r = recv(fd, (char *)buf + have, sizeof buf - have, 0)) > 0) {
            size_t len = have + (size_t)r, words = len / 8;
            for (size_t i = 0; i < words; i++, next++)
                if (buf[i] != next) {
                    printf("FAIL download word %llu got %llu\n", (unsigned long long)next,
                           (unsigned long long)buf[i]);
                    fails++;
                    next = buf[i];
                }
            have = len % 8;
            memmove(buf, (char *)buf + words * 8, have);
            down_bytes += (uint64_t)r;
        }
        down_ended++;
        printf("download-end errno %d after %llu words\n", r < 0 ? errno : -1,
               (unsigned long long)next);
        close(fd);
    }
}

static void *tcp_client(void *arg) {
    (void)arg;
    for (uint64_t seq = 1;;) {
        int fd = dial(ports[0], 5);
        printf("net-up\n");
        uint64_t last_ok = now_ms(CLOCK_MONOTONIC);
        for (;;) {
            uint64_t got = 0;
            int e = full(fd, &seq, 8, 1);
            if (!e)
                e = full(fd, &got, 8, 0);
            if (!e && got != seq) {
                printf("FAIL net echo %llu want %llu\n", (unsigned long long)got,
                       (unsigned long long)seq);
                fails++;
            }
            if (e) {
                if (e == ECONNRESET || e == EPIPE)
                    net_reset++;
                else if (e == EAGAIN)
                    net_timeout++;
                printf("net-end errno %d after %llu ms without echo\n", e,
                       (unsigned long long)(now_ms(CLOCK_MONOTONIC) - last_ok));
                break;
            }
            seq++;
            net_ok++;
            last_ok = now_ms(CLOCK_MONOTONIC);
            usleep(10000);
        }
        close(fd);
    }
}

static void *vsock_server(void *arg) {
    (void)arg;
    int lfd = socket(AF_VSOCK, SOCK_STREAM, 0);
    struct sockaddr_vm at = {.svm_family = AF_VSOCK, .svm_cid = VMADDR_CID_ANY, .svm_port = 5000};
    if (lfd < 0 || bind(lfd, (struct sockaddr *)&at, sizeof at) || listen(lfd, 4)) {
        printf("FAIL vsock listen errno %d\n", errno);
        fails++;
        return NULL;
    }
    for (;;) {
        int fd = accept(lfd, NULL, NULL);
        if (fd < 0)
            continue;
        uint64_t word;
        int e;
        while (!(e = full(fd, &word, 8, 0)) && !(e = full(fd, &word, 8, 1)))
            vsock_ok++;
        vsock_ended++;
        printf("vsock-end errno %d\n", e);
        close(fd);
    }
}

int main(void) {
    mkdir("/dev", 0755);
    mount("devtmpfs", "/dev", "devtmpfs", 0, NULL);
    int con = open("/dev/hvc0", O_RDWR);
    if (con >= 0) {
        dup2(con, 0);
        dup2(con, 1);
        dup2(con, 2);
    }
    setvbuf(stdout, NULL, _IOLBF, 0);
    printf("spike-init: up, %ld cpus online\n", sysconf(_SC_NPROCESSORS_ONLN));

    static struct worker workers[THREADS];
    const char *devs[THREADS] = {"/dev/vda", "/dev/vda", "/dev/arcboxhvc0", "/dev/arcboxhvc0"};
    /* VZ has no HVC block path: all four threads use virtio-blk. */
    if (access("/dev/arcboxhvc0", F_OK)) {
        devs[2] = devs[3] = "/dev/vda";
        printf("spike-init: no /dev/arcboxhvc0, threads 2-3 use /dev/vda\n");
    }
    for (uint64_t i = 0; i < THREADS; i++) {
        workers[i].id = i;
        workers[i].dev = devs[i];
        pthread_t t;
        pthread_create(&t, NULL, run, &workers[i]);
    }

    const char *net = getenv("spike_net");
    if (net) {
        if (eth0_up()) {
            printf("FAIL eth0 errno %d\n", errno);
            fails++;
        }
        if (sscanf(net, "%hu,%hu,%hu", &ports[0], &ports[1], &ports[2]) != 3) {
            printf("FAIL spike_net=%s\n", net);
            fails++;
        }
        pthread_t t;
        pthread_create(&t, NULL, tcp_client, NULL);
        pthread_create(&t, NULL, idle_client, NULL);
        pthread_create(&t, NULL, download_client, NULL);
        pthread_create(&t, NULL, vsock_server, NULL);
    }

    const char *online_at = getenv("spike_online_at");
    uint64_t online_tick = online_at ? strtoull(online_at, NULL, 10) : 0;
    uint64_t start = now_ms(CLOCK_MONOTONIC);
    struct timespec next;
    clock_gettime(CLOCK_MONOTONIC, &next);
    for (uint64_t n = 1;; n++) {
        next.tv_sec += 1;
        clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &next, NULL);
        char extra[200] = "";
        if (net)
            snprintf(extra, sizeof extra,
                     " net %llu %llu %llu vsock %llu %llu idle %llu down %llu %llu",
                     (unsigned long long)net_ok, (unsigned long long)net_reset,
                     (unsigned long long)net_timeout, (unsigned long long)vsock_ok,
                     (unsigned long long)vsock_ended, (unsigned long long)idle_ended,
                     (unsigned long long)(down_bytes >> 10), (unsigned long long)down_ended);
        printf("tick %llu mono_ms %llu real %llu ops %llu %llu %llu %llu fails %llu%s\n",
               (unsigned long long)n, (unsigned long long)(now_ms(CLOCK_MONOTONIC) - start),
               (unsigned long long)(now_ms(CLOCK_REALTIME) / 1000), (unsigned long long)ops[0],
               (unsigned long long)ops[1], (unsigned long long)ops[2], (unsigned long long)ops[3],
               (unsigned long long)fails, extra);
        if (n == online_tick)
            online_all();
    }
}
