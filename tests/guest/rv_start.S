    .section .text.start
    .globl _start
_start:
    la sp, stack_top
    call main
    li a7, 93
    ecall
1:  j 1b
    .globl out_char
out_char:
    li t0, 0x10000000
    sb a0, 0(t0)
    ret
