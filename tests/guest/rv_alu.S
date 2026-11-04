// RV64IMA differential test (see a64_alu.S). Runs under qemu-riscv64 and ruvm.
    .equ NVALS, 19
    .data
    .balign 8
vals:
    .quad 0,1,2,3,0xff,0x8000,0x7fffffff,0x80000000,0xffffffff,0x123456789abcdef
    .quad 0x7fffffffffffffff,0x8000000000000000,0xfedcba9876543210,0xffffffffffffffff
    .quad 0xffffffff00000000,0x00000000ffffffff,63,64,0xdeadbeefcafebabe
    .bss
    .balign 16
outbuf: .skip 0x800000
scratch: .skip 128

    .macro T insn:vararg
    mv a2, a1
    \insn
    sd a2, 0(s2)
    addi s2, s2, 8
    .endm
    .macro REC3
    sd a3, 0(s2)
    addi s2, s2, 8
    .endm

    .text
    .globl _start
_start:
    la   s2, outbuf
    la   s3, vals
    la   s4, scratch
    li   s6, 0
1:  slli t0, s6, 3
    add  t0, t0, s3
    ld   a0, 0(t0)
    li   s7, 0
2:  slli t0, s7, 3
    add  t0, t0, s3
    ld   a1, 0(t0)
    jal  ra, run_tests
    addi s7, s7, 1
    li   t0, NVALS
    bltu s7, t0, 2b
    addi s6, s6, 1
    li   t0, NVALS
    bltu s6, t0, 1b
    la   a1, outbuf
    sub  a2, s2, a1
    li   a0, 1
    li   a7, 64
    ecall
    li   a0, 0
    li   a7, 93
    ecall

run_tests:
    mv   s5, ra
    // ---- immediates
    T addi a2, a0, -1
    T addi a2, a0, 2047
    T slti a2, a0, -5
    T slti a2, a0, 100
    T sltiu a2, a0, -1
    T sltiu a2, a0, 1
    T xori a2, a0, -1
    T ori a2, a0, 0x555
    T andi a2, a0, -256
    T slli a2, a0, 13
    T srli a2, a0, 63
    T srli a2, a0, 1
    T srai a2, a0, 7
    T srai a2, a0, 63
    T addiw a2, a0, -7
    T addiw a2, a0, 2047
    T slliw a2, a0, 5
    T slliw a2, a0, 31
    T srliw a2, a0, 31
    T srliw a2, a0, 4
    T sraiw a2, a0, 3
    T sraiw a2, a0, 31
    T lui a2, 0x80000
    T lui a2, 0xfffff
    T lui a2, 0x12345
    // ---- reg-reg
    T add a2, a0, a1
    T sub a2, a0, a1
    T sll a2, a0, a1
    T slt a2, a0, a1
    T sltu a2, a0, a1
    T xor a2, a0, a1
    T srl a2, a0, a1
    T sra a2, a0, a1
    T or a2, a0, a1
    T and a2, a0, a1
    T addw a2, a0, a1
    T subw a2, a0, a1
    T sllw a2, a0, a1
    T srlw a2, a0, a1
    T sraw a2, a0, a1
    // ---- M extension
    T mul a2, a0, a1
    T mulh a2, a0, a1
    T mulhsu a2, a0, a1
    T mulhu a2, a0, a1
    T div a2, a0, a1
    T divu a2, a0, a1
    T rem a2, a0, a1
    T remu a2, a0, a1
    T mulw a2, a0, a1
    T divw a2, a0, a1
    T divuw a2, a0, a1
    T remw a2, a0, a1
    T remuw a2, a0, a1
    // ---- loads / stores
    sd   a0, 0(s4)
    sd   a1, 8(s4)
    T lb a2, 1(s4)
    T lb a2, 15(s4)
    T lbu a2, 2(s4)
    T lbu a2, 14(s4)
    T lh a2, 2(s4)
    T lh a2, 10(s4)
    T lhu a2, 4(s4)
    T lhu a2, 12(s4)
    T lw a2, 4(s4)
    T lw a2, 8(s4)
    T lwu a2, 4(s4)
    T lwu a2, 12(s4)
    T ld a2, 0(s4)
    T ld a2, 8(s4)
    sb   a0, 17(s4)
    sh   a1, 18(s4)
    sw   a0, 20(s4)
    sd   a1, 25(s4)
    T ld a2, 16(s4)
    T ld a2, 24(s4)
    T ld a2, 32(s4)
    // ---- atomics
    addi t6, s4, 64
    .irp op, amoswap, amoadd, amoxor, amoand, amoor, amomin, amomax, amominu, amomaxu
    sd   a1, 64(s4)
    mv   a2, a1
    \op\().d a2, a0, (t6)
    sd   a2, 0(s2)
    ld   a3, 64(s4)
    sd   a3, 8(s2)
    addi s2, s2, 16
    sd   a1, 64(s4)
    \op\().w a2, a0, (t6)
    sd   a2, 0(s2)
    ld   a3, 64(s4)
    sd   a3, 8(s2)
    addi s2, s2, 16
    .endr
    sd   a1, 64(s4)
    lr.d a2, (t6)
    sc.d a3, a0, (t6)
    sd   a2, 0(s2)
    sd   a3, 8(s2)
    ld   a3, 64(s4)
    sd   a3, 16(s2)
    sc.d a3, a1, (t6)            # no reservation -> must fail (1)
    sd   a3, 24(s2)
    ld   a3, 64(s4)
    sd   a3, 32(s2)
    lr.w a2, (t6)
    sc.w a3, a0, (t6)
    sd   a2, 40(s2)
    sd   a3, 48(s2)
    ld   a3, 64(s4)
    sd   a3, 56(s2)
    addi s2, s2, 64
    // ---- branches
    li   a2, 0
    beq  a0, a1, 1f
    addi a2, a2, 1
1:  bne  a0, a1, 1f
    addi a2, a2, 2
1:  blt  a0, a1, 1f
    addi a2, a2, 4
1:  bge  a0, a1, 1f
    addi a2, a2, 8
1:  bltu a0, a1, 1f
    addi a2, a2, 16
1:  bgeu a0, a1, 1f
    addi a2, a2, 32
1:  beqz a0, 1f
    addi a2, a2, 64
1:  la   t0, 1f
    jalr ra, 0(t0)
    j    2f
1:  addi a2, a2, 128
    ret
2:  jal  ra, 3f
    j    4f
3:  addi a2, a2, 256
    ret
4:  sd   a2, 0(s2)
    addi s2, s2, 8
    // auipc relative check
1:  auipc a2, 0
    la   t0, 1b
    sub  a2, a2, t0
    sd   a2, 0(s2)
    addi s2, s2, 8
    mv   ra, s5
    ret
