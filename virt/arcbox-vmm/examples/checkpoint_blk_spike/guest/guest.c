// Multi-vCPU virtio-blk workload for the HV checkpoint spike.
//
// Every vCPU takes 1 kHz virtual-timer interrupts through its own GIC
// redistributor and drives its own virtqueue of one virtio-blk device:
// write a 4 KiB block stamped with a sequence number, then read back a
// different block and check it against the sequence number last written
// there, with a FLUSH after every fourth write. CPU 0 negotiates the device and sets up every queue, and takes the
// device's SPI. Any failure stores a code to FAIL_MMIO, which exits to the
// host. Runs with the MMU off, so every access is Device memory: aligned,
// one access per volatile load or store.

typedef unsigned long u64;
typedef unsigned int u32;
typedef unsigned short u16;
typedef unsigned char u8;

#define FAIL_MMIO 0x09000000UL

#define DATA 0x40100000UL
#define SHARED_READY (*(volatile u64 *)(DATA + 0x00))
#define SPI_COUNT (*(volatile u64 *)(DATA + 0x08))
#define PERIOD (*(volatile u64 *)(DATA + 0x10))
#define VIRTIO_BASE (*(volatile u64 *)(DATA + 0x18))
#define BLK_INTID (*(volatile u64 *)(DATA + 0x20))

struct cpu_stat {
    volatile u64 ticks;  // timer interrupts taken
    volatile u64 ios;    // completed requests
    volatile u64 cntvct; // CNTVCT_EL0 at the last tick
    volatile u64 seq;    // last sequence number written
    volatile u64 pad[4];
};
#define STATS ((struct cpu_stat *)(DATA + 0x100))
// Per CPU: the sequence number last written to each of its blocks.
#define EXPECTED(cpu) ((volatile u64 *)(DATA + 0x10000 + (cpu) * 0x1000))

#define QSIZE 8
#define QUEUE(q) (0x40300000UL + (u64)(q) * 0x4000)
#define AVAIL(q) (QUEUE(q) + 0x1000)
#define USED(q) (QUEUE(q) + 0x2000)
#define BUF(cpu) (0x40400000UL + (u64)(cpu) * 0x10000)
#define BUF_HDR(cpu) BUF(cpu)
#define BUF_STATUS(cpu) (BUF(cpu) + 0x100)
#define BUF_DATA(cpu) (BUF(cpu) + 0x1000)

#define BLOCKS 128
#define BLOCK_BYTES 4096
#define SECTORS_PER_BLOCK (BLOCK_BYTES / 512)

// virtio-mmio v2 registers.
#define VM_MAGIC 0x000
#define VM_VERSION 0x004
#define VM_DEVICE_ID 0x008
#define VM_DEVICE_FEATURES 0x010
#define VM_DEVICE_FEATURES_SEL 0x014
#define VM_DRIVER_FEATURES 0x020
#define VM_DRIVER_FEATURES_SEL 0x024
#define VM_QUEUE_SEL 0x030
#define VM_QUEUE_NUM_MAX 0x034
#define VM_QUEUE_NUM 0x038
#define VM_QUEUE_READY 0x044
#define VM_QUEUE_NOTIFY 0x050
#define VM_INTERRUPT_STATUS 0x060
#define VM_INTERRUPT_ACK 0x064
#define VM_STATUS 0x070
#define VM_QUEUE_DESC_LOW 0x080
#define VM_QUEUE_DESC_HIGH 0x084
#define VM_QUEUE_DRIVER_LOW 0x090
#define VM_QUEUE_DRIVER_HIGH 0x094
#define VM_QUEUE_DEVICE_LOW 0x0a0
#define VM_QUEUE_DEVICE_HIGH 0x0a4
#define VM_CONFIG 0x100

#define F_BLK_MQ (1UL << 12)
#define F_EVENT_IDX (1UL << 29)
#define F_VERSION_1 (1UL << 32)

#define T_IN 0
#define T_OUT 1
#define T_FLUSH 4

#define DESC_NEXT 1
#define DESC_WRITE 2

#define MRS(reg)                                          \
    ({                                                    \
        u64 v_;                                           \
        __asm__ volatile("mrs %0, " #reg : "=r"(v_));     \
        v_;                                               \
    })
#define MSR(reg, v) __asm__ volatile("msr " #reg ", %0" ::"r"((u64)(v)))

// Single-instruction MMIO accessors: the host decodes them from the
// data-abort syndrome, which needs plain register-offset forms.
static inline u32 rd32(u64 a) {
    u32 v;
    __asm__ volatile("ldr %w0, [%1]" : "=r"(v) : "r"(a) : "memory");
    return v;
}
static inline void wr32(u64 a, u32 v) {
    __asm__ volatile("str %w0, [%1]" ::"r"(v), "r"(a) : "memory");
}
static inline void wr64(u64 a, u64 v) {
    __asm__ volatile("str %0, [%1]" ::"r"(v), "r"(a) : "memory");
}
#define V16(a) (*(volatile u16 *)(a))
#define V32(a) (*(volatile u32 *)(a))
#define V64(a) (*(volatile u64 *)(a))
#define V8(a) (*(volatile u8 *)(a))

static void __attribute__((noreturn)) fail(u64 cpu, u64 code, u64 detail) {
    for (;;) {
        wr64(FAIL_MMIO, (cpu << 56) | (code << 48) | (detail & 0xffffffffffffUL));
    }
}

static u64 this_cpu(void) { return MRS(mpidr_el1) & 0xff; }

void irq_handler(void) {
    u64 cpu = this_cpu();
    u64 intid = MRS(S3_0_C12_C12_0) & 0xffffff; // ICC_IAR1_EL1
    if (intid == 27) {
        STATS[cpu].ticks++;
        STATS[cpu].cntvct = MRS(cntvct_el0);
        MSR(cntv_cval_el0, MRS(cntv_cval_el0) + PERIOD);
    } else if (intid == BLK_INTID) {
        u64 base = VIRTIO_BASE;
        u32 status = rd32(base + VM_INTERRUPT_STATUS);
        wr32(base + VM_INTERRUPT_ACK, status);
        SPI_COUNT++;
    }
    if (intid < 1020) {
        MSR(S3_0_C12_C12_1, intid); // ICC_EOIR1_EL1
    }
}

static void gic_cpu_init(u64 gicr) {
    u64 sgi = gicr + 0x10000;
    V32(sgi + 0x80) |= 1u << 27; // GICR_IGROUPR0: PPI 27 in group 1
    V8(sgi + 0x400 + 27) = 0xa0; // GICR_IPRIORITYR
    V32(sgi + 0x100) = 1u << 27; // GICR_ISENABLER0
    MSR(S3_0_C12_C12_5, MRS(S3_0_C12_C12_5) | 1); // ICC_SRE_EL1.SRE
    __asm__ volatile("isb");
    MSR(S3_0_C4_C6_0, 0xff); // ICC_PMR_EL1
    MSR(S3_0_C12_C12_7, 1);  // ICC_IGRPEN1_EL1
    __asm__ volatile("isb");
}

static void gic_dist_init(u64 gicd, u64 intid) {
    wr32(gicd, 0x13); // GICD_CTLR: ARE_NS | EnableGrp1A | EnableGrp1
    u64 word = (intid / 32) * 4, bit = 1u << (intid % 32);
    V32(gicd + 0x80 + word) |= bit; // GICD_IGROUPR
    V8(gicd + 0x400 + intid) = 0xa0; // GICD_IPRIORITYR
    wr64(gicd + 0x6000 + intid * 8, 0); // GICD_IROUTER: CPU 0
    V32(gicd + 0x100 + word) = bit; // GICD_ISENABLER
}

static void timer_init(void) {
    if (PERIOD == 0) {
        PERIOD = MRS(cntfrq_el0) / 1000;
    }
    MSR(cntv_cval_el0, MRS(cntvct_el0) + PERIOD);
    MSR(cntv_ctl_el0, 1);
}

static void virtio_init(u64 base, u64 ncpu) {
    if (rd32(base + VM_MAGIC) != 0x74726976 || rd32(base + VM_VERSION) != 2 ||
        rd32(base + VM_DEVICE_ID) != 2) {
        fail(0, 1, rd32(base + VM_MAGIC));
    }
    wr32(base + VM_STATUS, 0);
    wr32(base + VM_STATUS, 1);
    wr32(base + VM_STATUS, 1 | 2);
    wr32(base + VM_DEVICE_FEATURES_SEL, 0);
    u64 features = rd32(base + VM_DEVICE_FEATURES);
    wr32(base + VM_DEVICE_FEATURES_SEL, 1);
    features |= (u64)rd32(base + VM_DEVICE_FEATURES) << 32;
    // A one-queue device does not offer MQ (nor the num_queues field).
    u64 want = (ncpu > 1 ? F_BLK_MQ : 0) | F_EVENT_IDX | F_VERSION_1;
    if ((features & want) != want) {
        fail(0, 2, features);
    }
    wr32(base + VM_DRIVER_FEATURES_SEL, 0);
    wr32(base + VM_DRIVER_FEATURES, (u32)want);
    wr32(base + VM_DRIVER_FEATURES_SEL, 1);
    wr32(base + VM_DRIVER_FEATURES, (u32)(want >> 32));
    wr32(base + VM_STATUS, 1 | 2 | 8);
    if (!(rd32(base + VM_STATUS) & 8)) {
        fail(0, 3, 0);
    }
    u64 nq = ncpu > 1 ? rd32(base + VM_CONFIG + 32) >> 16 : 1; // num_queues at config offset 34
    if (nq < ncpu) {
        fail(0, 4, nq);
    }
    for (u64 q = 0; q < ncpu; q++) {
        wr32(base + VM_QUEUE_SEL, (u32)q);
        if (rd32(base + VM_QUEUE_NUM_MAX) < QSIZE) {
            fail(0, 5, q);
        }
        wr32(base + VM_QUEUE_NUM, QSIZE);
        wr32(base + VM_QUEUE_DESC_LOW, (u32)QUEUE(q));
        wr32(base + VM_QUEUE_DESC_HIGH, 0);
        wr32(base + VM_QUEUE_DRIVER_LOW, (u32)AVAIL(q));
        wr32(base + VM_QUEUE_DRIVER_HIGH, 0);
        wr32(base + VM_QUEUE_DEVICE_LOW, (u32)USED(q));
        wr32(base + VM_QUEUE_DEVICE_HIGH, 0);
        wr32(base + VM_QUEUE_READY, 1);
    }
    wr32(base + VM_STATUS, 1 | 2 | 8 | 4);
}

static void set_desc(u64 q, u64 i, u64 addr, u32 len, u16 flags, u16 next) {
    u64 d = QUEUE(q) + i * 16;
    V64(d) = addr;
    V32(d + 8) = len;
    V16(d + 12) = flags;
    V16(d + 14) = next;
}

// One request on this CPU's queue, completed before returning.
static void blk_io(u64 cpu, u32 type, u64 sector) {
    u64 q = cpu;
    V32(BUF_HDR(cpu)) = type;
    V32(BUF_HDR(cpu) + 4) = 0;
    V64(BUF_HDR(cpu) + 8) = sector;
    V8(BUF_STATUS(cpu)) = 0xff;
    if (type == T_FLUSH) {
        set_desc(q, 0, BUF_HDR(cpu), 16, DESC_NEXT, 1);
        set_desc(q, 1, BUF_STATUS(cpu), 1, DESC_WRITE, 0);
    } else {
        set_desc(q, 0, BUF_HDR(cpu), 16, DESC_NEXT, 1);
        set_desc(q, 1, BUF_DATA(cpu), BLOCK_BYTES, DESC_NEXT | (type == T_IN ? DESC_WRITE : 0), 2);
        set_desc(q, 2, BUF_STATUS(cpu), 1, DESC_WRITE, 0);
    }

    u16 used_before = V16(USED(q) + 2);
    u16 avail = V16(AVAIL(q) + 2);
    V16(AVAIL(q) + 4 + (avail % QSIZE) * 2) = 0;
    V16(AVAIL(q) + 4 + QSIZE * 2) = used_before; // used_event: interrupt on this one
    __asm__ volatile("dmb sy" ::: "memory");
    V16(AVAIL(q) + 2) = (u16)(avail + 1);
    __asm__ volatile("dsb sy" ::: "memory");
    wr32(VIRTIO_BASE + VM_QUEUE_NOTIFY, (u32)q);
    while (V16(USED(q) + 2) == used_before) {
    }
    __asm__ volatile("dmb sy" ::: "memory");
    if (V8(BUF_STATUS(cpu)) != 0) {
        fail(cpu, 6, V8(BUF_STATUS(cpu)));
    }
    STATS[cpu].ios++;
}

static u64 pattern(u64 seq, u64 i) { return (seq * 0x9e3779b97f4a7c15UL) ^ (i * 0x100000001b3UL); }

static void workload(u64 cpu) {
    volatile u64 *data = (volatile u64 *)BUF_DATA(cpu);
    for (;;) {
        u64 seq = STATS[cpu].seq + 1;
        u64 blk = seq % BLOCKS;
        data[0] = seq;
        data[1] = cpu;
        data[2] = blk;
        for (u64 i = 3; i < BLOCK_BYTES / 8; i++) {
            data[i] = pattern(seq, i);
        }
        blk_io(cpu, T_OUT, (cpu * BLOCKS + blk) * SECTORS_PER_BLOCK);
        // An fsync on the host: a request that stays in flight for long
        // enough that a pause regularly lands during one.
        if (seq % 4 == 0) {
            blk_io(cpu, T_FLUSH, 0);
        }
        EXPECTED(cpu)[blk] = seq;
        STATS[cpu].seq = seq;

        u64 vb = (seq * 7 + 3) % BLOCKS;
        u64 want = EXPECTED(cpu)[vb];
        if (want == 0) {
            continue;
        }
        blk_io(cpu, T_IN, (cpu * BLOCKS + vb) * SECTORS_PER_BLOCK);
        if (data[0] != want || data[1] != cpu || data[2] != vb) {
            fail(cpu, 7, data[0]);
        }
        for (u64 i = 3; i < BLOCK_BYTES / 8; i++) {
            if (data[i] != pattern(want, i)) {
                fail(cpu, 8, i);
            }
        }
    }
}

void cpu_main(u64 cpu, u64 gicr, u64 gicd, u64 virtio, u64 intid, u64 ncpu) {
    gic_cpu_init(gicr);
    if (cpu == 0) {
        VIRTIO_BASE = virtio;
        BLK_INTID = intid;
        gic_dist_init(gicd, intid);
        virtio_init(virtio, ncpu);
        __asm__ volatile("dsb sy" ::: "memory");
        SHARED_READY = 1;
    } else {
        while (SHARED_READY == 0) {
        }
    }
    timer_init();
    __asm__ volatile("msr daifclr, #2");
    workload(cpu);
}
