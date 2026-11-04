// Bare-metal start-up for a64_priv.c: EL1 entry, exception vectors, EL0 entry/exit.
// Runs unchanged on `qemu-system-aarch64 -M virt -cpu max` and on ruvm.

    .section .text.start
    .globl _start
_start:
    ldr  x1, =stack_top
    mov  sp, x1
    bl   main
    ldr  x0, =0x84000008       // PSCI SYSTEM_OFF
    hvc  #0
1:  b    1b

// 16 vectors: save x0-x30 + 8 spare bytes, call exc_handler(frame, index), restore, ERET.
.macro VEC idx
    .balign 128
    sub  sp, sp, #256
    stp  x0, x1, [sp, #0]
    stp  x2, x3, [sp, #16]
    stp  x4, x5, [sp, #32]
    stp  x6, x7, [sp, #48]
    stp  x8, x9, [sp, #64]
    stp  x10, x11, [sp, #80]
    stp  x12, x13, [sp, #96]
    stp  x14, x15, [sp, #112]
    stp  x16, x17, [sp, #128]
    stp  x18, x19, [sp, #144]
    stp  x20, x21, [sp, #160]
    stp  x22, x23, [sp, #176]
    stp  x24, x25, [sp, #192]
    stp  x26, x27, [sp, #208]
    stp  x28, x29, [sp, #224]
    str  x30, [sp, #240]
    mov  x0, sp
    mov  x1, #\idx
    bl   exc_handler
    b    exc_return
.endm

    .text
    .balign 2048
    .globl vectors
vectors:
    VEC 0
    VEC 1
    VEC 2
    VEC 3
    VEC 4
    VEC 5
    VEC 6
    VEC 7
    VEC 8
    VEC 9
    VEC 10
    VEC 11
    VEC 12
    VEC 13
    VEC 14
    VEC 15

exc_return:
    ldp  x0, x1, [sp, #0]
    ldp  x2, x3, [sp, #16]
    ldp  x4, x5, [sp, #32]
    ldp  x6, x7, [sp, #48]
    ldp  x8, x9, [sp, #64]
    ldp  x10, x11, [sp, #80]
    ldp  x12, x13, [sp, #96]
    ldp  x14, x15, [sp, #112]
    ldp  x16, x17, [sp, #128]
    ldp  x18, x19, [sp, #144]
    ldp  x20, x21, [sp, #160]
    ldp  x22, x23, [sp, #176]
    ldp  x24, x25, [sp, #192]
    ldp  x26, x27, [sp, #208]
    ldp  x28, x29, [sp, #224]
    ldr  x30, [sp, #240]
    add  sp, sp, #256
    eret

// enter_user(pc, sp): ERET to EL0t; returns when the user program calls svc #0.
    .globl enter_user
enter_user:
    adr  x9, kctx
    stp  x29, x30, [x9]
    mov  x10, sp
    stp  x10, x19, [x9, #16]
    stp  x20, x21, [x9, #32]
    msr  sp_el0, x1
    msr  elr_el1, x0
    msr  spsr_el1, xzr
    eret

    .globl user_exit
user_exit:                      // exc_handler points ELR_EL1 here with SPSR = EL1h
    adr  x9, kctx
    ldp  x29, x30, [x9]
    ldp  x10, x19, [x9, #16]
    mov  sp, x10
    ldp  x20, x21, [x9, #32]
    ret

    .data
    .balign 16
kctx:
    .space 48
