/* Ignium 内核链接脚本(QEMU virt 目标:加载基址 0x80200000,S 模式)。
 *
 * 符号契约(被源码引用的符号,改名须同步修改引用处):
 *   _kernel_start / _kernel_end —— 内核镜像边界(M1 物理分配器用)
 *   _text_start / _rodata_start / _data_start / _bss_start —— 分区边界
 *   _stack_bottom / _stack_top —— 栈区边界(entry.S 清栈、panic 水位用)
 *   __global_pointer$ —— gp 相对寻址锚点(未来编译优化用)
 */

OUTPUT_ARCH(riscv)
ENTRY(_start)

BASE_ADDRESS = 0x80200000;      /* QEMU virt 上 OpenSBI 跳转内核的地址 */
STACK_SIZE = 64K;               /* 引导栈大小;M1 引入守护页后缩小 */

SECTIONS
{
    /* ============ 内核镜像 [ _kernel_start, _kernel_end ) ============ */
    . = BASE_ADDRESS;
    _kernel_start = .;

    /* 代码段:entry.S 的 .text.entry 必须排最前,保证入口在镜像开头 */
    . = ALIGN(4K);
    _text_start = .;
    .text : {
        *(.text.entry)
        *(.text*)
    }

    /* 只读数据(常量、字符串、跳转表) */
    . = ALIGN(4K);
    _rodata_start = .;
    .rodata : {
        *(.rodata*)
        *(.srodata*)
    }

    /* 可写数据;锚点 __global_pointer$ 放在 .data 前部(gp 相对寻址范围
     * 为 ±2KB,位置偏前可覆盖更多静态数据) */
    . = ALIGN(4K);
    _data_start = .;
    .data : {
        *(.data*)
        *(.sdata*)
        . = ALIGN(16);
        __global_pointer$ = .;
    }

    /* BSS(清零区)。注意:栈不在这里 —— 栈若归入 BSS,会被 _end 包含,
     * M1 物理分配器会把栈误当作可用内存。 */
    . = ALIGN(4K);
    _bss_start = .;
    .bss : {
        *(.bss*)
        *(.sbss*)
        *(COMMON)
    }
    . = ALIGN(4K);
    _end = .;
    _kernel_end = .;

    /* ============ 镜像之外的独立区 ============ */

    /* 引导栈:NOLOAD(不占镜像文件),位于 _kernel_end 之后。
     * entry.S 的清零循环以 _stack_top 为界,栈区因此也被清零,
     * 这是 panic 栈水位检测成立的前提。 */
    .stack (NOLOAD) : {
        _stack_bottom = .;
        . += STACK_SIZE;
        _stack_top = .;
    }

    /* 孤儿段策略:显式丢弃工具链噪音段;调试/属性段显式收纳
     * (否则 --orphan-handling=error 会打断 debug 构建) */
    .debug : {
        *(.debug*)
    }
    .riscv.attributes : {
        *(.riscv.attributes)
    }
    /DISCARD/ : {
        *(.eh_frame)
        *(.comment)
    }
}
