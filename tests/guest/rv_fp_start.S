    .section .text.start
    .globl _start
_start:
    la   sp, stack_top
    li   t0, 1 << 13           // mstatus.FS = Initial
    csrs mstatus, t0
    call main
    li   t0, 0x100000          // test finisher: pass
    li   t1, 0x5555
    sw   t1, 0(t0)
1:  j    1b
