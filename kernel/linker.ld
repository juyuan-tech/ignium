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
TRAP_STACK_SIZE = 16K;          /* 每 hart 槽内的栈大小(陷阱/异常处理器专用) */
TRAP_STRIDE = 32K;              /* per-hart 槽跨度 = 16K 守护 + 16K 栈;
                                   2 的幂 → 按 hartid 移位可寻址 */
MAX_HARTS = 4;                  /* 与 arch/riscv64.rs 的 MAX_HARTS、QEMU -smp 4 一致;
                                   超限 hart 停 park(不进入本数组) */

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

    /* 可写数据;锚点 __global_pointer$(L1 诚实说明):gp 相对寻址
     * ±2KB 只能覆盖 .data/.sdata 尾部附近的小数据;.sbss/.bss 在
     * 2KB 之外不可达。当前 Rust 裸机目标不生成 gp 相对访问(静态
     * 落 .data/.bss 而非小数据段),锚点仅为未来优化预留,勿期待
     * 它覆盖 .sbss。 */
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

    /* 调试段(.debug 与 .riscv.attributes):SHF_NONALLOC,不显式放置
     * —— 显式放置会推进位置计数器,把 .stack 推离 PC 相对范围
     * (dev 构建调试段巨大,实测 -524796 越界);lld 对非分配孤儿段
     * 自动置于末尾,不参与地址布局、不进加载段,对 _alloc_start
     * 无影响。 */

    /* 引导栈:NOLOAD(不占镜像文件),位于 _kernel_end 之后。
     * entry.S 的清零循环以 _stack_top 为界,栈区因此也被清零,
     * 这是 panic 栈水位检测成立的前提。
     * 栈向下增长;下方紧邻 _kernel_end(内核数据)——
     * M1 引入 MMU 后须在 _kernel_end 与栈之间加不可映射守护页。
     * 溢出防护:栈水位(p后诊断)+ 守护页(事前防护,4KB 页级)。 */
    .stack (NOLOAD) : ALIGN(4K) {
        _stack_guard = .;
        . += 4K;                /* 守护页:MMU 不映射,越界→页故障 */
        _stack_bottom = .;
        . += STACK_SIZE;
        _stack_top = .;
    }

    /* 陷阱栈:per-hart 数组(D7 多核)。每槽 stride 32K = 16K 守护 + 16K 栈,
     * 数组基址 32K 对齐(槽偏移 = hartid*32K,移位可寻址)。
     * sscratch 恒在本 hart 槽内:陷阱外 = 槽顶 T_h,陷阱期间 = 帧基址;
     * hartid = (sscratch - _trap_stack_base) >> 15,不依赖 tp。
     * 与引导栈分离:处理器绝不在被中断者的 sp 上运行。 */
    .trap_stack (NOLOAD) : ALIGN(32K) {
        _trap_stack_base = .;
        _trap_stack_bottom = .;  /* 兼容符号 = 数组起点(首槽守护页下缘) */
        _trap_stack_guard = .;   /* 兼容符号 = 数组起点(首槽守护页) */
        . += TRAP_STRIDE * MAX_HARTS;
        _trap_stack_top = .;
    }

    /* per-hart idle 栈数组(D7/D8):副核 idle 上下文用栈,每槽同 stride。
     * hart0 用 _stack(引导栈),不占本数组。守护页在 mmu 映射中 unmap。 */
    .idle_stack (NOLOAD) : ALIGN(32K) {
        _idle_stack_base = .;
        _idle_stack_guard = .;
        . += TRAP_STRIDE * MAX_HARTS;
        _idle_stack_top = .;
    }

    /* M1 物理分配器的起点:栈区/陷阱栈区属于保留区,
     * 分配器必须从 _alloc_start 之后取页(pro 审计 #4)。
     * 红线:从此处到文件末尾**不得**再出现任何 SHF_ALLOC 段
     * (任何未来新段都必须在 _alloc_start 之前定义,或 /DISCARD/)。
     * 非分配孤儿段(调试/属性)由 lld 置于末尾,天然不触碰本区。 */
    . = ALIGN(4K);
    _alloc_start = .;

    /* 孤儿段策略:显式丢弃工具链噪音段 */
    /DISCARD/ : {
        *(.eh_frame)
        *(.comment)
    }
}
