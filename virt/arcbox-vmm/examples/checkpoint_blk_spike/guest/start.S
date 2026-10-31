// Entry, stacks and exception vectors for the multi-vCPU blk checkpoint guest.
//
// Every vCPU enters at _start at EL1h with the MMU off. The host passes:
//   x0 = CPU index, x1 = this CPU's GIC redistributor base,
//   x2 = GIC distributor base, x3 = virtio-mmio base of the blk device,
//   x4 = INTID of the blk device's SPI, x5 = number of vCPUs.

        .equ STACK_BASE, 0x40200000
        .equ STACK_SIZE, 0x10000
        .equ TRAP_MMIO,  0x09000008

        .section .text.start
        .global _start
_start:
        mov     x9, #STACK_SIZE
        madd    x9, x0, x9, x9
        ldr     x10, =STACK_BASE
        add     sp, x10, x9               // top of this CPU's stack
        adr     x9, vectors
        msr     vbar_el1, x9
        mov     x9, #(3 << 20)            // CPACR_EL1.FPEN: no FP/SIMD traps
        msr     cpacr_el1, x9
        isb
        bl      cpu_main
1:      b       1b

irq_entry:
        // Interrupts stay masked in the handler, so ELR/SPSR need no saving.
        sub     sp, sp, #(20 * 8)
        stp     x0, x1, [sp, #0]
        stp     x2, x3, [sp, #16]
        stp     x4, x5, [sp, #32]
        stp     x6, x7, [sp, #48]
        stp     x8, x9, [sp, #64]
        stp     x10, x11, [sp, #80]
        stp     x12, x13, [sp, #96]
        stp     x14, x15, [sp, #112]
        stp     x16, x17, [sp, #128]
        stp     x18, x30, [sp, #144]
        bl      irq_handler
        ldp     x0, x1, [sp, #0]
        ldp     x2, x3, [sp, #16]
        ldp     x4, x5, [sp, #32]
        ldp     x6, x7, [sp, #48]
        ldp     x8, x9, [sp, #64]
        ldp     x10, x11, [sp, #80]
        ldp     x12, x13, [sp, #96]
        ldp     x14, x15, [sp, #112]
        ldp     x16, x17, [sp, #128]
        ldp     x18, x30, [sp, #144]
        add     sp, sp, #(20 * 8)
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
        b       irq_entry                 // current EL, SPx, IRQ (+0x280)
        .balign 0x80
        .rept   10
        b       unexpected
        .balign 0x80
        .endr
