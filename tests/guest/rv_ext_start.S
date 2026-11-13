// M-mode driver for rv_ext.c: trap stub (logs faults through m_trap_c) and enter_s().
    .section .text.start
    .globl _start
_start:
    la   sp, stack_top
    la   t0, m_trap
    csrw mtvec, t0
    call main
    li   t0, 0x100000
    li   t1, 0x5555
    sw   t1, 0(t0)
1:  j    1b

    .balign 4
m_trap:
    addi sp, sp, -(8*32)
    sd   ra, 8*1(sp)
    sd   t0, 8*5(sp)
    sd   t1, 8*6(sp)
    sd   t2, 8*7(sp)
    sd   a0, 8*10(sp)
    sd   a1, 8*11(sp)
    sd   a2, 8*12(sp)
    sd   a3, 8*13(sp)
    sd   a4, 8*14(sp)
    sd   a5, 8*15(sp)
    sd   a6, 8*16(sp)
    sd   a7, 8*17(sp)
    sd   t3, 8*28(sp)
    sd   t4, 8*29(sp)
    sd   t5, 8*30(sp)
    sd   t6, 8*31(sp)
    csrr a0, mcause
    csrr a1, mtval
    csrr a2, mepc
    mv   a3, ra                 // interrupted ra (for "return from a faulting call")
    call m_trap_c               // returns the new mepc, or 1 = leave S-mode
    li   t0, 1
    beq  a0, t0, leave_s
    csrw mepc, a0
    ld   ra, 8*1(sp)
    ld   t0, 8*5(sp)
    ld   t1, 8*6(sp)
    ld   t2, 8*7(sp)
    ld   a0, 8*10(sp)
    ld   a1, 8*11(sp)
    ld   a2, 8*12(sp)
    ld   a3, 8*13(sp)
    ld   a4, 8*14(sp)
    ld   a5, 8*15(sp)
    ld   a6, 8*16(sp)
    ld   a7, 8*17(sp)
    ld   t3, 8*28(sp)
    ld   t4, 8*29(sp)
    ld   t5, 8*30(sp)
    ld   t6, 8*31(sp)
    addi sp, sp, 8*32
    mret

// enter_s(fn): run fn in S-mode; S code returns to M with ecall
    .globl enter_s
enter_s:
    la   t0, kctx
    sd   ra, 0(t0)
    sd   sp, 8(t0)
    sd   s0, 16(t0)
    sd   s1, 24(t0)
    csrw mepc, a0
    li   t1, 3 << 11
    csrc mstatus, t1
    li   t1, 1 << 11            // MPP = S
    csrs mstatus, t1
    la   sp, s_stack_top
    mret
leave_s:
    la   t0, kctx
    ld   ra, 0(t0)
    ld   sp, 8(t0)
    ld   s0, 16(t0)
    ld   s1, 24(t0)
    ret

    .section .bss
    .balign 16
kctx: .space 64
s_stack: .space 4096
    .globl s_stack_top
s_stack_top:
