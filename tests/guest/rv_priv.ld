ENTRY(_start)
SECTIONS {
  . = 0x80000000;
  .text : { *(.text.start) *(.text .text.*) }
  .user : { *(.user) }
  .rodata : { *(.rodata .rodata.* .srodata*) }
  .data : { *(.data .data.* .sdata*) }
  .bss : { *(.bss .bss.* .sbss* COMMON) }
  . = ALIGN(16);
  . += 0x10000;
  stack_top = .;
}
