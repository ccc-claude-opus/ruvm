// S-mode payload in Linux Image format: exercises the SBI interface.
// Same output with ruvm's built-in SBI, OpenSBI on ruvm, and OpenSBI on QEMU.
#define SBI(eid, fid) li a7, eid; li a6, fid; ecall

    .section .text.start
    .globl _start
_start:
    .option push
    .option norvc
    j    entry                  // code0 (4 bytes: keep the header aligned)
    .option pop
    .word 0                     // code1
    .dword 0x200000             // text_offset
    .dword _end - _start        // image_size
    .dword 0                    // flags
    .word 2                     // version
    .word 0
    .dword 0
    .ascii "RISCV\0\0\0"
    .ascii "RSC\x05"
    .word 0

entry:
    la   sp, stack_top
    mv   s0, a1                 // device tree
    la   a0, hello
    call puts
    lwu  a0, 0(s0)              // DTB magic (big-endian d00dfeed read little-endian)
    call puthex
    SBI(0x10, 0)                // spec version >= 2.0 ?
    srli a0, a1, 24
    sltiu a0, a0, 2
    xori a0, a0, 1
    call puthex
    li   s1, 0x10               // probe a few extensions
    la   s2, exts
1:  ld   a0, 0(s2)
    beqz a0, 2f
    SBI(0x10, 3)
    mv   a0, a1
    call puthex
    addi s2, s2, 8
    j    1b
2:  // DBCN console_write
    la   a1, dbcn_msg
    la   a0, dbcn_end
    sub  a0, a0, a1
    li   a2, 0
    SBI(0x4442434e, 0)
    mv   a0, a1
    call puthex
    // timer interrupt through SBI set_timer
    la   t0, strap
    csrw stvec, t0
    li   t0, 1 << 5
    csrs sie, t0
    csrs sstatus, 2
    rdtime a0
    li   t0, 50000
    add  a0, a0, t0
    SBI(0x54494d45, 0)
3:  wfi
    la   t0, ticks
    ld   t0, 0(t0)
    beqz t0, 3b
    la   a0, tick_msg
    call puts
    // HSM status of hart 0: started (0)
    li   a0, 0
    SBI(0x48534d, 2)
    mv   a0, a1
    call puthex
    la   a0, bye
    call puts
    li   a0, 0                  // shutdown
    li   a1, 0
    SBI(0x53525354, 0)
4:  j    4b

    .balign 4
strap:                          // S timer interrupt: disarm and count
    addi sp, sp, -64
    sd   ra, 0(sp)
    sd   a0, 8(sp)
    sd   a6, 16(sp)
    sd   a7, 24(sp)
    sd   t0, 32(sp)
    sd   a1, 40(sp)
    li   a0, -1
    SBI(0x54494d45, 0)
    la   t0, ticks
    li   a0, 1
    sd   a0, 0(t0)
    ld   ra, 0(sp)
    ld   a0, 8(sp)
    ld   a6, 16(sp)
    ld   a7, 24(sp)
    ld   t0, 32(sp)
    ld   a1, 40(sp)
    addi sp, sp, 64
    sret

putc:                           // legacy console_putchar(a0)
    li   a7, 1
    ecall
    ret
puts:
    addi sp, sp, -16
    sd   ra, 0(sp)
    sd   s3, 8(sp)
    mv   s3, a0
1:  lbu  a0, 0(s3)
    beqz a0, 2f
    call putc
    addi s3, s3, 1
    j    1b
2:  ld   ra, 0(sp)
    ld   s3, 8(sp)
    addi sp, sp, 16
    ret
puthex:                         // print a0 as 16 hex digits + newline
    addi sp, sp, -32
    sd   ra, 0(sp)
    sd   s3, 8(sp)
    sd   s4, 16(sp)
    mv   s3, a0
    li   s4, 60
1:  srl  a0, s3, s4
    andi a0, a0, 15
    la   t0, digits
    add  t0, t0, a0
    lbu  a0, 0(t0)
    call putc
    addi s4, s4, -4
    bgez s4, 1b
    li   a0, '\n'
    call putc
    ld   ra, 0(sp)
    ld   s3, 8(sp)
    ld   s4, 16(sp)
    addi sp, sp, 32
    ret

    .section .rodata
digits:   .ascii "0123456789abcdef"
hello:    .asciz "S-mode payload: hello via SBI\n"
tick_msg: .asciz "timer interrupt via SBI set_timer\n"
bye:      .asciz "shutting down via SBI SRST\n"
dbcn_msg: .ascii "DBCN console_write works\n"
dbcn_end:
    .balign 8
exts:     .dword 0x10, 0x54494d45, 0x735049, 0x52464e43, 0x48534d, 0x53525354, 0x4442434e, 0x12345678, 0
    .data
    .balign 8
ticks:    .dword 0
