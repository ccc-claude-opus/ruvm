// Start-up for FP/SIMD tests: enable FP/SIMD at EL1, set a stack, run main(), PSCI off.
    .section .text.start
    .globl _start
_start:
    mov  x0, #3 << 20
    msr  cpacr_el1, x0         // CPACR_EL1.FPEN = 0b11
    isb
    ldr  x1, =stack_top
    mov  sp, x1
    bl   main
    ldr  x0, =0x84000008       // PSCI SYSTEM_OFF
    hvc  #0
1:  b    1b
