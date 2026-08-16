OUTPUT_ARCH(riscv)
ENTRY(_start)

BASE_ADDRESS = 0x80200000;
STACK_SIZE = 64K;

SECTIONS
{
    . = BASE_ADDRESS;
    _kernel_start = .;
    _text_start = .;
    .text : {
        *(.text.entry)
        *(.text*)
    }
    . = ALIGN(4K);
    _rodata_start = .;
    .rodata : {
        *(.rodata*)
        *(.srodata*)
    }
    . = ALIGN(4K);
    _data_start = .;
    .data : {
        *(.data*)
        *(.sdata*)
    }
    . = ALIGN(4K);
    _bss_start = .;
    .bss : {
        *(.bss*)
        *(.sbss*)
        *(COMMON)
        . = ALIGN(16);
        _stack_bottom = .;
        . += STACK_SIZE;
        _stack_top = .;
    }
    . = ALIGN(4K);
    _end = .;
    _kernel_end = .;

    /DISCARD/ : {
        *(.eh_frame)
        *(.comment)
    }
}
