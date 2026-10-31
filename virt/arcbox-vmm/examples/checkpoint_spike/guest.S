// Bare-metal EL1 guest for the HV checkpoint spike.
//
// Loaded at RAM_BASE with the MMU off. Entry registers from the host:
//   x0 = GIC distributor base, x1 = this vCPU's redistributor base.
//
// The guest arms the virtual timer at 1 kHz through the in-kernel GICv3
// (PPI 27) and idles in WFI. Every interrupt bumps a tick counter; every
// wakeup bumps a loop counter and checks invariants held in registers the
// host must save and restore: x19/x20 (x20 == 3 * x19), x21/x22 and the
// 128-bit SIMD register v5. A broken invariant stores to FAIL_MMIO, which
// is unmapped and exits to the host.
//
// Shared data page at DATA (little-endian u64s):
//   +0  timer ticks          +8  main-loop wakeups
//   +16 CNTVCT_EL0 at the last tick
//   +24 timer period (CNTFRQ / 1000)
//   +32 1 once initialised

        .equ DATA,          0x40100000
        .equ STACK_TOP,     0x40200000
        .equ FAIL_MMIO,     0x09000000
        .equ TRAP_MMIO,     0x09000008

        .text
        .global _start
_start:
        ldr     x2, =STACK_TOP
        mov     sp, x2
        adr     x2, vectors
        msr     vbar_el1, x2
        mov     x2, #(3 << 20)            // CPACR_EL1.FPEN: no FP/SIMD traps
        msr     cpacr_el1, x2
        isb

        // Distributor: ARE_NS | EnableGrp1A | EnableGrp1.
        mov     w2, #0x13
        str     w2, [x0]

        // Redistributor: wake the CPU interface.
        ldr     w2, [x1, #0x14]
        bic     w2, w2, #2
        str     w2, [x1, #0x14]
1:      ldr     w2, [x1, #0x14]
        tbnz    w2, #2, 1b

        // SGI/PPI frame: PPI 27 in group 1, priority 0xa0, enabled.
        add     x3, x1, #0x10000
        ldr     w2, [x3, #0x80]
        orr     w2, w2, #(1 << 27)
        str     w2, [x3, #0x80]
        mov     w2, #0xa0
        strb    w2, [x3, #0x41b]
        mov     w2, #(1 << 27)
        str     w2, [x3, #0x100]

        // CPU interface: system-register access, open priority mask, group 1 on.
        mrs     x2, S3_0_C12_C12_5        // ICC_SRE_EL1
        orr     x2, x2, #1
        msr     S3_0_C12_C12_5, x2
        isb
        mov     x2, #0xff
        msr     S3_0_C4_C6_0, x2          // ICC_PMR_EL1
        mov     x2, #1
        msr     S3_0_C12_C12_7, x2        // ICC_IGRPEN1_EL1
        isb

        // Virtual timer at 1 kHz.
        ldr     x23, =DATA
        mrs     x2, cntfrq_el0
        mov     x3, #1000
        udiv    x2, x2, x3
        str     x2, [x23, #24]
        msr     cntv_tval_el0, x2
        mov     x2, #1
        msr     cntv_ctl_el0, x2

        // Register invariants the checkpoint must carry.
        mov     x19, #0
        mov     x20, #0
        ldr     x21, =0x0123456789abcdef
        ldr     x22, =0xfedcba9876543210
        fmov    d5, x21
        ins     v5.d[1], x22

        mov     x2, #1
        str     x2, [x23, #32]
        msr     daifclr, #2

loop:
        wfi
        add     x19, x19, #1
        add     x20, x20, #3
        add     x9, x19, x19, lsl #1
        cmp     x9, x20
        b.ne    fail
        umov    x9, v5.d[0]
        cmp     x9, x21
        b.ne    fail
        umov    x9, v5.d[1]
        cmp     x9, x22
        b.ne    fail
        str     x19, [x23, #8]
        b       loop

fail:
        ldr     x9, =FAIL_MMIO
        str     x19, [x9]
        b       fail

irq:
        stp     x0, x1, [sp, #-16]!
        stp     x2, x3, [sp, #-16]!
        mrs     x0, S3_0_C12_C12_0        // ICC_IAR1_EL1
        cmp     x0, #27
        b.ne    2f
        ldr     x1, =DATA
        ldr     x2, [x1]
        add     x2, x2, #1
        str     x2, [x1]
        mrs     x2, cntvct_el0
        str     x2, [x1, #16]
        ldr     x3, [x1, #24]           // next deadline: drift-free 1 kHz
        mrs     x2, cntv_cval_el0
        add     x2, x2, x3
        msr     cntv_cval_el0, x2
2:      msr     S3_0_C12_C12_1, x0        // ICC_EOIR1_EL1
        ldp     x2, x3, [sp], #16
        ldp     x0, x1, [sp], #16
        eret

unexpected:
        mrs     x0, esr_el1
        ldr     x1, =TRAP_MMIO
        str     x0, [x1]
        b       unexpected

        .ltorg

        .balign 2048
vectors:
        .rept   5
        b       unexpected
        .balign 0x80
        .endr
        b       irq                       // current EL, SPx, IRQ (+0x280)
        .balign 0x80
        .rept   10
        b       unexpected
        .balign 0x80
        .endr
