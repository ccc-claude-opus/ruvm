// Bare-metal SMP start-up (-bios none: every hart starts here with a0 = hartid).
    .section .text.start
    .globl _start
_start:
    mv   s0, a0
    la   t0, stack_top
    slli t1, s0, 13            // 8 KiB per hart
    sub  sp, t0, t1
    la   t0, m_trap
    csrw mtvec, t0
    mv   a0, s0
    call main
1:  wfi
    j    1b

// machine software interrupt: clear this hart's msip, flag it, return
    .balign 4
m_trap:
    csrr t0, mcause
    li   t1, 0x8000000000000003
    bne  t0, t1, 2f
    csrr t0, mhartid
    li   t1, 0x2000000         // CLINT
    slli t2, t0, 2
    add  t1, t1, t2
    sw   zero, 0(t1)
    la   t1, ipi_got
    slli t2, t0, 2
    add  t1, t1, t2
    li   t2, 1
    sw   t2, 0(t1)
    mret
2:  j    2b
