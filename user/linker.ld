/* Ignium 用户测试程序链接脚本(M3 T1)。
 *
 * 加载基址 0x4000_0000 为内核 ELF 加载器约定的用户区低地址
 * (Sv39 用户区 [0x4000_0000, 0x4000_0000_00),上限 2^38)。**必须**与
 * kernel/src/tests.rs boot_elf_test 的 marker VA(0x4000_2000)无重叠:
 * 本程序 < 4KB(段分页对齐后映射到前 2 页),marker 页在 0x4000_2000。
 */

OUTPUT_ARCH(riscv)
ENTRY(_start)

/* 注意:linker 脚本不接受 0x4000_0000 这类下划线十六进制(lld 报
 * "malformed number"),须写全 0x40000000。 */
BASE = 0x40000000;

SECTIONS
{
    . = BASE;
    .text : {
        *(.text._start)
        *(.text*)
    }
    /* 各段页对齐:映射边界干净,加载器逐段映射不重叠。 */
    . = ALIGN(4096);
    .rodata : { *(.rodata*) *(.srodata*) }
    . = ALIGN(4096);
    .data : { *(.data*) *(.sdata*) }
    . = ALIGN(4096);
    .bss : {
        *(.bss*) *(.sbss*)
        *(COMMON)
    }
    /DISCARD/ : { *(.eh_frame) *(.comment) }
}
