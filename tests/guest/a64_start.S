    .section .text.start
    .globl _start
_start:
    adrp x1, stack_top
    add  x1, x1, :lo12:stack_top
    mov  sp, x1
    bl   main
    mov  x8, #93
    svc  #0
1:  b 1b
    .globl out_char
out_char:
    mov  x1, #0x09000000
    strb w0, [x1]
    ret
