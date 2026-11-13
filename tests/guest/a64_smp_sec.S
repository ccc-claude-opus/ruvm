// Secondary CPU entry (PSCI CPU_ON): x0 = context id = CPU number.
    .text
    .globl secondary_entry
secondary_entry:
    ldr  x1, =stack_top
    lsl  x2, x0, #13           // 8 KiB per CPU
    sub  x1, x1, x2
    mov  sp, x1
    ldr  x3, =vectors
    msr  vbar_el1, x3
    mov  x19, x0
    mov  x0, x19
    bl   secondary_main
1:  wfi
    b    1b
