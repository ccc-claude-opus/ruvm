// Bare-metal start-up for rv_priv.c: M-mode entry, M/S trap frames, user code.
// Runs unchanged on `qemu-system-riscv64 -M virt -bios none` and on ruvm.

    .section .text.start
    .globl _start
_start:
    la   sp, stack_top
    la   t0, m_stack + 2048
    csrw mscratch, t0
    la   t0, m_trap_entry
    csrw mtvec, t0
    call main
    li   t0, 0x100000          // test finisher: pass
    li   t1, 0x5555
    sw   t1, 0(t0)
1:  j    1b

// Save x1..x31 + xepc into a 33-slot frame on the trap stack held in xscratch.
.macro SAVE scratch, epc
    csrrw sp, \scratch, sp
    addi sp, sp, -8*34
    sd   x1, 8*1(sp)
    sd   x3, 8*3(sp)
    sd   x4, 8*4(sp)
    sd   x5, 8*5(sp)
    sd   x6, 8*6(sp)
    sd   x7, 8*7(sp)
    sd   x8, 8*8(sp)
    sd   x9, 8*9(sp)
    sd   x10, 8*10(sp)
    sd   x11, 8*11(sp)
    sd   x12, 8*12(sp)
    sd   x13, 8*13(sp)
    sd   x14, 8*14(sp)
    sd   x15, 8*15(sp)
    sd   x16, 8*16(sp)
    sd   x17, 8*17(sp)
    sd   x18, 8*18(sp)
    sd   x19, 8*19(sp)
    sd   x20, 8*20(sp)
    sd   x21, 8*21(sp)
    sd   x22, 8*22(sp)
    sd   x23, 8*23(sp)
    sd   x24, 8*24(sp)
    sd   x25, 8*25(sp)
    sd   x26, 8*26(sp)
    sd   x27, 8*27(sp)
    sd   x28, 8*28(sp)
    sd   x29, 8*29(sp)
    sd   x30, 8*30(sp)
    sd   x31, 8*31(sp)
    csrr t0, \scratch
    sd   t0, 8*2(sp)           // interrupted sp
    csrr t0, \epc
    sd   t0, 8*32(sp)
    mv   a0, sp
.endm

.macro RESTORE scratch, epc
    ld   t0, 8*32(sp)
    csrw \epc, t0
    ld   t0, 8*2(sp)
    csrw \scratch, t0
    ld   x1, 8*1(sp)
    ld   x3, 8*3(sp)
    ld   x4, 8*4(sp)
    ld   x5, 8*5(sp)
    ld   x6, 8*6(sp)
    ld   x7, 8*7(sp)
    ld   x8, 8*8(sp)
    ld   x9, 8*9(sp)
    ld   x10, 8*10(sp)
    ld   x11, 8*11(sp)
    ld   x12, 8*12(sp)
    ld   x13, 8*13(sp)
    ld   x14, 8*14(sp)
    ld   x15, 8*15(sp)
    ld   x16, 8*16(sp)
    ld   x17, 8*17(sp)
    ld   x18, 8*18(sp)
    ld   x19, 8*19(sp)
    ld   x20, 8*20(sp)
    ld   x21, 8*21(sp)
    ld   x22, 8*22(sp)
    ld   x23, 8*23(sp)
    ld   x24, 8*24(sp)
    ld   x25, 8*25(sp)
    ld   x26, 8*26(sp)
    ld   x27, 8*27(sp)
    ld   x28, 8*28(sp)
    ld   x29, 8*29(sp)
    ld   x30, 8*30(sp)
    ld   x31, 8*31(sp)
    addi sp, sp, 8*34
    csrrw sp, \scratch, sp     // sp = interrupted sp, xscratch = trap stack again
.endm

    .text
    .balign 4
m_trap_entry:
    SAVE mscratch, mepc
    call m_trap
    RESTORE mscratch, mepc
    mret

    .balign 4
    .globl s_trap_entry
s_trap_entry:
    SAVE sscratch, sepc
    call s_trap
    RESTORE sscratch, sepc
    sret

// enter_user(pc, sp): sret to U-mode; returns when the user program calls exit.
    .globl enter_user
enter_user:
    la   t0, kctx
    sd   ra, 0(t0)
    sd   sp, 8(t0)
    sd   s0, 16(t0)
    sd   s1, 24(t0)
    csrw sepc, a0
    li   t1, 1 << 8
    csrc sstatus, t1           // SPP = U
    mv   sp, a1
    sret

    .globl user_exit
user_exit:                      // s_trap points sepc here (with SPP = S)
    la   t0, kctx
    ld   ra, 0(t0)
    ld   sp, 8(t0)
    ld   s0, 16(t0)
    ld   s1, 24(t0)
    ret

// --- user program: placed on its own page, mapped at VA 0x40000000 (U|R|X)
// syscalls (ecall, a7): 0 exit, 1 putchar(a0), 2 print hex(a0)
    .section .user, "ax"
    .balign 4096
    .globl user_code
    .option push
    .option norvc               // faulting instructions are 4 bytes: handler does sepc += 4
user_code:
    li   a7, 1
    li   a0, 'U'
    ecall
    li   a0, '\n'
    ecall
    li   t0, 0x40001000        // user data page
    ld   a0, 0(t0)
    li   a7, 2
    ecall
    li   t1, 0x1234
    sd   t1, 8(t0)
    ld   a0, 8(t0)
    ecall
    li   t0, 0x40005000        // unmapped -> load page fault
    ld   a0, 0(t0)
    li   t0, 0x40000000        // code page is read-only -> store page fault
    sd   zero, 0(t0)
    csrr a0, sstatus           // privileged CSR -> illegal instruction
    li   t0, 0x80000000        // kernel memory, not mapped U -> load page fault... (identity map is S-only)
    lw   a0, 0(t0)
    li   t0, 0x40002ff8        // user stack page: AMO
    li   t1, 5
    sd   t1, 0(t0)
    amoadd.d a0, t1, (t0)
    ld   a0, 0(t0)
    li   a7, 2
    ecall
    li   a7, 0
    ecall
    .option pop
    .balign 4096
    .globl user_data
user_data:
    .dword 0x1122334455667788
    .balign 4096
