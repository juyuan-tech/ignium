//! M3 T1:用户程序 ELF 加载器(RISC-V ELF64,ET_EXEC)。
//!
//! 承接 M2-DESIGN §7 提纲,按 M3-DESIGN §3 固化:解析 + 校验 + 映射 +
//! 用户栈 + argc/argv + 构建集成。设计要点:
//!
//! - **只读解析** `parse` 全字段校验(§3.2):magic/class/ET_EXEC/EM_RISCV、
//!   `e_phoff + e_phnum*56 ≤ len`、每 PT_LOAD 的 `p_offset+p_filesz ≤ len`、
//!   `p_memsz ≥ p_filesz`(bss)、`p_vaddr < USER_VA_LIMIT`(Sv39 用户区
//!   上限 2^38)、段页内偏移匹配、段间无覆盖(vaddr 排序);
//! - **无任意物理地址**:所有物理页由 `mem::alloc_pages_zeroed` 新分配,
//!   `p_paddr` 完全忽略(微内核不信任可执行文件声明的物理地址);
//! - **逐段逐页映射**(§3.3):`mmu::map_user_page`(拒绝覆盖已有 PTE,
//!   自动置 U 位 + A/D),flags X→RX(0xCB)/ W→RW(0xC7)/ R→R(0xC3);
//!   失败逐页回退(unmap_4k + free_pages);
//! - **用户栈**(§3.4):`USER_STACK_TOP = USER_VA_LIMIT - 64K`(2^38 下方
//!   64K,Sv39 规范地址;审计修正:M3 T1 首用高位用户 VA 时发现原
//!   `0x3FFF_0000_0000`(=2^46)超出 Sv39 可寻址范围,见 mmu.rs
//!   `USER_VA_LIMIT`),8 页(32KB),栈底下方 1 页守护(VA 空洞不分配,
//!   沿用 D20 语义);初始栈写 argc/argv(RISC-V ABI:a0=argc、a1=argv,
//!   sp 16B 对齐)。
//!
//! # 内嵌测试程序
//! `HELLO_ELF` 由 `kernel/build.rs` 编译 `user/` 独立 crate 并拷入
//! OUT_DIR(`include_bytes!`);`tests::boot_elf_test` 加载运行(M3 T1 banner)。
//!
//! # 安全
//! 所有写用户页/拷贝文件字节的裸指针操作均在本模块内显式 `unsafe` 块,
//! 物理地址来源单一(`mem::alloc_pages_zeroed`);不信任 ELF 声明的任何
//! 物理地址。

use alloc::vec::Vec;

/// 用户地址空间上限(Sv39 用户区 2^38;单一来源为 `mmu::USER_VA_LIMIT`,
/// 与 `mmu::map_user_page` 拒绝阈值一致 —— 审计修正见 mmu.rs 常量注释)。
const USER_VA_LIMIT: usize = crate::mmu::USER_VA_LIMIT;
/// 用户栈顶(栈向低地址生长;`USER_VA_LIMIT - 64K` 留顶部余量,顶部 16B
/// 对齐)。低地址侧为栈区。
pub const USER_STACK_TOP: usize = USER_VA_LIMIT - 0x1_0000;
/// 用户栈页数(32KB)。
pub const USER_STACK_PAGES: usize = 8;
const PAGE_SIZE: usize = 4096;
/// argc/argv 参数上限。
const MAX_ARGS: usize = 16;

/// 内嵌的用户测试程序 ELF(kernel/build.rs 编译 `user/` 拷入 OUT_DIR)。
pub(crate) const HELLO_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/hello.elf"));
/// 内嵌的 UART 服务进程 ELF(M3-2;同上,编译 `user/` 拷入 OUT_DIR)。
/// `tests::boot_elf_test` 校验解析;M3-2 T1 经 spawn_user_args 加载运行。
pub(crate) const UART_SERVER_ELF: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/uart_server.elf"));
/// 内嵌的 M3-3 内存服务进程 ELF(同方案 A,编译 `user/` 拷入 OUT_DIR)。
/// 纯服务授权:经 IPC 发放 `Cap::Page` 的唯一入口(M3-DESIGN §11.3)。
pub(crate) const MEMORY_SERVER_ELF: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/memory_server.elf"));
/// 内嵌的 M3-3 内存客户端 ELF:申请→映射→读写→归还往返(经 mem_server)。
pub(crate) const MEM_CLIENT_ELF: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/mem_client.elf"));

/// ELF 加载/校验错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfError {
    /// 文件截断(头/程序头越界)。
    Truncated,
    /// magic 非 `\x7fELF`。
    BadMagic,
    /// 非 64 位 ELF(EI_CLASS≠2)。
    Not64,
    /// 非 ET_EXEC。
    NotExec,
    /// 非 RISC-V(EM_RISCV≠243)。
    NotRiscv,
    /// 段非法(memsz<filesz / 页内偏移失配 / args 超限)。
    BadSegment,
    /// 段间映射覆盖。
    Overlap,
    /// vaddr 超出 Sv39 用户区上限。
    AddressTooHigh,
    /// 物理内存不足。
    NoMemory,
    /// 页映射失败(覆盖已有 PTE / paddr 非法)。
    MapFailed,
    /// 目标进程不存在。
    BadProcess,
}

/// 一个 `PT_LOAD` 段。
#[derive(Debug, Clone, Copy)]
pub struct Segment {
    /// 段起始虚拟地址。
    pub vaddr: usize,
    /// 段在文件中的起始偏移。
    pub offset: usize,
    /// 段文件长度。
    pub filesz: usize,
    /// 段内存长度(≥ filesz,bss 尾部零填充)。
    pub memsz: usize,
    /// ELF 段标志(PF_X=1 / PF_W=2 / PF_R=4)。
    pub flags: u32,
}

/// ELF 解析结果(只读校验,不映射)。
#[derive(Debug, Clone)]
pub struct ElfInfo {
    /// 程序入口虚拟地址。
    pub entry: usize,
    /// 已校验的 PT_LOAD 段(按 vaddr 升序)。
    pub segments: Vec<Segment>,
}

/// 加载结果。
#[derive(Debug, Clone, Copy)]
pub struct LoadedElf {
    /// 程序入口虚拟地址。
    pub entry: usize,
    /// 初始 sp(16B 对齐,指向 argc)。
    pub stack_top: usize,
    /// argc(亦为 a0 帧值)。
    pub argc: usize,
    /// argv 指针数组地址(亦为 a1 帧值)。
    pub argv: usize,
}

#[inline]
const fn align_down(v: usize, a: usize) -> usize {
    v & !(a - 1)
}

#[inline]
const fn align_up(v: usize, a: usize) -> usize {
    (v + a - 1) & !(a - 1)
}

/// 只读校验 ELF 头与各 PT_LOAD 段(M3-DESIGN §3.2;零物理分配)。
///
/// 返回按 vaddr 升序的段列表(段间无覆盖已校验)。
pub fn parse(bytes: &[u8]) -> Result<ElfInfo, ElfError> {
    if bytes.len() < 64 {
        return Err(ElfError::Truncated);
    }
    // e_ident[0..4] = magic。
    if &bytes[0..4] != b"\x7fELF" {
        return Err(ElfError::BadMagic);
    }
    // e_ident[4] = EI_CLASS:2 = ELFCLASS64。
    if bytes[4] != 2 {
        return Err(ElfError::Not64);
    }
    let etype = u16::from_le_bytes([bytes[16], bytes[17]]);
    if etype != 2 {
        // ET_EXEC(1=REL, 2=EXEC, 3=DYN)。
        return Err(ElfError::NotExec);
    }
    let machine = u16::from_le_bytes([bytes[18], bytes[19]]);
    if machine != 243 {
        // EM_RISCV。
        return Err(ElfError::NotRiscv);
    }
    let entry = u64::from_le_bytes([
        bytes[24], bytes[25], bytes[26], bytes[27], bytes[28], bytes[29], bytes[30], bytes[31],
    ]) as usize;
    let phoff = u64::from_le_bytes([
        bytes[32], bytes[33], bytes[34], bytes[35], bytes[36], bytes[37], bytes[38], bytes[39],
    ]) as usize;
    let phnum = u16::from_le_bytes([bytes[56], bytes[57]]) as usize;
    // e_phoff + e_phnum*56 ≤ len(程序头表在文件内)。
    if phnum > 0 {
        let ph_bytes = phnum.checked_mul(56).ok_or(ElfError::Truncated)?;
        let end = phoff.checked_add(ph_bytes).ok_or(ElfError::Truncated)?;
        if end > bytes.len() {
            return Err(ElfError::Truncated);
        }
    } else if phoff > bytes.len() {
        return Err(ElfError::Truncated);
    }
    let mut segments: Vec<Segment> = Vec::new();
    for i in 0..phnum {
        let off = phoff + i * 56;
        let p_type =
            u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]]);
        if p_type != 1 {
            // 仅处理 PT_LOAD(1);PHDR/INTERP/DYNAMIC 等忽略。
            continue;
        }
        let flags = u32::from_le_bytes([
            bytes[off + 4],
            bytes[off + 5],
            bytes[off + 6],
            bytes[off + 7],
        ]);
        let p_offset = u64::from_le_bytes([
            bytes[off + 8],
            bytes[off + 9],
            bytes[off + 10],
            bytes[off + 11],
            bytes[off + 12],
            bytes[off + 13],
            bytes[off + 14],
            bytes[off + 15],
        ]) as usize;
        let p_vaddr = u64::from_le_bytes([
            bytes[off + 16],
            bytes[off + 17],
            bytes[off + 18],
            bytes[off + 19],
            bytes[off + 20],
            bytes[off + 21],
            bytes[off + 22],
            bytes[off + 23],
        ]) as usize;
        let p_filesz = u64::from_le_bytes([
            bytes[off + 32],
            bytes[off + 33],
            bytes[off + 34],
            bytes[off + 35],
            bytes[off + 36],
            bytes[off + 37],
            bytes[off + 38],
            bytes[off + 39],
        ]) as usize;
        let p_memsz = u64::from_le_bytes([
            bytes[off + 40],
            bytes[off + 41],
            bytes[off + 42],
            bytes[off + 43],
            bytes[off + 44],
            bytes[off + 45],
            bytes[off + 46],
            bytes[off + 47],
        ]) as usize;
        // vaddr 必须位于 Sv39 用户区(高位省略 → 映射错位)。
        if p_vaddr >= USER_VA_LIMIT {
            return Err(ElfError::AddressTooHigh);
        }
        // B4(M3 收尾审查):**段上界**校验 —— `p_vaddr + p_memsz` 须落在
        // Sv39 用户区内。此前只校验下界,恶意 ELF 可令 `vaddr+memsz` 逼近
        // usize::MAX:段重叠检查的 `a.vaddr + a.memsz`(未 checked)与
        // map_segment 的 `align_up(x)`(`x+4095`)都会溢出 → overflow-checks
        // 下内核 panic 停机。此处一次性消除两处未检算术的溢出面。
        if p_vaddr
            .checked_add(p_memsz)
            .is_none_or(|end| end > USER_VA_LIMIT)
        {
            return Err(ElfError::AddressTooHigh);
        }
        // bss:内存长度 ≥ 文件长度(尾部零填充)。
        if p_memsz < p_filesz {
            return Err(ElfError::BadSegment);
        }
        // 文件内越界。
        if p_offset
            .checked_add(p_filesz)
            .is_none_or(|e| e > bytes.len())
        {
            return Err(ElfError::Truncated);
        }
        // 段页内偏移匹配:同一 4KB 页内,文件偏移与虚拟地址偏移一致
        // (否则映射时文件字节落点错位)。
        if p_vaddr & 0xfff != p_offset & 0xfff {
            return Err(ElfError::BadSegment);
        }
        segments.push(Segment {
            vaddr: p_vaddr,
            offset: p_offset,
            filesz: p_filesz,
            memsz: p_memsz,
            flags,
        });
    }
    // 段间无覆盖:按 vaddr 排序后逐对检查**页对齐映射范围**(拒绝覆盖语义)。
    segments.sort_by_key(|s| s.vaddr);
    for w in segments.windows(2) {
        let a = &w[0];
        let b = &w[1];
        let a_end = align_up(a.vaddr + a.memsz, PAGE_SIZE);
        let b_start = align_down(b.vaddr, PAGE_SIZE);
        if a_end > b_start {
            return Err(ElfError::Overlap);
        }
    }
    Ok(ElfInfo { entry, segments })
}

/// 加载 ELF 到进程 `pid` 的地址空间(M3-DESIGN §3.3):逐段映射 + 建初始栈。
///
/// `args` 为 argv 字符串列表(argv[0] 惯例为程序名);`p_paddr` 忽略。
/// 成功返回 `(entry, sp, argc, argv)` 供 `sched::spawn_user_args` 建帧。
/// 失败**逐页回退**:已映射页全部 `unmap_4k` + `free_pages` 归还,不残留。
pub fn load(pid: usize, bytes: &[u8], args: &[&[u8]]) -> Result<LoadedElf, ElfError> {
    let info = parse(bytes)?;
    let root = crate::process::pid_root(pid).ok_or(ElfError::BadProcess)?;
    let mut mapped: Vec<(usize, usize)> = Vec::new();
    for seg in &info.segments {
        if let Err(e) = map_segment(root, seg, bytes, &mut mapped) {
            rollback(root, &mapped);
            return Err(e);
        }
    }
    let (stack_top, argv) = match build_user_stack(root, args, &mut mapped) {
        Ok(v) => v,
        Err(e) => {
            rollback(root, &mapped);
            return Err(e);
        }
    };
    Ok(LoadedElf {
        entry: info.entry,
        stack_top,
        argc: args.len(),
        argv,
    })
}

/// 回滚:unmap + 归还全部已映射页(load 失败路径)。
fn rollback(root: usize, mapped: &[(usize, usize)]) {
    for &(va, pa) in mapped {
        let _ = crate::mmu::unmap_4k(root, va);
        let _ = crate::mem::free_pages(pa);
    }
}

/// 逐页映射一个 PT_LOAD 段(含文件字节拷贝 + bss 零填充)。
///
/// 每页:`alloc_pages_zeroed`(bss 尾部天然整页零)→ 拷贝本页与
/// `[seg.offset, seg.offset+filesz)` 的交集 → `map_user_page`(拒绝覆盖)。
/// 任一页失败 → Err(调用方整体回退)。
fn map_segment(
    root: usize,
    seg: &Segment,
    bytes: &[u8],
    mapped: &mut Vec<(usize, usize)>,
) -> Result<(), ElfError> {
    let start = align_down(seg.vaddr, PAGE_SIZE);
    let end = align_up(
        seg.vaddr
            .checked_add(seg.memsz)
            .ok_or(ElfError::AddressTooHigh)?,
        PAGE_SIZE,
    );
    let flags = segment_flags(seg.flags);
    let mut va = start;
    while va < end {
        if crate::mmu::is_mapped(root, va) {
            // 防御:拒绝覆盖已有映射(与 map_user_page 语义一致)。
            return Err(ElfError::Overlap);
        }
        let pa = crate::mem::alloc_pages_zeroed(0).ok_or(ElfError::NoMemory)?;
        // 本页对应文件偏移区间 [base, base+PAGE) ∩ 段文件区间。
        // wrapping:首页 va<seg.vaddr(段内未页对齐)时相对偏移为负。
        let base = seg.offset.wrapping_add(va.wrapping_sub(seg.vaddr));
        let copy_lo = base.max(seg.offset);
        let copy_hi = (base + PAGE_SIZE).min(seg.offset + seg.filesz);
        if copy_lo < copy_hi {
            // copy_hi ≤ seg.offset+seg.filesz ≤ bytes.len()(parse 已校验)。
            for (i, b) in bytes[copy_lo..copy_hi].iter().enumerate() {
                // SAFETY:pa 为本页刚分配的物理页(未释放、映射前写),页内
                // 偏移 copy_lo-base+i ∈ [0, PAGE_SIZE) —— 与用户 VA 对应。
                unsafe {
                    core::ptr::write_volatile((pa + (copy_lo - base) + i) as *mut u8, *b);
                }
            }
        }
        if crate::mmu::map_user_page(root, va, pa, flags).is_err() {
            let _ = crate::mem::free_pages(pa);
            return Err(ElfError::MapFailed);
        }
        mapped.push((va, pa));
        va += PAGE_SIZE;
    }
    Ok(())
}

/// ELF 段标志 → Sv39 叶子 PTE 标志(mmu PTE_LEAF_*;`map_user_page` 自动置 U)。
///
/// 与 M2 内核镜像段权限拆分同语义:代码可执行不可写 / 数据可写不可执行。
fn segment_flags(flags: u32) -> u64 {
    if flags & 1 != 0 {
        0xCB // PF_X → RX(PTE_LEAF_RX)
    } else if flags & 2 != 0 {
        0xC7 // PF_W → RW(PTE_LEAF_RW)
    } else {
        0xC3 // 只读(PTE_LEAF_R)
    }
}

/// 建用户初始栈(M3-DESIGN §3.4):8 页映射 + argc/argv 布局。
///
/// 返回 `(sp_final, argv_array)`:
/// - `sp_final`(16B 对齐)指向 argc;`argv_array` 为 argv 指针数组首地址;
/// - 帧值 a0=argc、a1=argv_array(经 `sched::spawn_user_args` 注入);
/// - 栈布局(psABI 惯例,自高到低):参数字符串 → argv 指针数组(+NULL)→ argc;
/// - 守护页 `[USER_STACK_TOP-36K, USER_STACK_TOP-32K)` 为 VA 空洞(不映射);
/// - 栈 8 页物理页各自独立分配、物理非连续,写按所在页解引用 `stack_pas`
///   (审计修正:曾误用首页 PA 作基址线性偏移,越页即写坏其它进程页)。
fn build_user_stack(
    root: usize,
    args: &[&[u8]],
    mapped: &mut Vec<(usize, usize)>,
) -> Result<(usize, usize), ElfError> {
    let stack_bottom = USER_STACK_TOP - USER_STACK_PAGES * PAGE_SIZE;
    let n = args.len();
    if n > MAX_ARGS {
        return Err(ElfError::BadSegment);
    }
    // 字符串总量须能放下(留 argc/argv 结构余量)。
    let str_total: usize = args.iter().map(|a| a.len() + 1).sum();
    if str_total > USER_STACK_PAGES * PAGE_SIZE - (n + 2) * 8 - 16 {
        return Err(ElfError::BadSegment);
    }
    // 1) 栈页映射(栈底上方为可用区;守护页不映射)。各页独立分配,物理非连续。
    let mut stack_pas = [0usize; USER_STACK_PAGES];
    for (i, pa_slot) in stack_pas.iter_mut().enumerate() {
        let va = stack_bottom + i * PAGE_SIZE;
        if crate::mmu::is_mapped(root, va) {
            return Err(ElfError::Overlap);
        }
        let pa = crate::mem::alloc_pages_zeroed(0).ok_or(ElfError::NoMemory)?;
        crate::mmu::map_user_page(root, va, pa, 0xC7).map_err(|_| ElfError::MapFailed)?;
        mapped.push((va, pa));
        *pa_slot = pa;
    }
    // 栈内 VA → 对应物理地址(按所在页查 stack_pas,而非首页 PA 线性偏移)。
    let pa_at = |uva: usize| -> Result<usize, ElfError> {
        let idx = (uva - stack_bottom) / PAGE_SIZE;
        if idx >= USER_STACK_PAGES {
            return Err(ElfError::BadSegment);
        }
        Ok(stack_pas[idx] + (uva & (PAGE_SIZE - 1)))
    };
    // 2) 参数字符串(自 str_area 起向上写)。
    let str_area = (USER_STACK_TOP - str_total) & !0xF;
    let mut arg_starts = [0usize; MAX_ARGS];
    let mut sptr = str_area;
    for (i, a) in args.iter().enumerate() {
        arg_starts[i] = sptr;
        for (j, b) in a.iter().enumerate() {
            // SAFETY:目标 VA 在栈区内且页已映射(pa_at 按页解析物理地址)。
            unsafe {
                core::ptr::write_volatile(pa_at(sptr + j)? as *mut u8, *b);
            }
        }
        unsafe {
            core::ptr::write_volatile(pa_at(sptr + a.len())? as *mut u8, 0);
        }
        sptr += a.len() + 1;
    }
    // 3) argv 指针数组 + NULL 终结(str_area 下方,16B 对齐)。
    let argv_array = (str_area - (n + 1) * 8) & !0xF;
    // 防御:argv 数组与 argc 须位于栈区内(栈溢边界)。
    if argv_array < stack_bottom + 16 {
        return Err(ElfError::BadSegment);
    }
    for (i, st) in arg_starts.iter().take(n).enumerate() {
        let cell = argv_array + i * 8;
        // SAFETY:cell 位于栈区(>= stack_bottom+16,且已在范围校验内)。
        unsafe {
            core::ptr::write_volatile(pa_at(cell)? as *mut usize, *st);
        }
    }
    unsafe {
        core::ptr::write_volatile(pa_at(argv_array + n * 8)? as *mut usize, 0);
    }
    // 4) argc 于 sp(16B 对齐)。
    let sp_final = (argv_array - 8) & !0xF;
    unsafe {
        core::ptr::write_volatile(pa_at(sp_final)? as *mut usize, n);
    }
    Ok((sp_final, argv_array))
}
