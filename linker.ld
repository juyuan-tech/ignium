OUTPUT_ARCH(riscv)
ENTRY(_start)

BASE_ADDRESS = 0x80200000;

SECTIONS
{
    . = BASE_ADDRESS;
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
        . += 64K;
        _stack_top = .;
    }
    . = ALIGN(4K);
    _end = .;
}
