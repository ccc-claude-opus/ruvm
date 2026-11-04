// Exhaustive-ish AArch64 integer test. Uses only Linux write/exit syscalls so the
// same ELF runs under qemu-aarch64 (reference) and ruvm. Output = raw result dump.
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

    .macro REC
    mrs x9, nzcv
    stp x2, x9, [x20], #16
    .endm
    // run one instruction with data-dependent flags, x2 preloaded with x1
    .macro T insn:vararg
    cmp x0, x1
    mov x2, x1
    \insn
    REC
    .endm

    .text
    .globl _start
_start:
    adrp x20, outbuf
    add  x20, x20, :lo12:outbuf
    adrp x21, vals
    add  x21, x21, :lo12:vals
    mov  x22, #0
1:  ldr  x0, [x21, x22, lsl #3]
    mov  x23, #0
2:  ldr  x1, [x21, x23, lsl #3]
    bl   run_tests
    add  x23, x23, #1
    cmp  x23, #NVALS
    b.lo 2b
    add  x22, x22, #1
    cmp  x22, #NVALS
    b.lo 1b
    adrp x1, outbuf
    add  x1, x1, :lo12:outbuf
    sub  x2, x20, x1
    mov  x0, #1
    mov  x8, #64
    svc  #0
    mov  x0, #0
    mov  x8, #93
    svc  #0

run_tests:
    mov  x19, x30
    // ---- add/sub immediate, shifted & extended register
    T add x2, x0, #0xabc
    T add x2, x0, #0x123, lsl #12
    T adds x2, x0, #1
    T subs x2, x0, #1
    T subs w2, w0, #0xff
    T cmn x0, #5
    T add x2, x0, x1
    T adds x2, x0, x1
    T subs x2, x0, x1
    T sub x2, x0, x1, lsl #3
    T add x2, x0, x1, lsr #5
    T adds x2, x0, x1, asr #7
    T subs w2, w0, w1
    T adds w2, w0, w1
    T add w2, w0, w1, lsl #31
    T add x2, x0, w1, uxtw
    T add x2, x0, w1, sxtw #2
    T add x2, x0, w1, uxtb #1
    T adds x2, x0, w1, sxth #3
    T subs x2, x0, x1, uxtx #2
    T neg x2, x0
    T negs x2, x1
    // ---- carry
    T adc x2, x0, x1
    T adcs x2, x0, x1
    T sbc x2, x0, x1
    T sbcs x2, x0, x1
    T adcs w2, w0, w1
    T sbcs w2, w0, w1
    T ngc x2, x0
    // ---- logical register
    T and x2, x0, x1
    T orr x2, x0, x1
    T eor x2, x0, x1
    T ands x2, x0, x1
    T bic x2, x0, x1
    T orn x2, x0, x1
    T eon x2, x0, x1
    T bics x2, x0, x1
    T and x2, x0, x1, lsl #4
    T orr x2, x0, x1, ror #9
    T eor w2, w0, w1, asr #3
    T bic w2, w0, w1, lsr #11
    T orn x2, x0, x1, ror #33
    T mvn x2, x0
    T mvn w2, w0
    T tst x0, x1
    // ---- logical immediate
    T orr x2, x0, #0x5555555555555555
    T and x2, x0, #0xff00ff00ff00ff00
    T eor x2, x0, #1
    T ands x2, x0, #0xf0
    T and w2, w0, #0x7fffff
    T orr w2, w0, #0xc0000003
    T eor w2, w0, #0x00ff00ff
    T and x2, x0, #0x8000000000000000
    T orr x2, x0, #0x3fffffffffffffff
    T tst w0, #0x80000000
    // ---- move wide
    T movz x2, #0x1234, lsl #16
    T movn x2, #0x1234, lsl #32
    T movn w2, #0xabcd
    T mov x2, x0
    T movk x2, #0xabcd, lsl #32
    T movk w2, #0x1234, lsl #16
    // ---- multiply / divide / shift
    T mul x2, x0, x1
    T mul w2, w0, w1
    T madd x2, x0, x1, x0
    T msub x2, x0, x1, x1
    T madd w2, w0, w1, w0
    T smull x2, w0, w1
    T umull x2, w0, w1
    T smaddl x2, w0, w1, x0
    T umsubl x2, w0, w1, x1
    T smulh x2, x0, x1
    T umulh x2, x0, x1
    T udiv x2, x0, x1
    T sdiv x2, x0, x1
    T udiv w2, w0, w1
    T sdiv w2, w0, w1
    T lsl x2, x0, x1
    T lsr x2, x0, x1
    T asr x2, x0, x1
    T ror x2, x0, x1
    T lsl w2, w0, w1
    T lsr w2, w0, w1
    T asr w2, w0, w1
    T ror w2, w0, w1
    // ---- bitfield & extract
    T ubfx x2, x0, #5, #12
    T sbfx x2, x0, #7, #20
    T ubfx w2, w0, #3, #9
    T sbfx w2, w0, #9, #14
    T bfi x2, x0, #8, #16
    T bfi w2, w0, #4, #12
    T bfxil x2, x0, #12, #20
    T bfxil w2, w0, #5, #7
    T ubfiz x2, x0, #9, #17
    T sbfiz x2, x0, #11, #13
    T ubfiz w2, w0, #6, #10
    T sxtb x2, w0
    T sxth x2, w0
    T sxtw x2, w0
    T sxtb w2, w0
    T sxth w2, w0
    T uxtb w2, w0
    T uxth w2, w0
    T lsl x2, x0, #13
    T lsr x2, x0, #7
    T asr x2, x0, #9
    T lsl w2, w0, #5
    T lsr w2, w0, #17
    T asr w2, w0, #31
    T extr x2, x0, x1, #13
    T ror x2, x0, #17
    T extr w2, w0, w1, #9
    T ror w2, w0, #3
    // ---- bit ops
    T clz x2, x0
    T clz w2, w0
    T rbit x2, x0
    T rbit w2, w0
    T rev x2, x0
    T rev w2, w0
    // ---- conditional select / compare
    .irp c, eq,ne,hs,lo,mi,pl,vs,vc,hi,ls,ge,lt,gt,le
    T csel x2, x0, x1, \c
    T csinc x2, x0, x1, \c
    T csinv w2, w0, w1, \c
    T csneg x2, x0, x1, \c
    T cset w2, \c
    T csetm x2, \c
    T ccmp x0, x1, #5, \c
    T ccmn x0, #7, #10, \c
    T ccmp w0, w1, #3, \c
    T ccmn w0, w1, #12, \c
    .endr
    // ---- loads / stores
    adrp x4, scratch
    add  x4, x4, :lo12:scratch
    str  x0, [x4]
    str  x1, [x4, #8]
    T ldr x2, [x4]
    T ldr w2, [x4, #8]
    T ldrb w2, [x4, #1]
    T ldrh w2, [x4, #2]
    T ldrsb x2, [x4, #3]
    T ldrsb w2, [x4, #8]
    T ldrsh x2, [x4, #10]
    T ldrsh w2, [x4, #12]
    T ldrsw x2, [x4, #4]
    T ldur x2, [x4, #1]
    T ldur w2, [x4, #-0]
    T ldursw x2, [x4, #3]
    T ldp x2, x3, [x4]
    T ldp w2, w3, [x4, #4]
    T ldpsw x2, x3, [x4, #8]
    stp  x1, x0, [x4, #16]
    T ldr x2, [x4, #16]
    T ldr x2, [x4, #24]
    strb w0, [x4, #40]
    strh w1, [x4, #42]
    str  w0, [x4, #44]
    stur x1, [x4, #49]
    T ldp x2, x3, [x4, #40]
    T ldr x2, [x4, #49]
    mov  x5, #3
    mov  x6, #-2
    T ldr x2, [x4, x5, lsl #3]
    T ldr w2, [x4, x5, lsl #2]
    T ldrb w2, [x4, x5]
    T ldr x2, [x4, w6, sxtw #3]
    T ldr w2, [x4, w5, uxtw]
    T ldrh w2, [x4, x5, lsl #1]
    // pre/post-index writeback (record the updated base too)
    add  x5, x4, #16
    T ldr x2, [x5, #-8]!
    sub  x7, x5, x4
    str  x7, [x20], #8
    add  x5, x4, #16
    T ldr w2, [x5], #4
    sub  x7, x5, x4
    str  x7, [x20], #8
    add  x5, x4, #32
    T ldp x2, x3, [x5, #-16]!
    sub  x7, x5, x4
    str  x7, [x20], #8
    add  x5, x4, #8
    T ldp w2, w3, [x5], #8
    sub  x7, x5, x4
    str  x7, [x20], #8
    add  x5, x4, #64
    str  x0, [x5, #-8]!
    str  w1, [x5, #-4]!
    strb w0, [x5, #-1]!
    sub  x7, x5, x4
    str  x7, [x20], #8
    ldp  x2, x3, [x4, #48]
    stp  x2, x3, [x20], #16
    add  sp, x4, #112
    stp  x0, x1, [sp, #-16]!
    ldp  x2, x3, [sp], #16
    stp  x2, x3, [x20], #16
    // ---- branches
    mov  x2, #0
    cbz  x0, 1f
    add  x2, x2, #1
1:  cbnz x1, 1f
    add  x2, x2, #2
1:  cbz  w0, 1f
    add  x2, x2, #4
1:  tbz  x0, #63, 1f
    add  x2, x2, #8
1:  tbnz w1, #5, 1f
    add  x2, x2, #16
1:  cmp  x0, x1
    b.eq 1f
    add  x2, x2, #32
1:  b.lt 1f
    add  x2, x2, #64
1:  b.hi 1f
    add  x2, x2, #128
1:  b.mi 1f
    add  x2, x2, #256
1:  b.vs 1f
    add  x2, x2, #512
1:  adr  x5, 1f
    br   x5
    add  x2, x2, #1024
1:  bl   2f
    b    3f
2:  add  x2, x2, #2048
    ret
3:  str  x2, [x20], #8
    // ---- adr/adrp are position dependent: only record relative results
    adr  x2, .
    adr  x3, .+8
    sub  x2, x3, x2
    adrp x3, vals
    add  x3, x3, :lo12:vals
    adr  x5, vals
    sub  x3, x5, x3
    stp  x2, x3, [x20], #16
    mov  x30, x19
    ret
